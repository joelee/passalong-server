//! `docs/api/openapi.json` and `docs/api/README.md` describe one API
//! (PLAN-00001, REQ-01). Until the routes exist in code and the document is
//! exported from them, this test keeps the two from drifting apart.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

const METHODS: [&str; 4] = ["get", "post", "put", "delete"];

fn read(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/api")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn document() -> Value {
    serde_json::from_str(&read("openapi.json")).expect("openapi.json is JSON")
}

/// Every operation: its id, its method, and its object.
fn operations(document: &Value) -> Vec<(String, String, Value)> {
    let mut found = Vec::new();
    for (path, item) in document["paths"].as_object().expect("paths") {
        for method in METHODS {
            if let Some(operation) = item.get(method) {
                let id = operation["operationId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
                found.push((id.to_owned(), method.to_owned(), operation.clone()));
            }
        }
    }
    found
}

/// The text between the first pair of backticks of a table cell.
fn ticked(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The operation ids of the README's route tables: rows whose second cell
/// is a method and a path.
fn readme_operations(readme: &str) -> BTreeSet<String> {
    readme
        .lines()
        .filter_map(|line| {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let route = ticked(cells.get(2)?).into_iter().next()?;
            let is_route = METHODS
                .iter()
                .any(|m| route.starts_with(&format!("{} /", m.to_uppercase())));
            is_route
                .then(|| ticked(cells[1]).into_iter().next())
                .flatten()
        })
        .collect()
}

/// The codes of the README's error table.
fn readme_error_codes(readme: &str) -> BTreeSet<String> {
    let table = readme
        .split("## Error codes")
        .nth(1)
        .expect("an `Error codes` section");
    let table = table.split("\n## ").next().unwrap();
    table
        .lines()
        .filter(|line| line.starts_with("| `"))
        .flat_map(|line| ticked(line.split('|').nth(1).unwrap()))
        .collect()
}

#[test]
fn it_is_an_openapi_3_1_document() {
    let document = document();
    assert!(document["openapi"].as_str().unwrap().starts_with("3.1"));
    assert!(document["info"]["version"].is_string());
    let bearer = &document["components"]["securitySchemes"]["apiKey"];
    assert_eq!(bearer["scheme"], "bearer");
}

#[test]
fn its_operations_are_those_of_the_readme() {
    let ours: BTreeSet<String> = operations(&document())
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let theirs = readme_operations(&read("README.md"));
    assert!(
        theirs.len() >= 20,
        "the README's route tables were not found: {theirs:?}"
    );
    assert_eq!(ours, theirs);
}

#[test]
fn operation_ids_are_unique() {
    let all = operations(&document());
    let unique: BTreeSet<&String> = all.iter().map(|(id, _, _)| id).collect();
    assert_eq!(all.len(), unique.len());
}

#[test]
fn every_operation_that_is_not_a_get_says_what_a_replay_gets() {
    for (id, method, operation) in operations(&document()) {
        if method == "get" {
            continue;
        }
        let replay = operation["x-passalong-replay"].as_str().unwrap_or("");
        assert!(replay.len() > 10, "{id} has no x-passalong-replay");
    }
}

#[test]
fn every_operation_but_the_health_checks_needs_a_key() {
    let document = document();
    assert_eq!(document["security"][0]["apiKey"], serde_json::json!([]));
    for (id, _, operation) in operations(&document) {
        let open = operation["security"] == serde_json::json!([]);
        assert_eq!(open, id == "healthz" || id == "readyz", "{id}");
    }
}

#[test]
fn its_error_codes_are_those_of_the_readme_and_of_the_code() {
    let document = document();
    let ours: BTreeSet<String> = document["components"]["schemas"]["ErrorCode"]["enum"]
        .as_array()
        .expect("components.schemas.ErrorCode.enum")
        .iter()
        .map(|code| code.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ours, readme_error_codes(&read("README.md")));

    use passalong_server_core::error::ApiError;
    for err in [
        ApiError::ForbiddenRole,
        ApiError::KeyIdMismatch,
        ApiError::RewriteInProgress,
        ApiError::LeaseHeld,
        ApiError::RewriteIncomplete {
            staged: 0,
            source: 1,
        },
        ApiError::RewriteEnded,
        ApiError::QuotaExceeded,
        ApiError::ItemTooLarge,
        ApiError::ContentMismatch,
        ApiError::NotFound,
        ApiError::InvalidId(String::new()),
        ApiError::InvalidRequest(String::new()),
    ] {
        assert!(
            ours.contains(err.code()),
            "{} is not in openapi.json",
            err.code()
        );
    }
}

#[test]
fn no_response_carries_a_field_taken_from_meta() {
    // The envelope (IDEA-00001-R03-INFO-02): `meta` is opaque, and nothing
    // the client keeps inside it may appear as a field of the API.
    let text = read("openapi.json");
    let document = document();
    let item = &document["components"]["schemas"]["Item"]["properties"];
    let fields: BTreeSet<&str> = item
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields,
        BTreeSet::from(["id", "meta", "storedBytes", "receivedAt"])
    );
    for forbidden in [
        "\"preview\"",
        "\"mime\"",
        "\"sha256\"",
        "\"device\"",
        "\"origin\"",
    ] {
        assert!(!text.contains(forbidden), "openapi.json names {forbidden}");
    }
}

#[test]
fn staged_items_can_be_read_back_and_not_deleted() {
    let document = document();
    let partitions = |path: &str, method: &str| -> Vec<String> {
        document["paths"][path][method]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == "partition")
            .unwrap_or_else(|| panic!("{method} {path} has no partition"))["schema"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect()
    };
    for path in [
        "/v1/items",
        "/v1/item-ids",
        "/v1/items/{id}",
        "/v1/items/{id}/content",
    ] {
        assert_eq!(
            partitions(path, "get"),
            ["current", "plain", "staged"],
            "{path}"
        );
        let errors = document["paths"][path]["get"]["x-passalong-errors"].to_string();
        assert!(
            errors.contains("LEASE_HELD"),
            "{path} cannot refuse a non-holder"
        );
    }
    assert_eq!(partitions("/v1/items/{id}", "delete"), ["current", "plain"]);
}

#[test]
fn content_longer_than_announced_is_refused_as_it_arrives() {
    // The shelf refuses it mid-stream (PLAN-00002), so the operation that
    // carries content must be able to say so, not only the commit.
    let document = document();
    for path in [
        "/v1/uploads/{uploadId}/content",
        "/v1/uploads/{uploadId}/commit",
    ] {
        let method = if path.ends_with("content") {
            "put"
        } else {
            "post"
        };
        let errors = document["paths"][path][method]["x-passalong-errors"].to_string();
        assert!(
            errors.contains("CONTENT_MISMATCH"),
            "{method} {path}: {errors}"
        );
    }
}
