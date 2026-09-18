//! `serve`, and `check --health`, which asks a running `serve`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use passalong_server_api::client::Client;
use passalong_server_api::{Options, Server};
use passalong_server_core::config::{Config, ListenMode};

use crate::commands::Done;

/// Until SIGTERM, which is how systemd and Docker ask, or Ctrl-C.
async fn told_to_stop() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(err) => {
            tracing::warn!(target: "passalong_server::http", %err, "cannot listen for SIGTERM; Ctrl-C still stops the server");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start: {err}"))
}

pub fn serve(config: Config) -> Done {
    runtime()?.block_on(async {
        let server = Server::bind(config, Options::default())
            .await
            .map_err(|err| err.to_string())?;
        server
            .run(told_to_stop())
            .await
            .map_err(|err| format!("the listener failed: {err}"))?;
        tracing::info!(target: "passalong_server::http", "stopped");
        Ok(String::new())
    })
}

/// Where a process on this host reaches a server that listens on `listen`.
fn local(listen: SocketAddr) -> SocketAddr {
    let ip = match listen.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, listen.port())
}

/// How long the probe waits. Docker's own timeout, in the `Dockerfile`, is
/// longer, so that this is the one that speaks.
const PROBE: Duration = Duration::from_secs(4);

/// `check --health`: `/readyz` of the configured address. In `tls` mode the
/// probe connects by the pin of the configured certificate, like any client:
/// there is no way to skip the check, and none is needed.
pub fn health(config: &Config) -> Done {
    let addr = local(config.listen.address);
    let client = match config.listen.mode {
        ListenMode::Plain => Client::plain(addr),
        ListenMode::Tls => {
            let file = &config
                .tls
                .as_ref()
                .ok_or("not healthy: listen.mode is \"tls\" and there is no [tls] section")?
                .cert_file;
            let pem = std::fs::read(file)
                .map_err(|err| format!("not healthy: {}: {err}", file.display()))?;
            let pin = passalong_server_api::tls::pin_of_pem(&pem)
                .map_err(|err| format!("not healthy: {}: {err}", file.display()))?;
            Client::pinned(addr, &pin).map_err(|err| format!("not healthy: {err}"))?
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start: {err}"))?;
    let answer = runtime.block_on(async {
        tokio::time::timeout(PROBE, client.request("GET", "/readyz").send()).await
    });
    match answer {
        Ok(Ok(answer)) if answer.status == 204 => Ok(format!("ready: {addr}\n")),
        Ok(Ok(answer)) => Err(format!("not ready: {addr} answered {}", answer.status)),
        Ok(Err(err)) => Err(format!("not healthy: {addr}: {err}")),
        Err(_) => Err(format!("not healthy: {addr} did not answer in time")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_on_every_address_is_asked_on_this_host() {
        for (listen, asked) in [
            ("0.0.0.0:8443", "127.0.0.1:8443"),
            ("[::]:8443", "[::1]:8443"),
            ("192.0.2.4:9000", "192.0.2.4:9000"),
            ("127.0.0.1:1", "127.0.0.1:1"),
        ] {
            assert_eq!(local(listen.parse().unwrap()), asked.parse().unwrap());
        }
    }
}
