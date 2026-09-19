//! Encryption and the rewrite session over HTTP (PLAN-00004, step 6). The
//! rules themselves are the core's, and tested there; these tests are of
//! the routes: what they take, what they answer, and with which status.

mod common;

use common::{DAY, TestServer, honest};
use passalong_server_api::client::Response;
use passalong_server_core::workspace::Role;
use serde_json::{Value, json};

const K1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const K2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn code(response: &Response) -> String {
    response.json()["code"].as_str().unwrap_or("").to_owned()
}

fn text(response: &Response) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

fn seal(key: &str) -> Value {
    json!({ "keyId": key, "header": { "schema": 1, "wrapped": "d2hhdGV2ZXI" } })
}

fn migrate(key: &str) -> Value {
    json!({ "kind": "migrate", "expectedKeyId": null, "newKeyId": key, "newHeader": { "schema": 1 } })
}

impl TestServer {
    /// Another key for `home`.
    fn key(&self, label: &str, role: Role) -> String {
        self.control()
            .create_key("home", label, role, Some(DAY))
            .unwrap()
            .0
            .reveal()
    }

    /// Stages the re-sealed copy of `id`; the answer of the commit.
    async fn stage(&self, token: &str, id: &str, key: &str, content: &[u8]) -> Response {
        let body = json!({
            "id": id,
            "meta": { "schema": 2, "sealed": "c2VhbGVk" },
            "size": content.len().to_string(),
            "expectedKeyId": key,
            "inRewrite": true,
        });
        let begun = self.call_as(token, "POST", "/v1/uploads", Some(body)).await;
        if begun.status != 201 {
            return begun;
        }
        let upload = begun.json()["uploadId"].as_str().unwrap().to_owned();
        assert_eq!(self.put_content(token, &upload, content).await.status, 204);
        self.call_as(token, "POST", &format!("/v1/uploads/{upload}/commit"), None)
            .await
    }
}

#[tokio::test]
async fn a_workspace_is_sealed_and_its_header_replaced() {
    let server = TestServer::start().await;
    let sealed = server
        .call("PUT", "/v1/workspace/encryption", Some(seal(K1)))
        .await;
    assert_eq!(sealed.status, 200, "{}", text(&sealed));
    let view = sealed.json();
    assert_eq!(view["state"], "sealed");
    assert_eq!(view["keyId"], K1);
    assert_eq!(view["header"], seal(K1)["header"]);
    assert_eq!(view["rewrite"], Value::Null);

    // Sent again, it is the same answer; under another key it is refused.
    let again = server
        .call("PUT", "/v1/workspace/encryption", Some(seal(K1)))
        .await;
    assert_eq!(again.status, 200);
    let other = server
        .call("PUT", "/v1/workspace/encryption", Some(seal(K2)))
        .await;
    assert_eq!(
        (other.status, code(&other).as_str()),
        (409, "KEY_ID_MISMATCH")
    );

    // A new passphrase wraps the same key anew.
    let replaced = server
        .call(
            "PUT",
            "/v1/workspace/encryption/header",
            Some(json!({ "expectedKeyId": K1, "header": { "schema": 1, "wrapped": "bmV3" } })),
        )
        .await;
    assert_eq!(replaced.status, 200, "{}", text(&replaced));
    assert_eq!(replaced.json()["header"]["wrapped"], "bmV3");
    let stale = server
        .call(
            "PUT",
            "/v1/workspace/encryption/header",
            Some(json!({ "expectedKeyId": K2, "header": {} })),
        )
        .await;
    assert_eq!(
        (stale.status, code(&stale).as_str()),
        (409, "KEY_ID_MISMATCH")
    );
    assert_eq!(
        server.get("/v1/workspace").await.json()["encryption"]["header"]["wrapped"],
        "bmV3"
    );
}

#[tokio::test]
async fn the_header_comes_back_byte_for_byte() {
    // Like `meta`, the header is the client's document. The client may well
    // authenticate it, so the server must not so much as reorder its keys.
    let server = TestServer::start().await;
    let header = "{ \"zeta\":1,\"alpha\" : [1,  2.50],\n \"n\":1e2 }";
    let body = format!("{{\"keyId\":\"{K1}\",\"header\":{header}}}");
    let sealed = server
        .client()
        .request("PUT", "/v1/workspace/encryption")
        .bearer(&server.token)
        .header("Content-Type", "application/json")
        .body_raw(body.into_bytes())
        .send()
        .await
        .unwrap();
    assert_eq!(sealed.status, 200, "{}", text(&sealed));
    assert!(text(&sealed).contains(header), "{}", text(&sealed));
    let workspace = server.get("/v1/workspace").await;
    assert!(text(&workspace).contains(header), "{}", text(&workspace));
}

#[tokio::test]
async fn a_fresh_start_drops_the_items_and_seals() {
    let server = TestServer::start().await;
    server.upload(&honest(1, b"hello")).await;
    let fresh = server
        .call(
            "POST",
            "/v1/workspace/encryption/fresh-start",
            Some(seal(K1)),
        )
        .await;
    assert_eq!(fresh.status, 200, "{}", text(&fresh));
    assert_eq!(fresh.json()["state"], "sealed");
    assert_eq!(server.get("/v1/items").await.json(), json!([]));
    // What was there is not destroyed: it is the plain partition, which
    // still counts, until its owner deletes it.
    let plain = server.get("/v1/item-ids?partition=plain").await.json();
    assert_eq!(plain, json!([honest(1, b"hello").id]));
    assert_eq!(server.get("/v1/workspace").await.json()["usedBytes"], "5");
}

#[tokio::test]
async fn a_migration_runs_from_begin_to_commit() {
    let server = TestServer::start().await;
    let (first, second) = (honest(1, b"hello"), honest(2, b"world!"));
    server.upload(&first).await;
    server.upload(&second).await;

    let none = server.get("/v1/rewrite").await;
    assert_eq!((none.status, code(&none).as_str()), (404, "NOT_FOUND"));

    let begun = server.call("POST", "/v1/rewrite", Some(migrate(K1))).await;
    assert_eq!(begun.status, 201, "{}", text(&begun));
    let session = begun.json();
    assert_eq!(session["kind"], "migrate");
    assert_eq!(session["newKeyId"], K1);
    assert_eq!(session["sourceItems"], 2);
    assert_eq!(session["stagedIds"], json!([]));
    assert_eq!(session["leaseExpiresAt"], "2027-01-15T08:10:00Z");
    assert_eq!(
        session["holder"],
        server.get("/v1/viewer").await.json()["key"]["id"]
    );
    // Sent again by the holder, it is the same session.
    let again = server.call("POST", "/v1/rewrite", Some(migrate(K1))).await;
    assert_eq!(again.status, 201);

    // Readers still go by what was there before: a plaintext workspace.
    let encryption = server.get("/v1/workspace").await.json()["encryption"].clone();
    assert_eq!(encryption["state"], "rewriting");
    assert_eq!(encryption["keyId"], Value::Null);
    assert_eq!(encryption["rewrite"]["newKeyId"], K1);
    // And writers wait.
    let shut_out = server.begin(&honest(3, b"later")).await;
    assert_eq!(
        (shut_out.status, code(&shut_out).as_str()),
        (409, "REWRITE_IN_PROGRESS")
    );
    assert_eq!(
        shut_out.json()["leaseExpiresAt"],
        "2027-01-15T08:10:00Z",
        "so that the client can say how long"
    );

    let staged = server
        .stage(&server.token, &first.id, K1, b"SEALED-1")
        .await;
    assert_eq!(staged.status, 200, "{}", text(&staged));
    assert_eq!(staged.json()["item"]["id"], first.id.as_str());
    assert_eq!(staged.json()["item"]["storedBytes"], "8");
    assert_eq!(
        server.get("/v1/rewrite").await.json()["stagedIds"],
        json!([first.id])
    );
    assert_eq!(
        server.get("/v1/item-ids?partition=staged").await.json(),
        json!([first.id])
    );
    let content = server
        .get(&format!("/v1/items/{}/content?partition=staged", first.id))
        .await;
    assert_eq!(content.body, b"SEALED-1");
    assert_eq!(
        server
            .get(&format!("/v1/items/{}/content", first.id))
            .await
            .body,
        b"hello"
    );

    let early = server
        .call(
            "POST",
            "/v1/rewrite/commit",
            Some(json!({ "newKeyId": K1 })),
        )
        .await;
    assert_eq!(
        (early.status, code(&early).as_str()),
        (409, "REWRITE_INCOMPLETE")
    );

    server.clock.advance(60);
    let beat = server.call("POST", "/v1/rewrite/heartbeat", None).await;
    assert_eq!(beat.status, 200, "{}", text(&beat));
    assert_eq!(beat.json()["leaseExpiresAt"], "2027-01-15T08:11:00Z");

    server
        .stage(&server.token, &second.id, K1, b"SEALED-2!")
        .await;
    let committed = server
        .call(
            "POST",
            "/v1/rewrite/commit",
            Some(json!({ "newKeyId": K1 })),
        )
        .await;
    assert_eq!(committed.status, 200, "{}", text(&committed));
    let view = committed.json();
    assert_eq!(view["state"], "sealed");
    assert_eq!(view["keyId"], K1);
    assert_eq!(view["header"], json!({ "schema": 1 }));
    assert_eq!(view["rewrite"], Value::Null);
    assert_eq!(
        server
            .get(&format!("/v1/items/{}/content", second.id))
            .await
            .body,
        b"SEALED-2!"
    );
    // The answer was lost and the commit is sent again: the same answer.
    let again = server
        .call(
            "POST",
            "/v1/rewrite/commit",
            Some(json!({ "newKeyId": K1 })),
        )
        .await;
    assert_eq!(again.status, 200, "{}", text(&again));
    assert_eq!(again.json()["state"], "sealed");
}

#[tokio::test]
async fn a_session_is_taken_over_and_aborted() {
    let server = TestServer::start().await;
    server.upload(&honest(1, b"hello")).await;
    let desktop = server.key("desktop", Role::ReadWrite);
    server.call("POST", "/v1/rewrite", Some(migrate(K1))).await;

    let early = server
        .call_as(&desktop, "POST", "/v1/rewrite/take-over", None)
        .await;
    assert_eq!((early.status, code(&early).as_str()), (409, "LEASE_HELD"));
    assert_eq!(early.json()["leaseExpiresAt"], "2027-01-15T08:10:00Z");
    assert_eq!(early.json()["retryable"], true);

    server.clock.advance(601);
    let taken = server
        .call_as(&desktop, "POST", "/v1/rewrite/take-over", None)
        .await;
    assert_eq!(taken.status, 200, "{}", text(&taken));
    assert_eq!(
        taken.json()["holder"],
        server
            .call_as(&desktop, "GET", "/v1/viewer", None)
            .await
            .json()["key"]["id"]
    );

    // The former holder is refused everything of the session.
    for (method, path, body) in [
        ("POST", "/v1/rewrite/heartbeat", None),
        (
            "POST",
            "/v1/rewrite/commit",
            Some(json!({ "newKeyId": K1 })),
        ),
        ("POST", "/v1/rewrite/abort", Some(json!({ "newKeyId": K1 }))),
    ] {
        let refused = server.call(method, path, body).await;
        assert_eq!(
            (refused.status, code(&refused).as_str()),
            (409, "LEASE_HELD"),
            "{path}"
        );
    }

    let aborted = server
        .call_as(
            &desktop,
            "POST",
            "/v1/rewrite/abort",
            Some(json!({ "newKeyId": K1 })),
        )
        .await;
    assert_eq!(aborted.status, 200, "{}", text(&aborted));
    assert_eq!(aborted.json()["state"], "plaintext");
    assert_eq!(aborted.json()["rewrite"], Value::Null);
    assert_eq!(
        server
            .get("/v1/items")
            .await
            .json()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A `beginRewrite` that arrives late must not reopen what was aborted.
    let stale = server.call("POST", "/v1/rewrite", Some(migrate(K1))).await;
    assert_eq!(
        (stale.status, code(&stale).as_str()),
        (409, "REWRITE_ENDED")
    );
    let anew = server.call("POST", "/v1/rewrite", Some(migrate(K2))).await;
    assert_eq!(anew.status, 201, "{}", text(&anew));
}

#[tokio::test]
async fn whoever_takes_a_session_over_finds_the_header_the_new_words_unlock() {
    // Resuming needs the new words, and the words need the header they
    // unlock. The old header is what readers get until the commit; without
    // the new one in the session, a second device could only abort, and the
    // holder itself could not resume after losing its memory.
    let server = TestServer::start().await;
    let old_header = "{ \"wrapped\":\"old\" }";
    let sealed = format!("{{\"keyId\":\"{K1}\",\"header\":{old_header}}}");
    let raw = |method: &'static str, path: &'static str, body: String| {
        server
            .client()
            .request(method, path)
            .bearer(&server.token)
            .header("Content-Type", "application/json")
            .body_raw(body.into_bytes())
            .send()
    };
    assert_eq!(
        raw("PUT", "/v1/workspace/encryption", sealed)
            .await
            .unwrap()
            .status,
        200
    );
    // Byte for byte, like every document of the client's.
    let new_header = "{ \"zeta\":1,\"wrapped\" : \"new\",\n \"n\":1e2 }";
    let begin = format!(
        "{{\"kind\":\"rotate\",\"expectedKeyId\":\"{K1}\",\"newKeyId\":\"{K2}\",\"newHeader\":{new_header}}}"
    );
    let begun = raw("POST", "/v1/rewrite", begin).await.unwrap();
    assert_eq!(begun.status, 201, "{}", text(&begun));
    assert!(text(&begun).contains(new_header), "{}", text(&begun));

    let desktop = server.key("desktop", Role::ReadWrite);
    server.clock.advance(601);
    let taken = server
        .call_as(&desktop, "POST", "/v1/rewrite/take-over", None)
        .await;
    assert_eq!(taken.status, 200);
    assert!(text(&taken).contains(new_header), "{}", text(&taken));
    let session = server.call_as(&desktop, "GET", "/v1/rewrite", None).await;
    assert!(text(&session).contains(new_header), "{}", text(&session));
    assert_eq!(session.json()["newHeader"]["wrapped"], "new");

    // Readers still get what opens the items that are there: the old one.
    let workspace = server.call_as(&desktop, "GET", "/v1/workspace", None).await;
    let encryption = workspace.json()["encryption"].clone();
    assert_eq!(encryption["header"]["wrapped"], "old");
    assert_eq!(encryption["rewrite"]["newHeader"]["wrapped"], "new");
    assert!(text(&workspace).contains(old_header) && text(&workspace).contains(new_header));

    // After the commit it is simply the header, and no session is left.
    let committed = server
        .call_as(
            &desktop,
            "POST",
            "/v1/rewrite/commit",
            Some(json!({ "newKeyId": K2 })),
        )
        .await;
    assert_eq!(committed.status, 200, "{}", text(&committed));
    assert!(text(&committed).contains(new_header));
    assert_eq!(committed.json()["rewrite"], Value::Null);
}

#[tokio::test]
async fn a_read_only_key_reads_the_session_and_changes_nothing() {
    let server = TestServer::start().await;
    let reader = server.key("kiosk", Role::ReadOnly);
    for (method, path, body) in [
        ("PUT", "/v1/workspace/encryption", Some(seal(K1))),
        (
            "POST",
            "/v1/workspace/encryption/fresh-start",
            Some(seal(K1)),
        ),
        (
            "PUT",
            "/v1/workspace/encryption/header",
            Some(json!({ "expectedKeyId": K1, "header": {} })),
        ),
        ("POST", "/v1/rewrite", Some(migrate(K1))),
        ("POST", "/v1/rewrite/heartbeat", None),
        ("POST", "/v1/rewrite/take-over", None),
        (
            "POST",
            "/v1/rewrite/commit",
            Some(json!({ "newKeyId": K1 })),
        ),
        ("POST", "/v1/rewrite/abort", Some(json!({ "newKeyId": K1 }))),
    ] {
        let refused = server.call_as(&reader, method, path, body).await;
        assert_eq!(
            (refused.status, code(&refused).as_str()),
            (403, "FORBIDDEN_ROLE"),
            "{method} {path}"
        );
    }
    server.call("POST", "/v1/rewrite", Some(migrate(K1))).await;
    let seen = server.call_as(&reader, "GET", "/v1/rewrite", None).await;
    assert_eq!(seen.status, 200);
    assert_eq!(seen.json()["newKeyId"], K1);
}

#[tokio::test]
async fn what_is_not_a_request_is_refused_as_one() {
    let server = TestServer::start().await;
    for (method, path, body, expected) in [
        (
            "PUT",
            "/v1/workspace/encryption",
            json!({ "keyId": "NOT-HEX", "header": {} }),
            "INVALID_ID",
        ),
        (
            "PUT",
            "/v1/workspace/encryption",
            json!({ "keyId": K1, "header": "not a document" }),
            "INVALID_REQUEST",
        ),
        (
            "PUT",
            "/v1/workspace/encryption",
            json!({ "keyId": K1, "header": {}, "words": "six of them" }),
            "INVALID_REQUEST",
        ),
        (
            "POST",
            "/v1/rewrite",
            json!({ "kind": "shuffle", "newKeyId": K1, "newHeader": {} }),
            "INVALID_REQUEST",
        ),
        (
            "POST",
            "/v1/rewrite",
            json!({ "kind": "rotate", "newKeyId": K1 }),
            "INVALID_REQUEST",
        ),
        ("POST", "/v1/rewrite/commit", json!({}), "INVALID_REQUEST"),
        (
            "POST",
            "/v1/rewrite/abort",
            json!({ "newKeyId": "" }),
            "INVALID_ID",
        ),
    ] {
        let refused = server.call(method, path, Some(body.clone())).await;
        assert_eq!(
            (refused.status, code(&refused)),
            (400, expected.to_owned()),
            "{method} {path} {body}"
        );
    }
    // Nothing of it changed anything.
    assert_eq!(
        server.get("/v1/workspace").await.json()["encryption"]["state"],
        "plaintext"
    );
}
