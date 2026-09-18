//! What a running server does unasked: the janitor, and stopping (PLAN-00004,
//! step 10). The binary itself is tested in the CLI crate.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{TestServer, honest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn staged_files(dir: &Path) -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += staged_files(&path);
        } else if path.components().any(|part| part.as_os_str() == "staging") {
            count += 1;
        }
    }
    count
}

#[tokio::test]
async fn the_janitor_forgets_an_upload_nobody_finished() {
    let server = TestServer::start_with("", |options| {
        options.janitor_every = Duration::from_millis(20);
    })
    .await;
    let item = honest(1, b"abandoned");
    let upload = server.begin(&item).await.json()["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    server
        .put_content(&server.token, &upload, &item.content)
        .await;
    let data = server.config.server.data_dir.clone();
    assert!(staged_files(&data) > 0);

    // The janitor has passed many times by now, and it is not yet old.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(staged_files(&data) > 0);

    server.clock.advance(25 * 3_600);
    let waited = Instant::now();
    while staged_files(&data) > 0 {
        assert!(waited.elapsed() < Duration::from_secs(10), "never cleaned");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let late = server
        .call("POST", &format!("/v1/uploads/{upload}/commit"), None)
        .await;
    assert_eq!(late.status, 404);
    assert_eq!(server.get("/v1/workspace").await.json()["usedBytes"], "0");
}

async fn half_an_upload(server: &TestServer, content: &[u8]) -> tokio::net::TcpStream {
    let item = honest(1, content);
    let upload = server.begin(&item).await.json()["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let head = format!(
        "PUT /v1/uploads/{upload}/content HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        server.token,
        content.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream
        .write_all(&content[..content.len() / 2])
        .await
        .unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stream
}

#[tokio::test]
async fn a_request_in_flight_is_finished_before_the_server_stops() {
    let server = TestServer::start().await;
    let content = vec![7_u8; 200_000];
    let mut stream = half_an_upload(&server, &content).await;
    let addr = server.addr;

    let stopping = tokio::spawn(server.shutdown());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!stopping.is_finished(), "it did not wait for the upload");
    // Nothing new is accepted meanwhile.
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "still listening"
    );

    stream
        .write_all(&content[content.len() / 2..])
        .await
        .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.unwrap();
    assert!(answer.starts_with("HTTP/1.1 204"), "{answer}");
    tokio::time::timeout(Duration::from_secs(5), stopping)
        .await
        .expect("it never stopped")
        .unwrap();
}

#[tokio::test]
async fn a_request_that_never_ends_is_waited_for_only_so_long() {
    let server = TestServer::start_with("", |options| {
        options.drain = Duration::from_millis(300);
    })
    .await;
    let _stream = half_an_upload(&server, &vec![7_u8; 200_000]).await;
    let began = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .expect("it waited for ever");
    assert!(began.elapsed() >= Duration::from_millis(300));
}
