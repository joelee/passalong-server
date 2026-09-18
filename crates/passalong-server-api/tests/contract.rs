//! The contract against the code, and one workspace against another
//! (PLAN-00004, step 7).
//!
//! Every request here is built from the route table: the operation id picks
//! the method and the path. So a test that calls an operation calls it as
//! the table has it, and the table is compared with `openapi.json`.

mod common;

use std::cell::RefCell;
use std::collections::BTreeSet;

use common::{DAY, TestServer};
use passalong_server_api::ROUTES;
use passalong_server_api::client::Response;
use passalong_server_core::workspace::Role;
use serde_json::{Value, json};

fn key_id(letter: char) -> String {
    letter.to_string().repeat(64)
}

fn text(response: &Response) -> String {
    String::from_utf8_lossy(&response.body).into_owned()
}

#[test]
fn the_route_table_and_the_contract_hold_the_same_operations() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api/openapi.json");
    let document: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut contract = BTreeSet::new();
    for (path, operations) in document["paths"].as_object().unwrap() {
        for (method, operation) in operations.as_object().unwrap() {
            if let Some(id) = operation.get("operationId").and_then(Value::as_str) {
                contract.insert((id.to_owned(), method.to_uppercase(), path.clone()));
            }
        }
    }
    let table: BTreeSet<_> = ROUTES
        .iter()
        .map(|route| {
            (
                route.operation.to_owned(),
                route.method.to_owned(),
                route.path.to_owned(),
            )
        })
        .collect();
    assert_eq!(table.len(), ROUTES.len(), "an operation is listed twice");
    assert_eq!(table.len(), 26);
    let only_served: Vec<_> = table.difference(&contract).collect();
    let only_written: Vec<_> = contract.difference(&table).collect();
    assert!(
        only_served.is_empty() && only_written.is_empty(),
        "served and not in the contract: {only_served:?}; in the contract and not served: {only_written:?}"
    );
}

/// Calls operations by their id, and remembers which were answered with
/// success.
struct Caller<'a> {
    server: &'a TestServer,
    served: RefCell<BTreeSet<&'static str>>,
}

#[derive(Default)]
struct Call<'a> {
    /// `{id}`, `{uploadId}`, or `{contentKey}`: whichever the path has.
    target: &'a str,
    query: &'a str,
    json: Option<Value>,
    content: Option<&'a [u8]>,
}

impl<'a> Caller<'a> {
    fn new(server: &'a TestServer) -> Self {
        Self {
            server,
            served: RefCell::default(),
        }
    }

    async fn call(&self, token: &str, operation: &str, call: Call<'_>) -> Response {
        let route = ROUTES
            .iter()
            .find(|route| route.operation == operation)
            .unwrap_or_else(|| panic!("{operation} is not in the route table"));
        let mut path = route.path.to_owned();
        for parameter in ["{id}", "{uploadId}", "{contentKey}"] {
            path = path.replace(parameter, call.target);
        }
        assert!(!path.contains('{'), "{path}");
        path.push_str(call.query);
        let mut request = self.server.client().request(route.method, &path);
        if !path.ends_with('z') {
            request = request.bearer(token);
        }
        if let Some(json) = &call.json {
            request = request.json(json);
        }
        if let Some(content) = call.content {
            request = request.body(content.to_vec());
        }
        let response = request.send().await.unwrap();
        if response.status < 400 {
            self.served.borrow_mut().insert(route.operation);
        }
        response
    }

    /// As [`Self::call`], and it must succeed.
    async fn ok(&self, token: &str, operation: &str, call: Call<'_>) -> Response {
        let response = self.call(token, operation, call).await;
        assert!(
            response.status < 400,
            "{operation}: {} {}",
            response.status,
            text(&response)
        );
        response
    }

    /// Uploads a sealed item; its id.
    async fn put(&self, token: &str, id: &str, under: &str, staged: bool, content: &[u8]) {
        let body = json!({
            "id": id,
            "meta": { "schema": 2, "sealed": format!("meta-of-{id}") },
            "size": content.len().to_string(),
            "expectedKeyId": under,
            "inRewrite": staged,
        });
        let begun = self
            .ok(
                token,
                "beginUpload",
                Call {
                    json: Some(body),
                    ..Call::default()
                },
            )
            .await;
        let upload = begun.json()["uploadId"].as_str().unwrap().to_owned();
        self.ok(
            token,
            "putUploadContent",
            Call {
                target: &upload,
                content: Some(content),
                ..Call::default()
            },
        )
        .await;
        self.ok(
            token,
            "commitUpload",
            Call {
                target: &upload,
                ..Call::default()
            },
        )
        .await;
    }

    async fn begin(&self, token: &str, id: &str, under: &str, size: usize) -> String {
        let body = json!({
            "id": id,
            "meta": { "schema": 2, "sealed": format!("meta-of-{id}") },
            "size": size.to_string(),
            "expectedKeyId": under,
        });
        self.ok(
            token,
            "beginUpload",
            Call {
                json: Some(body),
                ..Call::default()
            },
        )
        .await
        .json()["uploadId"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn seal(key: &str, mark: &str) -> Option<Value> {
    Some(json!({ "keyId": key, "header": { "wrapped": mark } }))
}

fn rotate(old: &str, new: &str, mark: &str) -> Option<Value> {
    Some(
        json!({ "kind": "rotate", "expectedKeyId": old, "newKeyId": new, "newHeader": { "wrapped": mark } }),
    )
}

#[tokio::test]
async fn every_operation_is_served() {
    let server = TestServer::start().await;
    let caller = Caller::new(&server);
    let home = server.token.clone();
    let (k1, k2, k3) = (key_id('1'), key_id('2'), key_id('3'));
    let none = Call::default;

    caller.ok("", "healthz", none()).await;
    caller.ok("", "readyz", none()).await;
    caller.ok(&home, "getViewer", none()).await;
    caller.ok(&home, "probeWrite", none()).await;
    caller
        .ok(
            &home,
            "enableEncryption",
            Call {
                json: seal(&k1, "first"),
                ..none()
            },
        )
        .await;

    let item = "00000001-aaaaaaaaaaaa";
    caller
        .put(&home, item, &k1, false, b"sealed under one")
        .await;
    let dropped = caller.begin(&home, "00000002-bbbbbbbbbbbb", &k1, 4).await;
    caller
        .ok(
            &home,
            "abortUpload",
            Call {
                target: &dropped,
                ..none()
            },
        )
        .await;
    caller
        .ok(
            &home,
            "cleanStaging",
            Call {
                json: Some(json!({ "olderThanSecs": 0 })),
                ..none()
            },
        )
        .await;

    let listed = caller.ok(&home, "listItems", none()).await.json();
    assert_eq!(listed[0]["id"], item);
    let ids = caller.ok(&home, "listItemIds", none()).await.json();
    assert_eq!(ids, json!([item]));
    caller
        .ok(
            &home,
            "getItem",
            Call {
                target: item,
                ..none()
            },
        )
        .await;
    let content = caller
        .ok(
            &home,
            "getItemContent",
            Call {
                target: item,
                ..none()
            },
        )
        .await;
    assert_eq!(content.body, b"sealed under one");
    let resolved = caller
        .ok(
            &home,
            "resolveItem",
            Call {
                query: "?input=aaaa",
                ..none()
            },
        )
        .await;
    assert_eq!(resolved.json(), json!({ "result": "resolved", "id": item }));
    let found = caller
        .ok(
            &home,
            "findByContentKey",
            Call {
                target: "aaaaaaaaaaaa",
                ..none()
            },
        )
        .await;
    assert_eq!(found.json()["id"], item);

    // A rotation, from its beginning to its commit.
    caller
        .ok(
            &home,
            "beginRewrite",
            Call {
                json: rotate(&k1, &k2, "second"),
                ..none()
            },
        )
        .await;
    caller.ok(&home, "getRewrite", none()).await;
    caller.ok(&home, "heartbeatRewrite", none()).await;
    caller
        .put(&home, item, &k2, true, b"sealed under two!")
        .await;
    let staged = caller
        .ok(
            &home,
            "getItemContent",
            Call {
                target: item,
                query: "?partition=staged",
                ..none()
            },
        )
        .await;
    assert_eq!(staged.body, b"sealed under two!");
    let committed = caller
        .ok(
            &home,
            "commitRewrite",
            Call {
                json: Some(json!({ "newKeyId": k2 })),
                ..none()
            },
        )
        .await;
    assert_eq!(committed.json()["keyId"], k2.as_str());
    let content = caller
        .ok(
            &home,
            "getItemContent",
            Call {
                target: item,
                ..none()
            },
        )
        .await;
    assert_eq!(content.body, b"sealed under two!");

    let replaced = caller
        .ok(
            &home,
            "replaceHeader",
            Call {
                json: Some(json!({ "expectedKeyId": k2, "header": { "wrapped": "third" } })),
                ..none()
            },
        )
        .await;
    assert_eq!(replaced.json()["header"]["wrapped"], "third");

    // Another rotation, which its holder leaves; a second key of the
    // workspace takes it over once the lease has ended, stages what is
    // missing, and commits.
    let desktop = server
        .control()
        .create_key("home", "desktop", Role::ReadWrite, Some(DAY))
        .unwrap()
        .0
        .reveal();
    caller
        .ok(
            &home,
            "beginRewrite",
            Call {
                json: rotate(&k2, &k3, "fourth"),
                ..none()
            },
        )
        .await;
    server.clock.advance(server.config.rewrite.lease_secs + 1);
    caller.ok(&desktop, "takeOverRewrite", none()).await;
    caller
        .put(&desktop, item, &k3, true, b"sealed under three")
        .await;
    let committed = caller
        .ok(
            &desktop,
            "commitRewrite",
            Call {
                json: Some(json!({ "newKeyId": k3 })),
                ..none()
            },
        )
        .await;
    assert_eq!(committed.json()["keyId"], k3.as_str());
    assert_eq!(committed.json()["header"]["wrapped"], "fourth");

    // And one that is given up.
    let k4 = key_id('4');
    caller
        .ok(
            &home,
            "beginRewrite",
            Call {
                json: rotate(&k3, &k4, "fifth"),
                ..none()
            },
        )
        .await;
    let aborted = caller
        .ok(
            &home,
            "abortRewrite",
            Call {
                json: Some(json!({ "newKeyId": k4 })),
                ..none()
            },
        )
        .await;
    assert_eq!(aborted.json()["keyId"], k3.as_str());

    let query = format!("?expectedKeyId={k3}");
    caller
        .ok(
            &home,
            "deleteItem",
            Call {
                target: item,
                query: &query,
                ..none()
            },
        )
        .await;
    let workspace = caller.ok(&home, "getWorkspace", none()).await.json();
    assert_eq!(workspace["itemCount"], 0);

    // The other workspace starts afresh: its plaintext item stays, apart.
    let other = server.other_token.clone();
    let plain = common::honest(7, b"plain");
    let begun = server
        .call_as(
            &other,
            "POST",
            "/v1/uploads",
            Some(json!({ "id": plain.id, "meta": plain.meta, "size": "5" })),
        )
        .await;
    let upload = begun.json()["uploadId"].as_str().unwrap().to_owned();
    server.put_content(&other, &upload, b"plain").await;
    caller
        .ok(
            &other,
            "commitUpload",
            Call {
                target: &upload,
                ..none()
            },
        )
        .await;
    caller
        .ok(
            &other,
            "freshStart",
            Call {
                json: seal(&k1, "other"),
                ..none()
            },
        )
        .await;
    let kept = caller
        .ok(
            &other,
            "listItemIds",
            Call {
                query: "?partition=plain",
                ..none()
            },
        )
        .await;
    assert_eq!(kept.json(), json!([plain.id]));

    let served = caller.served.borrow();
    let missing: Vec<_> = ROUTES
        .iter()
        .map(|route| route.operation)
        .filter(|operation| !served.contains(operation))
        .collect();
    assert!(missing.is_empty(), "never served with success: {missing:?}");
}

/// What one workspace has that the other's key must never see or touch.
struct Theirs {
    token: String,
    data_key: String,
    next_key: String,
    item: &'static str,
    content_key: &'static str,
    upload: String,
    /// What must appear in no answer to the other key.
    secrets: Vec<String>,
}

/// A sealed workspace with an item, and an upload that awaits its commit.
async fn furnish(
    caller: &Caller<'_>,
    token: &str,
    name: &str,
    keys: (char, char),
    item: &'static str,
) -> Theirs {
    let (data_key, next_key) = (key_id(keys.0), key_id(keys.1));
    let mark = format!("header-of-{name}");
    caller
        .ok(
            token,
            "enableEncryption",
            Call {
                json: seal(&data_key, &mark),
                ..Call::default()
            },
        )
        .await;
    caller
        .put(token, item, &data_key, false, name.as_bytes())
        .await;
    // Other content than the item's, or there were nothing to upload.
    let pending = format!("{}-{}", &item[..8], "9".repeat(12)).replace("0000000", "0000001");
    let upload = caller.begin(token, &pending, &data_key, 3).await;
    caller
        .ok(
            token,
            "putUploadContent",
            Call {
                target: &upload,
                content: Some(b"abc"),
                ..Call::default()
            },
        )
        .await;
    let api_key = caller.ok(token, "getViewer", Call::default()).await.json()["key"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Theirs {
        token: token.to_owned(),
        secrets: vec![
            // Its name, as a JSON string: `other` alone is in many a sentence.
            format!("\"{name}\""),
            mark,
            data_key.clone(),
            next_key.clone(),
            item.to_owned(),
            pending,
            upload.clone(),
            api_key,
        ],
        data_key,
        next_key,
        item,
        content_key: &item[9..],
        upload,
    }
}

/// All that a workspace's own key sees of it.
async fn snapshot(caller: &Caller<'_>, theirs: &Theirs) -> Vec<String> {
    let mut seen = Vec::new();
    for (operation, call) in [
        ("getWorkspace", Call::default()),
        ("listItems", Call::default()),
        ("getRewrite", Call::default()),
        (
            "getItemContent",
            Call {
                target: theirs.item,
                ..Call::default()
            },
        ),
    ] {
        let answer = caller.call(&theirs.token, operation, call).await;
        seen.push(format!("{operation}: {} {}", answer.status, text(&answer)));
    }
    seen
}

/// Every operation of the route table, by `attacker`'s key, aimed at what
/// is `victim`'s. Whether the victim has a rewrite open is the caller's
/// business; this is run in both states.
async fn attack(caller: &Caller<'_>, attacker: &Theirs, victim: &Theirs) {
    let before = snapshot(caller, victim).await;
    let end = Some(json!({ "newKeyId": victim.next_key }));
    let expected = format!("?expectedKeyId={}", attacker.data_key);
    for route in ROUTES {
        let none = Call::default;
        // The call, and whether it may succeed. Where it may, it is an
        // operation of the attacker's own workspace that names nothing, and
        // what matters is that the answer holds nothing of the victim's.
        let (calls, may_succeed): (Vec<Call<'_>>, bool) = match route.operation {
            "healthz" | "readyz" | "getViewer" | "getWorkspace" | "probeWrite" => {
                (vec![none()], true)
            }
            "cleanStaging" => (
                vec![Call {
                    json: Some(json!({ "olderThanSecs": 0 })),
                    ..none()
                }],
                true,
            ),
            "listItems" | "listItemIds" => (
                vec![
                    none(),
                    Call {
                        query: "?partition=plain",
                        ..none()
                    },
                    Call {
                        query: "?after=00000000-000000000000",
                        ..none()
                    },
                ],
                true,
            ),
            "resolveItem" => (
                vec![
                    Call {
                        query: &format!("?input={}", victim.item),
                        ..none()
                    }
                    .leak(),
                    Call {
                        query: &format!("?input={}", victim.content_key),
                        ..none()
                    }
                    .leak(),
                ],
                true,
            ),
            "findByContentKey" => (
                vec![Call {
                    target: victim.content_key,
                    ..none()
                }],
                false,
            ),
            "getItem" => (
                vec![Call {
                    target: victim.item,
                    ..none()
                }],
                false,
            ),
            "getItemContent" => (
                vec![
                    Call {
                        target: victim.item,
                        ..none()
                    },
                    Call {
                        target: victim.item,
                        query: "?partition=plain",
                        ..none()
                    },
                    Call {
                        target: victim.item,
                        query: "?partition=staged",
                        ..none()
                    },
                ],
                false,
            ),
            "deleteItem" => (
                vec![
                    Call {
                        target: victim.item,
                        query: &expected,
                        ..none()
                    }
                    .leak(),
                ],
                false,
            ),
            "putUploadContent" => (
                vec![Call {
                    target: &victim.upload,
                    content: Some(b"xyz"),
                    ..none()
                }],
                false,
            ),
            "commitUpload" => (
                vec![Call {
                    target: &victim.upload,
                    ..none()
                }],
                false,
            ),
            // Aborting is answered alike for an upload that is gone and for
            // one that never was, so the answer tells nothing. That it did
            // nothing is seen at the end: the victim's upload still commits.
            "abortUpload" => (
                vec![Call {
                    target: &victim.upload,
                    ..none()
                }],
                true,
            ),
            // An upload for the victim's rewrite, under the victim's keys.
            "beginUpload" => (
                vec![
                    Call {
                        json: Some(
                            json!({ "id": victim.item, "meta": {}, "size": "1", "expectedKeyId": victim.next_key, "inRewrite": true }),
                        ),
                        ..none()
                    },
                    Call {
                        json: Some(
                            json!({ "id": victim.item, "meta": {}, "size": "1", "expectedKeyId": victim.data_key }),
                        ),
                        ..none()
                    },
                ],
                false,
            ),
            "enableEncryption" | "freshStart" => (
                vec![Call {
                    json: seal(&victim.data_key, "taken"),
                    ..none()
                }],
                false,
            ),
            "replaceHeader" => (
                vec![Call {
                    json: Some(
                        json!({ "expectedKeyId": victim.data_key, "header": { "wrapped": "taken" } }),
                    ),
                    ..none()
                }],
                false,
            ),
            "beginRewrite" => (
                vec![Call {
                    json: rotate(&victim.data_key, &victim.next_key, "taken"),
                    ..none()
                }],
                false,
            ),
            "getRewrite" | "heartbeatRewrite" | "takeOverRewrite" => (vec![none()], false),
            "commitRewrite" => (
                vec![Call {
                    json: end.clone(),
                    ..none()
                }],
                false,
            ),
            // With no session open in the attacker's own workspace, an abort
            // answers that workspace's state, so that it may be repeated.
            "abortRewrite" => (
                vec![Call {
                    json: end.clone(),
                    ..none()
                }],
                true,
            ),
            other => panic!("{other} has no isolation case: write one"),
        };
        for call in calls {
            let answer = caller.call(&attacker.token, route.operation, call).await;
            let body = text(&answer);
            if !may_succeed {
                assert!(
                    (400..500).contains(&answer.status),
                    "{}: {} {body}",
                    route.operation,
                    answer.status
                );
            }
            for secret in &victim.secrets {
                assert!(
                    !body.contains(secret.as_str()),
                    "{} tells of {secret}: {body}",
                    route.operation
                );
            }
        }
    }
    assert_eq!(snapshot(caller, victim).await, before, "something changed");
    // The victim's upload is as it was, too: it still commits. It is begun
    // again at once, so that the next attack finds one.
}

impl<'a> Call<'a> {
    /// For a query made on the spot: tests may leak a few bytes.
    fn leak(self) -> Call<'static> {
        Call {
            target: Box::leak(self.target.to_owned().into_boxed_str()),
            query: Box::leak(self.query.to_owned().into_boxed_str()),
            json: self.json,
            content: None,
        }
    }
}

#[tokio::test]
async fn no_key_sees_or_touches_another_workspace() {
    let server = TestServer::start().await;
    let caller = Caller::new(&server);
    let home = furnish(
        &caller,
        &server.token,
        "home",
        ('a', 'b'),
        "00000001-aaaaaaaaaaaa",
    )
    .await;
    let other = furnish(
        &caller,
        &server.other_token,
        "other",
        ('c', 'd'),
        "00000002-cccccccccccc",
    )
    .await;

    // Neither has a rewrite open.
    attack(&caller, &other, &home).await;
    attack(&caller, &home, &other).await;

    // Each in turn has one, with an item staged, and the lease run out: a
    // session anyone of the workspace could take over, and nobody else.
    for (victim, attacker) in [(&home, &other), (&other, &home)] {
        caller
            .ok(
                &victim.token,
                "beginRewrite",
                Call {
                    json: rotate(&victim.data_key, &victim.next_key, "next"),
                    ..Call::default()
                },
            )
            .await;
        caller
            .put(
                &victim.token,
                victim.item,
                &victim.next_key,
                true,
                b"staged",
            )
            .await;
        server.clock.advance(server.config.rewrite.lease_secs + 1);
        attack(&caller, attacker, victim).await;
        caller
            .ok(
                &victim.token,
                "abortRewrite",
                Call {
                    json: Some(json!({ "newKeyId": victim.next_key })),
                    ..Call::default()
                },
            )
            .await;
    }

    // After all that, each one's pending upload still commits.
    for theirs in [&home, &other] {
        caller
            .ok(
                &theirs.token,
                "commitUpload",
                Call {
                    target: &theirs.upload,
                    ..Call::default()
                },
            )
            .await;
    }
}
