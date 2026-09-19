//! The skeleton and the authentication layer (PLAN-00004, steps 3 and 4).

mod common;

use common::{DAY, TestServer};
use passalong_server_api::Options;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn the_health_checks_need_no_key_and_say_nothing() {
    let server = TestServer::start().await;
    for path in ["/healthz", "/readyz"] {
        let response = server.client().request("GET", path).send().await.unwrap();
        assert_eq!(response.status, 204, "{path}");
        assert!(response.body.is_empty());
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_refusal_is_a_problem_with_a_stable_code() {
    let server = TestServer::start().await;
    let response = server.call("GET", "/v1/no-such-thing", None).await;
    assert_eq!(response.status, 404);
    assert_eq!(
        response.header("content-type"),
        Some("application/problem+json")
    );
    let problem = response.json();
    assert_eq!(problem["code"], "NOT_FOUND");
    assert_eq!(problem["status"], 404);
    assert_eq!(problem["retryable"], false);
    assert!(problem["title"].is_string());
    // A method the path does not have.
    assert_eq!(server.call("DELETE", "/v1/viewer", None).await.status, 405);
    server.shutdown().await;
}

#[tokio::test]
async fn a_request_id_is_kept_when_plausible_and_made_otherwise() {
    let server = TestServer::start().await;
    let given = server
        .client()
        .request("GET", "/healthz")
        .header("X-Request-Id", "client-7f3a.41")
        .send()
        .await
        .unwrap();
    assert_eq!(given.header("x-request-id"), Some("client-7f3a.41"));
    for odd in ["", "has space", "quote\"and;semicolon", &"a".repeat(200)] {
        let made = server
            .client()
            .request("GET", "/healthz")
            .header("X-Request-Id", odd)
            .send()
            .await
            .unwrap();
        let id = made.header("x-request-id").unwrap();
        assert!(
            id != odd && id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{id:?}"
        );
    }
    let none = server
        .client()
        .request("GET", "/healthz")
        .send()
        .await
        .unwrap();
    assert_eq!(none.header("x-request-id").unwrap().len(), 32);
    server.shutdown().await;
}

#[tokio::test]
async fn a_handler_that_panics_is_a_500_and_the_server_lives() {
    let server =
        TestServer::start_with("", |options: &mut Options| options.panic_route = true).await;
    let response = server.call("GET", "/v1/test-panic", None).await;
    assert_eq!(response.status, 500);
    assert!(
        !String::from_utf8_lossy(&response.body).contains("boom"),
        "no detail leaves the server"
    );
    assert_eq!(server.call("GET", "/v1/viewer", None).await.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn the_viewer_is_the_key_and_the_server() {
    let server = TestServer::start().await;
    let viewer = server.call("GET", "/v1/viewer", None).await;
    assert_eq!(viewer.status, 200);
    let viewer = viewer.json();
    assert_eq!(viewer["key"]["id"], server.token[4..16]);
    assert_eq!(viewer["key"]["role"], "readWrite");
    assert_eq!(viewer["key"]["label"], "laptop");
    assert!(viewer["key"]["expiresAt"].as_str().unwrap().ends_with('Z'));
    assert_eq!(viewer["server"]["apiVersion"], 1);
    // Where this server's source is, for a client to show (AGPL, section 13).
    assert_eq!(
        viewer["server"]["sourceUrl"],
        "https://github.com/joelee/passalong-server"
    );
    assert!(
        viewer["server"]["maxItemBytes"].is_null(),
        "unlimited is said as null"
    );
    assert!(viewer["server"]["version"].is_string());

    let workspace = server.call("GET", "/v1/workspace", None).await.json();
    assert_eq!(workspace["name"], "home");
    assert_eq!(workspace["itemCount"], 0);
    assert_eq!(workspace["usedBytes"], "0");
    assert_eq!(workspace["quotaBytes"], (20_u64 << 30).to_string());
    assert_eq!(workspace["encryption"]["state"], "plaintext");
    assert!(workspace["encryption"]["keyId"].is_null());
    // The other key sees the other workspace, and nothing names either.
    assert_eq!(
        server
            .call_as(&server.other_token, "GET", "/v1/workspace", None)
            .await
            .json()["name"],
        "other"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn whatever_is_wrong_with_a_key_the_answer_is_the_same() {
    let server = TestServer::start().await;
    let wrong_secret = format!(
        "{}{}",
        &server.token[..80],
        if server.token.ends_with('0') {
            '1'
        } else {
            '0'
        }
    );
    let unknown = format!("pal_{}_{}", "0".repeat(12), &server.token[17..]);
    let mut answers = Vec::new();
    for header in [
        None,
        Some("Bearer".to_owned()),
        Some("Basic dXNlcjpwYXNz".to_owned()),
        Some("Bearer nonsense".to_owned()),
        Some(format!("Bearer {wrong_secret}")),
        Some(format!("Bearer {unknown}")),
    ] {
        let mut request = server.client().request("GET", "/v1/viewer");
        if let Some(header) = &header {
            request = request.header("Authorization", header);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status, 401, "{header:?}");
        assert_eq!(response.header("www-authenticate"), Some("Bearer"));
        let mut problem = response.json();
        assert_eq!(problem["code"], "UNAUTHENTICATED");
        problem.as_object_mut().unwrap().remove("requestId");
        answers.push(problem.to_string());
    }
    answers.dedup();
    assert_eq!(answers.len(), 1, "the refusals differ: {answers:?}");
    server.shutdown().await;
}

#[tokio::test]
async fn an_expired_key_and_a_revoked_one_are_told_so_from_the_next_request() {
    let server = TestServer::start().await;
    assert_eq!(server.call("GET", "/v1/viewer", None).await.status, 200);
    // The operator revokes the other key, from another connection.
    let cli = server.control();
    let other_id = &server.other_token[4..16];
    cli.revoke_key(other_id).unwrap();
    let refused = server
        .call_as(&server.other_token, "GET", "/v1/viewer", None)
        .await;
    assert_eq!(
        (
            refused.status,
            refused.json()["code"].as_str().unwrap().to_owned()
        ),
        (401, "KEY_REVOKED".to_owned())
    );
    assert_eq!(refused.json()["retryable"], false);

    server.clock.advance(90 * DAY);
    let expired = server.call("GET", "/v1/viewer", None).await;
    assert_eq!(expired.json()["code"], "KEY_EXPIRED");
    cli.extend_key(&server.token[4..16], Some(DAY)).unwrap();
    assert_eq!(server.call("GET", "/v1/viewer", None).await.status, 200);
    server.shutdown().await;
}

#[tokio::test]
async fn a_wrong_key_is_answered_before_its_body_is_sent() {
    let server = TestServer::start().await;
    let mut stream = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let head = format!(
        "POST /v1/uploads HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer pal_{}_{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        "0".repeat(12),
        "0".repeat(64),
        8 << 20
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    // Not one byte of the body is sent; the answer must come all the same.
    let mut answer = vec![0_u8; 64];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut answer))
        .await
        .expect("answered without the body")
        .unwrap();
    assert!(
        String::from_utf8_lossy(&answer[..read]).starts_with("HTTP/1.1 401"),
        "{}",
        String::from_utf8_lossy(&answer[..read])
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_control_database_that_cannot_answer_is_a_503_and_not_ready() {
    let server = TestServer::start().await;
    assert_eq!(server.call("GET", "/v1/viewer", None).await.status, 200);
    let raw = rusqlite::Connection::open(server.config.control_database()).unwrap();
    raw.execute_batch("DROP TABLE api_keys").unwrap();
    let response = server.call("GET", "/v1/viewer", None).await;
    assert_eq!(response.status, 503);
    assert_eq!(response.json()["code"], "SERVICE_UNAVAILABLE");
    assert_eq!(response.json()["retryable"], true);
    assert_eq!(
        server
            .client()
            .request("GET", "/readyz")
            .send()
            .await
            .unwrap()
            .status,
        503
    );
    assert_eq!(
        server
            .client()
            .request("GET", "/healthz")
            .send()
            .await
            .unwrap()
            .status,
        204,
        "alive, but not ready"
    );
    server.shutdown().await;
}
