//! A server on a free port of this machine, with two workspaces and a key
//! for each, for the API's end-to-end tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use passalong_server_api::client::{Client, Response};
use passalong_server_api::{Options, Server};
use passalong_server_core::clock::ManualClock;
use passalong_server_core::config::{self, Config};
use passalong_server_core::control::Control;
use passalong_server_core::random::OsRandom;
use passalong_server_core::workspace::Role;

pub const DAY: u64 = 86_400;

pub struct TestServer {
    pub addr: SocketAddr,
    pub clock: ManualClock,
    pub config: Config,
    /// A read-write key for workspace `home`.
    pub token: String,
    /// A read-write key for workspace `other`.
    pub other_token: String,
    pub dir: tempfile::TempDir,
    /// What the bridge held at most, for the test of its bound.
    pub bridge: Arc<passalong_server_api::BridgeStats>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    done: Option<tokio::task::JoinHandle<()>>,
}

pub fn config_in(dir: &std::path::Path, extra: &str) -> Config {
    let text = format!(
        "[server]\ndata_dir = {:?}\n[listen]\naddress = \"127.0.0.1:0\"\nmode = \"plain\"\n{extra}",
        dir.join("data").display().to_string()
    );
    config::parse(&text, &PathBuf::from("/test.toml"), &config::Process).unwrap()
}

impl TestServer {
    pub async fn start() -> Self {
        Self::start_with("", |_| {}).await
    }

    pub async fn start_with(extra: &str, tune: impl FnOnce(&mut Options)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path(), extra);
        std::fs::create_dir_all(config.server.data_dir.join("workspaces")).unwrap();
        let clock = ManualClock::at(1_800_000_000);
        let control = Self::control_of(&config, &clock);
        control.create_workspace("home", None).unwrap();
        control.create_workspace("other", None).unwrap();
        let token = control
            .create_key("home", "laptop", Role::ReadWrite, Some(90 * DAY))
            .unwrap()
            .0
            .reveal();
        let other_token = control
            .create_key("other", "phone", Role::ReadWrite, None)
            .unwrap()
            .0
            .reveal();

        let mut options = Options {
            clock: Arc::new(clock.clone()),
            ..Options::default()
        };
        let bridge = options.bridge.clone();
        tune(&mut options);
        let server = Server::bind(config.clone(), options).await.unwrap();
        let addr = server.local_addr();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let done = tokio::spawn(async move {
            server
                .run(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        Self {
            addr,
            bridge,
            clock,
            config,
            token,
            other_token,
            dir,
            stop: Some(stop),
            done: Some(done),
        }
    }

    fn control_of(config: &Config, clock: &ManualClock) -> Control {
        Control::open(
            config.control_database(),
            Duration::from_secs(5),
            Arc::new(clock.clone()),
            Box::new(OsRandom),
        )
        .unwrap()
    }

    /// The control database as the operator's CLI has it: another connection.
    pub fn control(&self) -> Control {
        Self::control_of(&self.config, &self.clock)
    }

    pub fn client(&self) -> Client {
        Client::plain(self.addr)
    }

    /// A request as `home`'s key, with a JSON body or none.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Response {
        self.call_as(&self.token, method, path, body).await
    }

    pub async fn call_as(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Response {
        let mut request = self.client().request(method, path).bearer(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        request.send().await.unwrap()
    }

    pub async fn shutdown(mut self) {
        drop(self.stop.take());
        self.done.take().unwrap().await.unwrap();
    }
}

use sha2::{Digest, Sha256};

/// A plaintext item as the client makes it: its id, its `meta`, its size.
pub struct Honest {
    pub id: String,
    pub meta: serde_json::Value,
    pub content: Vec<u8>,
}

pub fn honest(ts: u32, content: &[u8]) -> Honest {
    let sha: String = Sha256::digest(content)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Honest {
        id: format!("{ts:08x}-{}", &sha[..12]),
        meta: serde_json::json!({ "schema": 1, "kind": "text", "sha256": sha, "size": content.len() }),
        content: content.to_vec(),
    }
}

impl TestServer {
    /// `beginUpload` for a plaintext item.
    pub async fn begin(&self, item: &Honest) -> Response {
        let body = serde_json::json!({ "id": item.id, "meta": item.meta, "size": item.content.len().to_string(), "expectedKeyId": null });
        self.call("POST", "/v1/uploads", Some(body)).await
    }

    pub async fn put_content(&self, token: &str, upload: &str, content: &[u8]) -> Response {
        self.client()
            .request("PUT", &format!("/v1/uploads/{upload}/content"))
            .bearer(token)
            .body(content.to_vec())
            .send()
            .await
            .unwrap()
    }

    /// A whole upload; the answer of the commit.
    pub async fn upload(&self, item: &Honest) -> Response {
        let begun = self.begin(item).await;
        assert_eq!(
            begun.status,
            201,
            "{}",
            String::from_utf8_lossy(&begun.body)
        );
        let upload = begun.json()["uploadId"].as_str().unwrap().to_owned();
        assert_eq!(
            self.put_content(&self.token, &upload, &item.content)
                .await
                .status,
            204
        );
        self.call("POST", &format!("/v1/uploads/{upload}/commit"), None)
            .await
    }

    pub async fn get(&self, path: &str) -> Response {
        self.call("GET", path, None).await
    }
}
