//! The binary as a server (PLAN-00004, step 10): started, asked whether it
//! is healthy, used, told to stop, and its log read for what must not be
//! in it.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use passalong_server_api::client::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BIN: &str = env!("CARGO_BIN_EXE_passalong-server");

struct Host {
    dir: tempfile::TempDir,
    addr: SocketAddr,
}

impl Host {
    /// A host after `init`, set to listen on a free port of this machine.
    fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Free a moment ago, which is as good as it gets: `check --health`
        // goes by the configured address, so that cannot be port 0.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let host = Self { dir, addr };
        host.run(&[
            "init",
            "--data-dir",
            host.dir.path().join("data").to_str().unwrap(),
            "--config-file",
            host.config().to_str().unwrap(),
        ]);
        let text = std::fs::read_to_string(host.config()).unwrap();
        let text = text
            .replace(
                "address = \"0.0.0.0:8443\"",
                &format!("address = \"{addr}\""),
            )
            .replace("mode = \"tls\"", &format!("mode = \"{mode}\""));
        assert!(text.contains(&addr.to_string()) && text.contains(&format!("mode = \"{mode}\"")));
        std::fs::write(host.config(), text).unwrap();
        host
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("etc/config.toml")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("serve.log")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(BIN);
        if args[0] != "init" {
            command.arg("--config").arg(self.config());
        }
        command
            .args(args)
            .env_remove("PASSALONG_SERVER_CONFIG_FILE")
            .env("PASSALONG_SERVER_LOG_LEVEL", "debug")
            .env("HOME", self.dir.path())
            .env_remove("XDG_CONFIG_HOME");
        command
    }

    /// A command that must succeed; its standard output.
    fn run(&self, args: &[&str]) -> String {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn healthy(&self) -> bool {
        let output = self.command(&["check", "--health"]).output().unwrap();
        assert!(
            matches!(output.status.code(), Some(0 | 1)),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.status.success()
    }

    /// `serve`, with its log in a file, awaited until it is ready.
    fn serve(&self) -> Child {
        let log = std::fs::File::create(self.log()).unwrap();
        let child = self
            .command(&["serve"])
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let began = Instant::now();
        while !self.healthy() {
            assert!(
                began.elapsed() < Duration::from_secs(20),
                "never ready: {}",
                std::fs::read_to_string(self.log()).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }

    /// A workspace and a key for it: the key, and its public id.
    fn key(&self) -> (String, String) {
        self.run(&["workspace", "create", "home"]);
        let out = self.run(&["key", "create", "--workspace", "home", "--label", "laptop"]);
        let token = out
            .split_whitespace()
            .find(|word| word.starts_with("pal_"))
            .unwrap()
            .to_owned();
        let id = token.split('_').nth(1).unwrap().to_owned();
        (token, id)
    }
}

fn terminate(child: &Child) {
    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn exit_code(mut child: Child) -> Option<i32> {
    let began = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        if began.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            panic!("it never exited");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn no_secret_in(log: &Path, token: &str) -> String {
    let log = std::fs::read_to_string(log).unwrap();
    let secret = token.rsplit('_').next().unwrap();
    assert_eq!(secret.len(), 64);
    // No part of the secret: not whole, and not the start of it either.
    for part in [
        token,
        secret,
        &secret[..16],
        &secret[48..],
        "Bearer",
        "uthorization",
    ] {
        assert!(!log.contains(part), "the log holds {part:?}:\n{log}");
    }
    log
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_serves_plain_http_until_it_is_told_to_stop() {
    let host = Host::new("plain");
    assert!(!host.healthy(), "nothing listens yet");
    let child = host.serve();
    // A key made while the server runs holds from the next request on.
    let (token, key_id) = host.key();
    let client = Client::plain(host.addr);

    let viewer = client
        .request("GET", "/v1/viewer")
        .bearer(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(viewer.status, 200);
    assert_eq!(viewer.json()["key"]["id"], key_id.as_str());
    let wrong = format!(
        "{}{}",
        &token[..token.len() - 1],
        if token.ends_with('0') { '1' } else { '0' }
    );
    let refused = client
        .request("GET", "/v1/viewer")
        .bearer(&wrong)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status, 401);

    // An upload is half sent when the server is told to stop. It is finished.
    let begun = client
        .request("POST", "/v1/uploads")
        .bearer(&token)
        .json(&serde_json::json!({ "id": "00000001-aaaaaaaaaaaa", "meta": { "schema": 2 }, "size": "200000" }))
        .send()
        .await
        .unwrap();
    // A plaintext workspace checks content against `meta`, at the commit;
    // this upload is never committed.
    assert_eq!(
        begun.status,
        201,
        "{}",
        String::from_utf8_lossy(&begun.body)
    );
    let upload = begun.json()["uploadId"].as_str().unwrap().to_owned();
    let mut stream = tokio::net::TcpStream::connect(host.addr).await.unwrap();
    let head = format!(
        "PUT /v1/uploads/{upload}/content HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {token}\r\nConnection: close\r\nContent-Length: 200000\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&[7; 100_000]).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    terminate(&child);
    tokio::time::sleep(Duration::from_millis(200)).await;
    stream.write_all(&[7; 100_000]).await.unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 204"), "{answer}");

    assert_eq!(exit_code(child), Some(0));
    assert!(!host.healthy(), "it is gone");

    let log = no_secret_in(&host.log(), &token);
    for expected in [
        format!("address={}", host.addr),
        "mode=\"plain\"".to_owned(),
        format!("key={key_id}"),
        "operation=\"getViewer\"".to_owned(),
        "operation=\"putUploadContent\"".to_owned(),
        "status=200".to_owned(),
        "status=401".to_owned(),
        "request=".to_owned(),
        "ms=".to_owned(),
    ] {
        assert!(log.contains(&expected), "no {expected} in:\n{log}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_serves_tls_and_its_own_health_check_goes_by_the_pin() {
    let host = Host::new("tls");
    // Without the pair it does not start, and says what to do.
    let refused = host.command(&["serve"]).output().unwrap();
    assert_eq!(refused.status.code(), Some(1));
    let said = String::from_utf8_lossy(&refused.stderr).into_owned();
    for part in ["cert.pem", "key.pem", "passalong-server tls self-signed"] {
        assert!(said.contains(part), "{said}");
    }

    host.run(&[
        "tls",
        "self-signed",
        "--host",
        "localhost",
        "--ip",
        "127.0.0.1",
    ]);
    let pin = host.run(&["tls", "fingerprint"]).trim().to_owned();
    let child = host.serve();
    let (token, key_id) = host.key();

    let client = Client::pinned(host.addr, &pin).unwrap();
    let viewer = client
        .request("GET", "/v1/viewer")
        .bearer(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(viewer.status, 200);
    assert_eq!(viewer.json()["key"]["id"], key_id.as_str());
    // Plain HTTP gets nothing from it.
    let plain = Client::plain(host.addr)
        .request("GET", "/healthz")
        .send()
        .await;
    assert!(plain.map(|answer| answer.status).unwrap_or(0) != 204);

    // Not ready is not healthy: the data directory goes away.
    let data = host.dir.path().join("data");
    let aside = host.dir.path().join("data.aside");
    std::fs::rename(&data, &aside).unwrap();
    assert!(!host.healthy());
    assert_eq!(
        client
            .request("GET", "/healthz")
            .send()
            .await
            .unwrap()
            .status,
        204
    );
    std::fs::rename(&aside, &data).unwrap();
    assert!(host.healthy());

    terminate(&child);
    assert_eq!(exit_code(child), Some(0));
    let log = no_secret_in(&host.log(), &token);
    assert!(log.contains("mode=\"tls\""), "{log}");
    assert!(!log.contains("PRIVATE KEY"));
}
