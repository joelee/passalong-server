//! Listening, serving, and stopping.

use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::connect_info::Connected;
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::serve::{IncomingStream, Listener};
use passalong_server_core::clock::{Clock, SystemClock};
use passalong_server_core::config::{Config, ListenMode};
use passalong_server_core::control::Control;
use passalong_server_core::engines::Engines;
use passalong_server_core::error::ApiError;
use passalong_server_core::random::{OsRandom, RandomSource};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::handlers::{health, items, rewrite, uploads, viewer};
use crate::problem::{self, Problem};
use crate::routes::operation_of;
use crate::state::{AppState, BUSY, Inner};
use crate::tls::Reloading;

const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// What tests, and later `serve`, may tune.
#[derive(Clone)]
pub struct Options {
    /// The clock keys expire by and rate limits count by.
    pub clock: Arc<dyn Clock>,
    /// A route that panics, for the test that a panic is contained.
    pub panic_route: bool,
    /// What the bridge holds and held at most, for whoever wants to look.
    pub bridge: Arc<crate::bridge::BridgeStats>,
    /// How often the certificate files are looked at (PLAN-00004 D-06).
    pub tls_reload_every: Duration,
    /// How often the janitor passes through every workspace.
    pub janitor_every: Duration,
    /// How long requests in flight may take to finish once told to stop.
    pub drain: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            panic_route: false,
            bridge: Arc::default(),
            tls_reload_every: Duration::from_secs(30),
            janitor_every: Duration::from_secs(600),
            drain: Duration::from_secs(30),
        }
    }
}

/// Why the server did not start.
#[derive(Debug)]
pub struct StartError(pub String);

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StartError {}

/// The address a connection came from.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

impl Connected<IncomingStream<'_, Incoming>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'_, Incoming>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// A connection, in either mode.
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for Conn {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// What the server accepts connections from.
pub struct Incoming {
    tcp: TcpListener,
    /// `None` in plain mode.
    tls: Option<Arc<Reloading>>,
    /// Handshakes under way. Each is a task of its own, so that a client
    /// that says nothing holds up nobody but itself.
    handshakes: JoinSet<Option<(Conn, SocketAddr)>>,
}

/// How long a client may take to finish its handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// How many handshakes may be under way. Beyond it nothing is accepted
/// until one ends, which the timeout sees to.
const HANDSHAKES: usize = 1024;

async fn handshake(
    config: Arc<rustls::ServerConfig>,
    stream: TcpStream,
    peer: SocketAddr,
) -> Option<(Conn, SocketAddr)> {
    let accepting = tokio_rustls::TlsAcceptor::from(config).accept(stream);
    match tokio::time::timeout(HANDSHAKE, accepting).await {
        Ok(Ok(stream)) => Some((Conn::Tls(Box::new(stream)), peer)),
        // Port scanners and plain HTTP sent to a TLS port: not worth a line
        // at the usual level.
        Ok(Err(err)) => {
            tracing::debug!(target: "passalong_server::tls", address = %peer.ip(), %err, "a handshake failed");
            None
        }
        Err(_) => {
            tracing::debug!(target: "passalong_server::tls", address = %peer.ip(), "a handshake took too long");
            None
        }
    }
}

impl Listener for Incoming {
    type Io = Conn;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Conn, SocketAddr) {
        loop {
            let room = self.handshakes.len() < HANDSHAKES;
            tokio::select! {
                accepted = self.tcp.accept(), if room => match (accepted, &self.tls) {
                    (Ok((stream, peer)), None) => return (Conn::Plain(stream), peer),
                    (Ok((stream, peer)), Some(tls)) => {
                        self.handshakes.spawn(handshake(tls.current(), stream, peer));
                    }
                    (Err(err), _) => {
                        // Out of file descriptors, most likely: wait, do not spin.
                        tracing::warn!(target: "passalong_server::http", %err, "cannot accept a connection");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                },
                Some(done) = self.handshakes.join_next() => {
                    if let Ok(Some(connection)) = done {
                        return connection;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

/// Where the authentication layer notes the key, for the request's log line.
#[derive(Clone, Default)]
pub struct KeySlot(Arc<Mutex<Option<String>>>);

impl KeySlot {
    pub fn set(&self, key: &str) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(key.to_owned());
    }
    fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// An id a client sent is kept when it could not hurt a log: up to 64
/// letters, digits, dots, dashes, and underscores.
fn plausible(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn new_request_id() -> String {
    let mut bytes = [0_u8; 16];
    OsRandom.fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The outermost layer: gives the request its id, contains a panic, and
/// writes the request's one log line. Never the `Authorization` header.
async fn outer(mut request: Request, next: Next) -> Response {
    let started = Instant::now();
    let id = request
        .headers()
        .get(&REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .filter(|id| plausible(id))
        .map_or_else(new_request_id, str::to_owned);
    let method = request.method().as_str().to_owned();
    let matched = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned());
    let slot = KeySlot::default();
    request.extensions_mut().insert(slot.clone());

    // A handler that panics takes its task down, not the server, and the
    // client gets an answer instead of a closed connection.
    let mut response = match tokio::spawn(next.run(request)).await {
        Ok(response) => response,
        Err(err) => {
            tracing::error!(target: "passalong_server::http", request = %id, %err, "a handler panicked");
            problem::internal()
        }
    };
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID, value);
    }
    let operation = matched
        .as_deref()
        .and_then(|path| operation_of(&method, path))
        .unwrap_or("-");
    tracing::info!(
        target: "passalong_server::http",
        request = %id,
        key = %slot.get().as_deref().unwrap_or("-"),
        operation,
        method = %method,
        status = response.status().as_u16(),
        ms = started.elapsed().as_millis() as u64,
        "request"
    );
    response
}

async fn not_found() -> Problem {
    Problem::from(ApiError::NotFound)
}

async fn test_panic() -> Response {
    panic!("boom: a handler panicked, as the test asked");
}

fn router(state: AppState, options: &Options) -> Router {
    let mut v1 = Router::new()
        .route("/v1/viewer", get(viewer::get_viewer))
        .route("/v1/workspace", get(viewer::get_workspace))
        .route("/v1/workspace/probe", post(viewer::probe_write))
        .route("/v1/workspace/clean-staging", post(viewer::clean_staging))
        .route("/v1/items", get(items::list_items))
        .route("/v1/item-ids", get(items::list_item_ids))
        .route("/v1/items/resolve", get(items::resolve_item))
        .route(
            "/v1/content-keys/{contentKey}",
            get(items::find_by_content_key),
        )
        .route(
            "/v1/items/{id}",
            get(items::get_item).delete(items::delete_item),
        )
        .route("/v1/items/{id}/content", get(items::get_item_content))
        .route("/v1/uploads", post(uploads::begin_upload))
        .route(
            "/v1/uploads/{uploadId}/content",
            put(uploads::put_upload_content),
        )
        .route(
            "/v1/uploads/{uploadId}/commit",
            post(uploads::commit_upload),
        )
        .route("/v1/uploads/{uploadId}", delete(uploads::abort_upload))
        .route("/v1/workspace/encryption", put(rewrite::enable_encryption))
        .route(
            "/v1/workspace/encryption/fresh-start",
            post(rewrite::fresh_start),
        )
        .route(
            "/v1/workspace/encryption/header",
            put(rewrite::replace_header),
        )
        .route(
            "/v1/rewrite",
            get(rewrite::get_rewrite).post(rewrite::begin_rewrite),
        )
        .route("/v1/rewrite/heartbeat", post(rewrite::heartbeat_rewrite))
        .route("/v1/rewrite/take-over", post(rewrite::take_over_rewrite))
        .route("/v1/rewrite/commit", post(rewrite::commit_rewrite))
        .route("/v1/rewrite/abort", post(rewrite::abort_rewrite));
    if options.panic_route {
        v1 = v1.route("/v1/test-panic", get(test_panic));
    }
    // The key is checked for unknown paths too: what exists is not said to
    // those without one.
    let v1 = v1.fallback(not_found).layer(middleware::from_fn_with_state(
        state.clone(),
        crate::auth::layer,
    ));
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .merge(v1)
        .layer(middleware::from_fn(outer))
        .with_state(state)
}

/// A server that is bound and not yet serving.
pub struct Server {
    incoming: Incoming,
    router: Router,
    addr: SocketAddr,
    /// `tls` or `plain`, for the line that says what it listens on.
    mode: &'static str,
    state: AppState,
    janitor_every: Duration,
    drain: Duration,
}

/// The janitor: every so often, every workspace forgets the uploads nobody
/// finished and the outcomes nobody asks for any more. A workspace that
/// fails is logged and the next one tried; the janitor never ends.
async fn janitor(state: AppState, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let swept = state
            .blocking(|inner| {
                let workspaces = inner
                    .control
                    .workspaces()
                    .map_err(|_| ApiError::ServiceUnavailable)?;
                for workspace in workspaces {
                    let cleaned = inner
                        .engines
                        .get(&workspace.id)
                        .and_then(|engine| engine.clean_staging());
                    if let Err(err) = cleaned {
                        tracing::warn!(target: "passalong_server::janitor", workspace = %workspace.id.as_str(), %err, "could not clean a workspace's staging");
                    }
                }
                Ok(())
            })
            .await;
        if let Err(err) = swept {
            tracing::warn!(target: "passalong_server::janitor", %err, "could not list the workspaces");
        }
    }
}

/// The pair of `tls` mode, or why the server will not start (PLAN-00004
/// D-07): it never falls back to plain HTTP.
fn open_pair(config: &Config) -> Result<Reloading, StartError> {
    let how = "make a self-signed pair with `passalong-server tls self-signed --host <name>`, or set listen.mode = \"plain\" behind a TLS-terminating proxy";
    let Some(files) = &config.tls else {
        return Err(StartError(format!(
            "listen.mode is \"tls\" and there is no [tls] section with cert_file and key_file: {how}"
        )));
    };
    Reloading::open(&files.cert_file, &files.key_file).map_err(|err| {
        StartError(format!(
            "listen.mode is \"tls\" and the pair {} and {} cannot be used: {err}. To {how}",
            files.cert_file.display(),
            files.key_file.display()
        ))
    })
}

impl Server {
    /// Opens the control database and binds `listen.address`.
    ///
    /// # Errors
    ///
    /// When the database cannot be opened or the address cannot be bound.
    pub async fn bind(config: Config, options: Options) -> Result<Self, StartError> {
        let control = Control::open(
            config.control_database(),
            BUSY,
            options.clock.clone(),
            Box::new(OsRandom),
        )
        .map_err(|err| StartError(format!("{}: {err}", config.control_database().display())))?;
        let tls = match config.listen.mode {
            ListenMode::Plain => None,
            ListenMode::Tls => Some(Arc::new(open_pair(&config)?)),
        };
        if let Some(tls) = &tls {
            // Ends with the server: it holds the pair weakly.
            let (pair, every) = (Arc::downgrade(tls), options.tls_reload_every);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(pair) = pair.upgrade() else { return };
                    let _ = tokio::task::spawn_blocking(move || pair.look()).await;
                }
            });
        }
        let tcp = TcpListener::bind(config.listen.address)
            .await
            .map_err(|err| {
                StartError(format!("cannot listen on {}: {err}", config.listen.address))
            })?;
        let addr = tcp
            .local_addr()
            .map_err(|err| StartError(err.to_string()))?;
        let mode_name = match config.listen.mode {
            ListenMode::Plain => "plain",
            ListenMode::Tls => "tls",
        };
        let state = AppState(Arc::new(Inner {
            engines: Engines::new(config.clone(), BUSY, options.clock.clone()),
            control,
            clock: options.clock.clone(),
            bridge: options.bridge.clone(),
            failures: crate::rate::FailureLimiter::new(
                config.limits.auth_failures_per_minute,
                crate::rate::CAPACITY,
            ),
            config,
        }));
        Ok(Self {
            incoming: Incoming {
                tcp,
                tls,
                handshakes: JoinSet::new(),
            },
            mode: mode_name,
            router: router(state.clone(), &options),
            state,
            janitor_every: options.janitor_every,
            drain: options.drain,
            addr,
        })
    }

    /// The address it listens on: the port, when the configuration said 0.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serves until `shutdown` completes, then stops accepting and lets the
    /// requests in flight finish.
    ///
    /// # Errors
    ///
    /// The listener's error.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        tracing::info!(target: "passalong_server::http", address = %self.addr, mode = self.mode, "listening");
        let janitor = tokio::spawn(janitor(self.state.clone(), self.janitor_every));
        let (told, stopping) = tokio::sync::oneshot::channel::<()>();
        let mut serving = Box::pin(
            axum::serve(
                self.incoming,
                self.router
                    .into_make_service_with_connect_info::<PeerAddr>(),
            )
            .with_graceful_shutdown(async move {
                shutdown.await;
                let _ = told.send(());
            })
            .into_future(),
        );
        let ended = tokio::select! {
            ended = &mut serving => Some(ended),
            _ = stopping => None,
        };
        // Told to stop: nothing new is accepted, and what is in flight gets
        // its time, which is bounded, or a client that never finishes would
        // keep the process for ever.
        let ended = match ended {
            Some(ended) => ended,
            None => {
                tracing::info!(target: "passalong_server::http", "stopping: requests in flight may finish");
                match tokio::time::timeout(self.drain, serving).await {
                    Ok(ended) => ended,
                    Err(_) => {
                        tracing::warn!(target: "passalong_server::http", "stopped with requests still in flight");
                        Ok(())
                    }
                }
            }
        };
        janitor.abort();
        ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_id_from_a_client_is_kept_only_when_it_could_not_hurt_a_log() {
        for ok in ["a", "client-7f3a.41", "A_b-c.9", &"x".repeat(64)] {
            assert!(plausible(ok), "{ok}");
        }
        for bad in [
            "",
            "has space",
            "new\nline",
            "quote\"",
            &"x".repeat(65),
            "ü",
        ] {
            assert!(!plausible(bad), "{bad:?}");
        }
        let made = new_request_id();
        assert!(plausible(&made) && made.len() == 32 && made != new_request_id());
    }
}
