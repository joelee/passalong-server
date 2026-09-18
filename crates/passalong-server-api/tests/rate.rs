//! Rate limiting of failed authentications (PLAN-00004, step 9; D-03).

mod common;

use common::TestServer;
use passalong_server_api::client::Response;

const WRONG: &str =
    "pal_0123456789ab_0000000000000000000000000000000000000000000000000000000000000000";
const LIMIT: &str = "[limits]\nauth_failures_per_minute = 3\n";

fn code(response: &Response) -> String {
    response.json()["code"].as_str().unwrap_or("").to_owned()
}

impl TestServer {
    async fn viewer(&self, token: &str, forwarded: Option<&str>) -> Response {
        let mut request = self.client().request("GET", "/v1/viewer").bearer(token);
        if let Some(forwarded) = forwarded {
            request = request.header("X-Forwarded-For", forwarded);
        }
        request.send().await.unwrap()
    }
}

#[tokio::test]
async fn wrong_keys_close_the_door_for_a_minute_and_for_right_keys_too() {
    let server = TestServer::start_with(LIMIT, |_| {}).await;
    // What succeeds is never counted.
    for _ in 0..10 {
        assert_eq!(server.viewer(&server.token, None).await.status, 200);
    }
    for _ in 0..3 {
        let refused = server.viewer(WRONG, None).await;
        assert_eq!(
            (refused.status, code(&refused).as_str()),
            (401, "UNAUTHENTICATED")
        );
        server.clock.advance(10);
    }
    // The fourth is not looked at, and neither is a right key.
    for token in [WRONG, server.token.as_str()] {
        let limited = server.viewer(token, None).await;
        assert_eq!(
            (limited.status, code(&limited).as_str()),
            (429, "RATE_LIMITED")
        );
        assert_eq!(limited.json()["retryable"], true);
        // The oldest failure is 30 seconds old.
        assert_eq!(limited.header("retry-after"), Some("30"));
    }
    // No key at all is a failed authentication as well, and as limited.
    let bare = server
        .client()
        .request("GET", "/v1/viewer")
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status, 429);
    // The probes are not behind the door.
    let health = server
        .client()
        .request("GET", "/healthz")
        .send()
        .await
        .unwrap();
    assert_eq!(health.status, 204);

    // The window slides: when the oldest failure is a minute old, there is
    // room for one more.
    server.clock.advance(30);
    assert_eq!(server.viewer(&server.token, None).await.status, 200);
    assert_eq!(server.viewer(WRONG, None).await.status, 401);
    assert_eq!(server.viewer(&server.token, None).await.status, 429);
    server.clock.advance(60);
    assert_eq!(server.viewer(&server.token, None).await.status, 200);
}

#[tokio::test]
async fn an_expired_key_is_no_guess_and_is_not_counted() {
    let server = TestServer::start_with(LIMIT, |_| {}).await;
    server.clock.advance(91 * common::DAY);
    for _ in 0..5 {
        let expired = server.viewer(&server.token, None).await;
        assert_eq!(
            (expired.status, code(&expired).as_str()),
            (401, "KEY_EXPIRED")
        );
    }
    assert_eq!(server.viewer(&server.other_token, None).await.status, 200);
}

#[tokio::test]
async fn behind_a_proxy_the_address_is_the_one_the_proxy_wrote() {
    let extra = format!("behind_proxy = true\n{LIMIT}");
    let server = TestServer::start_with(&extra, |_| {}).await;
    // The proxy appends the address it saw; whatever stands before that is
    // the client's own claim. A forged one does not move the count.
    for forged in ["1.1.1.1", "2.2.2.2", "3.3.3.3"] {
        let chain = format!("{forged}, 203.0.113.9");
        assert_eq!(server.viewer(WRONG, Some(&chain)).await.status, 401);
    }
    let limited = server
        .viewer(&server.token, Some("4.4.4.4, 203.0.113.9"))
        .await;
    assert_eq!(limited.status, 429);
    // Someone else, though they claim to be the limited one.
    let other = server
        .viewer(&server.token, Some("203.0.113.9, 198.51.100.7"))
        .await;
    assert_eq!(other.status, 200);
    // Without the header, or with rubbish in it, it is the proxy's own
    // address that counts.
    assert_eq!(server.viewer(&server.token, None).await.status, 200);
    assert_eq!(
        server
            .viewer(&server.token, Some("not an address"))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn without_a_proxy_the_header_is_not_read() {
    let server = TestServer::start_with(LIMIT, |_| {}).await;
    for claimed in ["1.1.1.1", "2.2.2.2", "3.3.3.3"] {
        assert_eq!(server.viewer(WRONG, Some(claimed)).await.status, 401);
    }
    // Another claimed address is the same connection's address all the same.
    assert_eq!(
        server.viewer(&server.token, Some("4.4.4.4")).await.status,
        429
    );
}
