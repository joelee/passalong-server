//! Several processes, one workspace (PLAN-00002, REQ-08). The operations
//! CLI will be such a second process beside the server.

#![cfg(feature = "fault-injection")]

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use passalong_server_core::clock::ManualClock;
use passalong_server_core::ledger::{SqliteLedger, WorkspaceId};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::shelf::{FsShelf, ItemShelf};
use passalong_server_core::workspace::{EncryptionState, Engine, Limits, Partition};

const CHILD: &str = env!("CARGO_BIN_EXE_storage_child");
const WORKERS: [&str; 3] = ["ann", "bob", "cyd"];
const UPLOADS: u32 = 40;

#[test]
fn three_uploaders_and_a_rewriter_share_one_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let mut children = Vec::new();
    for name in WORKERS {
        let mut command = Command::new(CHILD);
        command.args(["worker", path, name, &UPLOADS.to_string()]);
        children.push((name, command.stderr(Stdio::piped()).spawn().unwrap()));
    }
    let mut command = Command::new(CHILD);
    command.args(["rewriter", path, "15"]);
    children.push(("rewriter", command.stderr(Stdio::piped()).spawn().unwrap()));

    for (name, child) in children {
        let output = child.wait_with_output().unwrap();
        // Exit code 3 would be a refusal the child did not expect; anything
        // else a panic or a SQLite error that got through.
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let engine = Engine::open(
        FsShelf::open(dir.path().join("workspaces").join("00000000000000aa")).unwrap(),
        SqliteLedger::open(dir.path().join("control.sqlite"), Duration::from_secs(20)).unwrap(),
        WorkspaceId::parse("00000000000000aa").unwrap(),
        Arc::new(ManualClock::at(1_000)),
        Box::new(SeededRandom::new(1)),
        Limits {
            // Made-up ids: the content check has tests of its own.
            check_plaintext_content: false,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(
        engine.encryption().unwrap().state,
        EncryptionState::Plaintext
    );
    // Half of each worker's texts are everybody's, and are stored once.
    let shared = UPLOADS / 2;
    let own = (UPLOADS - shared) * WORKERS.len() as u32;
    assert_eq!(
        engine.item_ids(Partition::Current).unwrap().len() as u32,
        shared + own
    );
    let live = engine.live_generations().unwrap();
    let on_shelf: u64 = live.iter().map(|g| engine.shelf().bytes(*g).unwrap()).sum();
    assert_eq!(engine.used_bytes().unwrap(), on_shelf);
    assert_eq!(engine.shelf().generations().unwrap(), live);
    assert_eq!(engine.pending_uploads().unwrap(), 0);
    assert!(engine.shelf().staged().unwrap().is_empty());
}
