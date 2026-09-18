//! Items and uploads over HTTP, and the bridge between asynchronous bodies
//! and the synchronous core (PLAN-00004, step 5: the checkpoint).

mod common;

use std::time::Duration;

use common::{TestServer, honest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn an_item_is_uploaded_listed_read_and_deleted() {
    let server = TestServer::start().await;
    let item = honest(1, b"hello");
    let committed = server.upload(&item).await;
    assert_eq!(committed.status, 200);
    let outcome = committed.json();
    assert_eq!(outcome["created"], true);
    assert_eq!(outcome["item"]["id"], item.id.as_str());
    assert_eq!(outcome["item"]["storedBytes"], "5");
    assert_eq!(outcome["item"]["meta"], item.meta);
    assert!(
        outcome["item"]["receivedAt"]
            .as_str()
            .unwrap()
            .ends_with('Z')
    );
    // The envelope, and nothing taken out of `meta`.
    let mut fields: Vec<&str> = outcome["item"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["id", "meta", "receivedAt", "storedBytes"]);

    let second = honest(2, b"world!");
    server.upload(&second).await;
    let listed = server.get("/v1/items").await.json();
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [second.id.as_str(), item.id.as_str()], "newest first");
    assert_eq!(
        server
            .get(&format!("/v1/item-ids?after={}", item.id))
            .await
            .json(),
        serde_json::json!([second.id])
    );
    assert_eq!(
        server
            .get(&format!("/v1/items?after={}", second.id))
            .await
            .json(),
        serde_json::json!([])
    );
    assert_eq!(
        server.get(&format!("/v1/items/{}", item.id)).await.json()["meta"],
        item.meta
    );
    assert_eq!(
        server
            .get(&format!("/v1/content-keys/{}", &item.id[9..]))
            .await
            .json()["id"],
        item.id.as_str()
    );
    assert_eq!(
        server.get("/v1/content-keys/000000000000").await.status,
        404
    );

    let resolved = server
        .get(&format!("/v1/items/resolve?input={}", &item.id[9..14]))
        .await
        .json();
    assert_eq!(
        (resolved["result"].as_str(), resolved["id"].as_str()),
        (Some("resolved"), Some(item.id.as_str()))
    );
    assert_eq!(
        server.get("/v1/items/resolve?input=zz").await.json()["result"],
        "invalidPrefix"
    );
    assert_eq!(
        server.get("/v1/items/resolve?input=ffffffff").await.json()["result"],
        "notFound"
    );

    let content = server.get(&format!("/v1/items/{}/content", item.id)).await;
    assert_eq!(
        (content.status, content.body.as_slice()),
        (200, &b"hello"[..])
    );
    assert_eq!(
        content.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(content.header("content-length"), Some("5"));
    assert_eq!(content.header("accept-ranges"), Some("bytes"));

    let workspace = server.get("/v1/workspace").await.json();
    assert_eq!(
        (
            workspace["itemCount"].as_u64(),
            workspace["usedBytes"].as_str()
        ),
        (Some(2), Some("11"))
    );

    let deleted = server
        .call("DELETE", &format!("/v1/items/{}", item.id), None)
        .await;
    assert_eq!(
        (
            deleted.status,
            deleted.json()["id"].as_str().map(str::to_owned)
        ),
        (200, Some(item.id.clone()))
    );
    // Deleting what is gone is NOT_FOUND, which a client takes as done.
    assert_eq!(
        server
            .call("DELETE", &format!("/v1/items/{}", item.id), None)
            .await
            .json()["code"],
        "NOT_FOUND"
    );
    assert_eq!(
        server.get(&format!("/v1/items/{}", item.id)).await.status,
        404
    );
    assert_eq!(
        server
            .get(&format!("/v1/items/{}/content", item.id))
            .await
            .status,
        404
    );
    for bad in [
        "/v1/items/not-an-id",
        "/v1/items/NOT-AN-ID/content",
        "/v1/uploads/zz/commit",
    ] {
        let method = if bad.contains("uploads") {
            "POST"
        } else {
            "GET"
        };
        assert_eq!(
            server.call(method, bad, None).await.json()["code"],
            "INVALID_ID",
            "{bad}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn meta_comes_back_byte_for_byte() {
    // The server keeps `meta` as the bytes the client sent: odd spacing, key
    // order, and all. On disk it is the client's `meta.json`.
    let server = TestServer::start().await;
    let item = honest(1, b"hello");
    let sha = item.meta["sha256"].as_str().unwrap();
    let meta =
        format!("{{  \"size\":5,\"zeta\" : [1,  2],\n \"sha256\":\"{sha}\" , \"schema\":1}}");
    let body = format!(
        "{{\"id\":\"{}\",\"size\":\"5\",\"expectedKeyId\":null,\"meta\":{meta}}}",
        item.id
    );
    let begun = server
        .client()
        .request("POST", "/v1/uploads")
        .bearer(&server.token)
        .header("Content-Type", "application/json")
        .body_raw(body.into_bytes())
        .send()
        .await
        .unwrap();
    assert_eq!(
        begun.status,
        201,
        "{}",
        String::from_utf8_lossy(&begun.body)
    );
    let upload = begun.json()["uploadId"].as_str().unwrap().to_owned();
    server.put_content(&server.token, &upload, b"hello").await;
    let committed = server
        .call("POST", &format!("/v1/uploads/{upload}/commit"), None)
        .await;
    assert!(
        String::from_utf8_lossy(&committed.body).contains(&meta),
        "{}",
        String::from_utf8_lossy(&committed.body)
    );
    server.shutdown().await;
}

#[tokio::test]
async fn every_upload_request_may_be_sent_again() {
    let server = TestServer::start().await;
    let item = honest(1, b"hello");
    let first = server.begin(&item).await.json();
    let again = server.begin(&item).await;
    assert_eq!(
        (again.status, again.json()),
        (201, first.clone()),
        "the same ticket"
    );
    let upload = first["uploadId"].as_str().unwrap();
    assert!(first["expiresAt"].as_str().unwrap().ends_with('Z'));

    assert_eq!(
        server
            .put_content(&server.token, upload, b"hello")
            .await
            .status,
        204
    );
    assert_eq!(
        server
            .put_content(&server.token, upload, b"hello")
            .await
            .status,
        204,
        "content sent again replaces"
    );
    let commit = format!("/v1/uploads/{upload}/commit");
    let outcome = server.call("POST", &commit, None).await.json();
    assert_eq!(outcome["created"], true);
    assert_eq!(
        server.call("POST", &commit, None).await.json(),
        outcome,
        "the same outcome, with the original `created`"
    );
    assert_eq!(
        server
            .put_content(&server.token, upload, b"hello")
            .await
            .status,
        204,
        "after the commit: acknowledged, discarded"
    );

    // Content that is there already needs no upload at all.
    let same = honest(2, b"hello");
    let stored = server.begin(&same).await;
    assert_eq!(stored.status, 200);
    assert_eq!(
        (
            stored.json()["created"].as_bool(),
            stored.json()["item"]["id"].as_str().map(str::to_owned)
        ),
        (Some(false), Some(item.id.clone()))
    );

    let abandoned = server.begin(&honest(3, b"never")).await.json()["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    for _ in 0..2 {
        assert_eq!(
            server
                .call("DELETE", &format!("/v1/uploads/{abandoned}"), None)
                .await
                .status,
            204
        );
    }
    assert_eq!(
        server
            .call("POST", &format!("/v1/uploads/{abandoned}/commit"), None)
            .await
            .json()["code"],
        "NOT_FOUND"
    );
    // Another key's upload is not this key's to touch, or to know of.
    let theirs = server.call_as(&server.other_token, "POST", "/v1/uploads", Some(serde_json::json!({ "id": honest(9, b"x").id, "meta": honest(9, b"x").meta, "size": "1" }))).await.json();
    let theirs = theirs["uploadId"].as_str().unwrap();
    assert_eq!(
        server.put_content(&server.token, theirs, b"x").await.json()["code"],
        "NOT_FOUND"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn content_that_is_not_what_was_announced_is_refused() {
    let server = TestServer::start().await;
    let item = honest(1, b"hello");
    let upload = server.begin(&item).await.json()["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();
    // Another length than announced: refused from the headers, unread.
    let long = server.put_content(&server.token, &upload, b"hello!").await;
    assert_eq!(
        (long.status, long.json()["code"].as_str().map(str::to_owned)),
        (422, Some("CONTENT_MISMATCH".to_owned()))
    );
    // The right length and the wrong bytes: refused at the commit.
    assert_eq!(
        server
            .put_content(&server.token, &upload, b"hullo")
            .await
            .status,
        204
    );
    let refused = server
        .call("POST", &format!("/v1/uploads/{upload}/commit"), None)
        .await;
    assert_eq!(refused.json()["code"], "CONTENT_MISMATCH");
    // And the upload can be repeated with the right content.
    assert_eq!(
        server
            .put_content(&server.token, &upload, b"hello")
            .await
            .status,
        204
    );
    assert_eq!(
        server
            .call("POST", &format!("/v1/uploads/{upload}/commit"), None)
            .await
            .json()["created"],
        true
    );
    server.shutdown().await;
}

#[tokio::test]
async fn limits_are_enforced_when_an_upload_begins() {
    let server = TestServer::start_with(
        "[limits]\nmax_item_bytes = \"1 KiB\"\nworkspace_quota_bytes = \"2 KiB\"\n",
        |_| {},
    )
    .await;
    assert_eq!(
        server.get("/v1/viewer").await.json()["server"]["maxItemBytes"],
        "1024"
    );
    let big = honest(1, &vec![7; 1025]);
    let refused = server.begin(&big).await;
    assert_eq!(
        (
            refused.status,
            refused.json()["code"].as_str().map(str::to_owned)
        ),
        (413, Some("ITEM_TOO_LARGE".to_owned()))
    );
    for ts in 1..=2 {
        assert_eq!(
            server
                .upload(&honest(ts, &vec![ts as u8; 1024]))
                .await
                .json()["created"],
            true
        );
    }
    assert_eq!(
        server.begin(&honest(3, b"one more")).await.json()["code"],
        "QUOTA_EXCEEDED"
    );

    // A JSON body far beyond what any request needs is refused unread.
    let huge = serde_json::json!({ "id": big.id, "meta": { "padding": "x".repeat(600_000) }, "size": "1" });
    assert_eq!(
        server.call("POST", "/v1/uploads", Some(huge)).await.status,
        413
    );
    // And what is not the contract's JSON is a plain refusal.
    for body in [
        serde_json::json!({}),
        serde_json::json!({ "id": "nope", "meta": {}, "size": "1" }),
        serde_json::json!({ "id": big.id, "meta": {}, "size": 5 }),
    ] {
        let response = server.call("POST", "/v1/uploads", Some(body)).await;
        assert_eq!(
            response.status,
            400,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn a_range_is_one_part_of_the_content() {
    let server = TestServer::start().await;
    let item = honest(1, b"0123456789");
    server.upload(&item).await;
    let path = format!("/v1/items/{}/content", item.id);
    let range = |value: &'static str| {
        let (client, token, path) = (server.client(), server.token.clone(), path.clone());
        async move {
            client
                .request("GET", &path)
                .bearer(&token)
                .header("Range", value)
                .send()
                .await
                .unwrap()
        }
    };
    for (asked, bytes, content_range) in [
        ("bytes=0-3", &b"0123"[..], "bytes 0-3/10"),
        ("bytes=4-6", b"456", "bytes 4-6/10"),
        ("bytes=7-", b"789", "bytes 7-9/10"),
        ("bytes=-2", b"89", "bytes 8-9/10"),
        ("bytes=8-99", b"89", "bytes 8-9/10"),
    ] {
        let part = range(asked).await;
        assert_eq!((part.status, part.body.as_slice()), (206, bytes), "{asked}");
        assert_eq!(part.header("content-range"), Some(content_range), "{asked}");
        assert_eq!(
            part.header("content-length"),
            Some(bytes.len().to_string().as_str()),
            "{asked}"
        );
    }
    let beyond = range("bytes=10-").await;
    assert_eq!(
        (beyond.status, beyond.header("content-range")),
        (416, Some("bytes */10"))
    );
    // Several ranges, or another unit: the whole item, which HTTP allows.
    for whole in ["bytes=0-1,4-5", "lines=1-2", "bytes=x-y"] {
        let all = range(whole).await;
        assert_eq!(
            (all.status, all.body.as_slice()),
            (200, &b"0123456789"[..]),
            "{whole}"
        );
    }
    server.shutdown().await;
}

#[tokio::test]
async fn sixty_four_mebibytes_pass_through_in_bounded_pieces() {
    let server = TestServer::start().await;
    let content: Vec<u8> = (0..64_u32 << 20).map(|i| (i % 251) as u8).collect();
    let item = honest(1, &content);
    assert_eq!(server.upload(&item).await.json()["created"], true);
    let back = server.get(&format!("/v1/items/{}/content", item.id)).await;
    assert_eq!(back.body.len(), content.len());
    assert!(back.body == content, "the content came back changed");
    // What the bridge held at most at any moment, in either direction: the
    // bound of its channel times its largest piece. Not the item.
    let peak = server.bridge.peak_bytes();
    assert!(
        peak > 0 && peak <= 1 << 20,
        "the bridge held {peak} bytes at once"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_slow_upload_holds_nobody_up_and_a_broken_one_can_be_repeated() {
    let server = TestServer::start().await;
    let item = honest(1, &vec![42; 300_000]);
    let upload = server.begin(&item).await.json()["uploadId"]
        .as_str()
        .unwrap()
        .to_owned();

    // Half the content, and then nothing: a client on a bad link.
    let mut slow = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let head = format!(
        "PUT /v1/uploads/{upload}/content HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\nContent-Length: 300000\r\n\r\n",
        server.token
    );
    slow.write_all(head.as_bytes()).await.unwrap();
    slow.write_all(&item.content[..150_000]).await.unwrap();
    slow.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The workspace answers others meanwhile: no lock is held for content.
    let other = honest(2, b"quick");
    let quick = tokio::time::timeout(Duration::from_secs(5), server.upload(&other))
        .await
        .expect("held up by the slow upload");
    assert_eq!(quick.json()["created"], true);
    assert_eq!(
        server.get("/v1/item-ids").await.json(),
        serde_json::json!([other.id])
    );

    // The slow client goes away. Its upload is not an item, and is not lost
    // either: the same ticket takes the content again.
    drop(slow);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        server
            .call("POST", &format!("/v1/uploads/{upload}/commit"), None)
            .await
            .json()["code"],
        "CONTENT_MISMATCH"
    );
    assert_eq!(
        server
            .put_content(&server.token, &upload, &item.content)
            .await
            .status,
        204
    );
    assert_eq!(
        server
            .call("POST", &format!("/v1/uploads/{upload}/commit"), None)
            .await
            .json()["created"],
        true
    );

    // An answer that is read slowly is nobody else's problem either.
    let mut reader = tokio::net::TcpStream::connect(server.addr).await.unwrap();
    let get = format!(
        "GET /v1/items/{}/content HTTP/1.1\r\nHost: t\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
        item.id, server.token
    );
    reader.write_all(get.as_bytes()).await.unwrap();
    let mut some = [0_u8; 64];
    reader.read_exact(&mut some).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server.get("/v1/workspace"))
            .await
            .unwrap()
            .status,
        200
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_read_only_key_reads_and_does_not_write() {
    let server = TestServer::start().await;
    let item = honest(1, b"hello");
    server.upload(&item).await;
    let kiosk = server
        .control()
        .create_key(
            "home",
            "kiosk",
            passalong_server_core::workspace::Role::ReadOnly,
            None,
        )
        .unwrap()
        .0
        .reveal();
    assert_eq!(
        server
            .call_as(
                &kiosk,
                "GET",
                &format!("/v1/items/{}/content", item.id),
                None
            )
            .await
            .body,
        b"hello"
    );
    assert_eq!(
        server
            .call_as(&kiosk, "GET", "/v1/viewer", None)
            .await
            .json()["key"]["role"],
        "readOnly"
    );
    let body =
        serde_json::json!({ "id": honest(2, b"x").id, "meta": honest(2, b"x").meta, "size": "1" });
    for (method, path, body) in [
        ("POST", "/v1/uploads".to_owned(), Some(body)),
        ("DELETE", format!("/v1/items/{}", item.id), None),
        ("POST", "/v1/workspace/probe".to_owned(), None),
    ] {
        let refused = server.call_as(&kiosk, method, &path, body).await;
        assert_eq!(
            (
                refused.status,
                refused.json()["code"].as_str().map(str::to_owned)
            ),
            (403, Some("FORBIDDEN_ROLE".to_owned())),
            "{method} {path}"
        );
    }
    assert_eq!(
        server
            .call("POST", "/v1/workspace/probe", None)
            .await
            .status,
        204
    );
    let cleaned = server
        .call(
            "POST",
            "/v1/workspace/clean-staging",
            Some(serde_json::json!({ "olderThanSecs": 0 })),
        )
        .await;
    assert_eq!(
        (cleaned.status, cleaned.json()["removed"].as_u64()),
        (200, Some(0))
    );
    server.shutdown().await;
}
