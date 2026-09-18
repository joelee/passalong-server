//! TLS: a pair made by `tls self-signed` is served, the client connects by
//! its pin and by nothing else, and a renewed pair is picked up without a
//! restart (PLAN-00004, step 8; D-06, D-07).

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use passalong_server_api::client::Client;
use passalong_server_api::{Options, Server, tls};
use passalong_server_core::clock::ManualClock;
use passalong_server_core::config::{self, Config};
use tokio::io::AsyncWriteExt;

const NOW: u64 = 1_800_000_000;

fn tls_config(dir: &Path) -> Config {
    let text = format!(
        "[server]\ndata_dir = {:?}\n[listen]\naddress = \"127.0.0.1:0\"\nmode = \"tls\"\n[tls]\ncert_file = {:?}\nkey_file = {:?}\n",
        dir.join("data").display().to_string(),
        dir.join("cert.pem").display().to_string(),
        dir.join("key.pem").display().to_string(),
    );
    let config = config::parse(&text, Path::new("/test.toml"), &config::Process).unwrap();
    std::fs::create_dir_all(config.server.data_dir.join("workspaces")).unwrap();
    config
}

/// Writes a new pair where the configuration looks for one; its pin.
fn new_pair(dir: &Path) -> String {
    let pair = tls::self_signed(&["localhost".to_owned(), "127.0.0.1".to_owned()], NOW).unwrap();
    // As a renewal does it: written aside, then moved into place.
    for (name, text) in [("cert.pem", &pair.cert_pem), ("key.pem", &pair.key_pem)] {
        let aside = dir.join(format!("{name}.new"));
        std::fs::write(&aside, text).unwrap();
        std::fs::rename(&aside, dir.join(name)).unwrap();
    }
    tls::pin_of_pem(pair.cert_pem.as_bytes()).unwrap()
}

struct Running {
    addr: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    done: tokio::task::JoinHandle<()>,
}

async fn start(config: Config) -> Running {
    let options = Options {
        clock: Arc::new(ManualClock::at(NOW)),
        tls_reload_every: Duration::from_millis(20),
        ..Options::default()
    };
    let server = Server::bind(config, options).await.unwrap();
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
    Running { addr, stop, done }
}

async fn healthy(addr: SocketAddr, pin: &str) -> bool {
    let client = Client::pinned(addr, pin).unwrap();
    matches!(client.request("GET", "/healthz").send().await, Ok(answer) if answer.status == 204)
}

#[tokio::test]
async fn a_made_pair_is_served_and_the_client_connects_by_its_pin_alone() {
    let dir = tempfile::tempdir().unwrap();
    let config = tls_config(dir.path());
    let pin = new_pair(dir.path());
    let server = start(config).await;

    assert!(healthy(server.addr, &pin).await);
    // The pin is all the client goes by: this certificate is for
    // `localhost`, not for an address, and nobody signed it.
    let other = tls::pin_of_pem(
        tls::self_signed(&["localhost".to_owned()], NOW)
            .unwrap()
            .cert_pem
            .as_bytes(),
    )
    .unwrap();
    let refused = Client::pinned(server.addr, &other)
        .unwrap()
        .request("GET", "/healthz")
        .send()
        .await;
    assert!(refused.is_err(), "{refused:?}");
    // There is no plain HTTP on a TLS port.
    let plain = Client::plain(server.addr)
        .request("GET", "/healthz")
        .send()
        .await;
    assert!(plain.map(|answer| answer.status).unwrap_or(0) != 204);

    // A connection that never says hello holds nobody else up.
    let mut silent = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let mut rubbish = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    rubbish
        .write_all(b"\x16\x03\x01 not a handshake")
        .await
        .unwrap();
    let served = tokio::time::timeout(Duration::from_secs(5), healthy(server.addr, &pin)).await;
    assert_eq!(served, Ok(true));
    silent.write_all(b"x").await.unwrap();

    drop(server.stop);
    server.done.await.unwrap();
}

#[tokio::test]
async fn a_renewed_pair_is_served_without_a_restart_and_a_broken_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let config = tls_config(dir.path());
    let first = new_pair(dir.path());
    let server = start(config).await;
    assert!(healthy(server.addr, &first).await);

    let second = new_pair(dir.path());
    assert_ne!(first, second);
    let mut renewed = false;
    for _ in 0..500 {
        if healthy(server.addr, &second).await {
            renewed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(renewed, "the new certificate was never served");
    assert!(!healthy(server.addr, &first).await);

    // A renewal that went wrong: the pair before it stays in use.
    std::fs::write(
        dir.path().join("cert.pem"),
        "-----BEGIN CERTIFICATE-----\nbm8=\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(healthy(server.addr, &second).await);
    // So does a certificate with another certificate's key.
    let stray = tls::self_signed(&["localhost".to_owned()], NOW).unwrap();
    std::fs::write(dir.path().join("cert.pem"), stray.cert_pem).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(healthy(server.addr, &second).await);

    drop(server.stop);
    server.done.await.unwrap();
}

#[tokio::test]
async fn without_its_pair_the_server_does_not_start_and_says_how_to_make_one() {
    let dir = tempfile::tempdir().unwrap();
    let config = tls_config(dir.path());
    let refused = Server::bind(config.clone(), Options::default())
        .await
        .err()
        .unwrap();
    let said = refused.to_string();
    for part in ["cert.pem", "key.pem", "passalong-server tls self-signed"] {
        assert!(said.contains(part), "{said}");
    }
    // Half a pair is no pair.
    let pair = tls::self_signed(&["localhost".to_owned()], NOW).unwrap();
    std::fs::write(dir.path().join("cert.pem"), pair.cert_pem).unwrap();
    assert!(Server::bind(config, Options::default()).await.is_err());
}

#[tokio::test]
async fn in_plain_mode_there_is_no_tls() {
    let server = common::TestServer::start().await;
    let pin = tls::pin_of_pem(
        tls::self_signed(&["localhost".to_owned()], NOW)
            .unwrap()
            .cert_pem
            .as_bytes(),
    )
    .unwrap();
    let answer = Client::pinned(server.addr, &pin)
        .unwrap()
        .request("GET", "/healthz")
        .send()
        .await;
    assert!(answer.is_err());
    assert_eq!(server.get("/healthz").await.status, 204);
}
