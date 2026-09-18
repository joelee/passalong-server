//! Nothing of an item reaches the logs (PLAN-00002, AC-09; `AGENTS.md`,
//! "Observability"): not its `meta`, not its content, not a header.

use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use passalong_server_core::clock::ManualClock;
use passalong_server_core::ids::{ApiKeyId, ItemId, KeyId};
use passalong_server_core::ledger::{SqliteLedger, WorkspaceId};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::shelf::{FsShelf, ItemShelf};
use passalong_server_core::upload::{Begun, UploadRequest};
use passalong_server_core::workspace::{Caller, Engine, Limits, Role};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// Keeps every event as text: target, level, and all fields.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut line = format!("{} {}", event.metadata().level(), event.metadata().target());
        event.record(&mut Fields(&mut line));
        self.0.lock().unwrap().push(line);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

const META: &[u8] = br#"{"name":"SECRET-META.txt"}"#;
const CONTENT: &[u8] = b"SECRET-CONTENT of the clipboard";
const HEADER: &[u8] = b"SECRET-HEADER wrapped key";

#[test]
fn a_session_with_warnings_and_errors_logs_ids_and_nothing_of_an_item() {
    let capture = Capture::default();
    let dir = tempfile::tempdir().unwrap();
    let item = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();

    tracing::subscriber::with_default(capture.clone(), || {
        let open = || {
            Engine::open(
                FsShelf::open(dir.path().join("ws")).unwrap(),
                SqliteLedger::open(
                    dir.path().join("control.sqlite"),
                    Duration::from_millis(100),
                )
                .unwrap(),
                WorkspaceId::parse("00000000000000aa").unwrap(),
                Arc::new(ManualClock::at(1_000)),
                Box::new(SeededRandom::new(1)),
                Limits::default(),
            )
            .unwrap()
        };
        let a = Caller::new(ApiKeyId::new("laptop"), Role::ReadWrite);
        let mut engine = open();
        engine
            .enable_encryption(&a, KeyId::parse("aa").unwrap(), HEADER.to_vec())
            .unwrap();
        let request = UploadRequest {
            id: item.clone(),
            meta: META.to_vec(),
            size: CONTENT.len() as u64,
            expected_key_id: Some(KeyId::parse("aa").unwrap()),
            in_rewrite: false,
        };
        let Begun::Ticket(ticket) = engine.begin_upload(&a, request).unwrap() else {
            panic!("already stored");
        };
        engine
            .put_upload_content(&a, &ticket.upload_id, &mut Cursor::new(CONTENT.to_vec()))
            .unwrap();
        engine.commit_upload(&a, &ticket.upload_id).unwrap();

        // An unclean stop: rubbish on the shelf, which the next opening
        // repairs and reports.
        let orphan = passalong_server_core::ids::UploadId::generate(&mut SeededRandom::new(9));
        engine.shelf().stage_create(&orphan).unwrap();
        engine
            .shelf()
            .stage_write(&orphan, &mut Cursor::new(CONTENT.to_vec()), 100)
            .unwrap();
        drop(engine);
        drop(open());

        // And an error: a control database that is not one.
        std::fs::write(dir.path().join("broken.sqlite"), CONTENT.repeat(40)).unwrap();
        SqliteLedger::open(dir.path().join("broken.sqlite"), Duration::from_millis(100))
            .unwrap_err();
    });

    let lines = capture.0.lock().unwrap().clone();
    let all = lines.join("\n");
    // The session did log: a publish, a repair, and an error.
    assert!(all.contains(item.as_str()), "{all}");
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("WARN") && line.contains("reconcile")),
        "{all}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("ERROR") && line.contains("ledger")),
        "{all}"
    );
    for secret in ["SECRET-META", "SECRET-CONTENT", "SECRET-HEADER"] {
        assert!(!all.contains(secret), "`{secret}` reached the log:\n{all}");
    }
}
