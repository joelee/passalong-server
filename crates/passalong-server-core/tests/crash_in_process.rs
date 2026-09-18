//! What a death between a shelf step and the ledger's commit leaves, and
//! how it is repaired (PLAN-00002, REQ-05). Provoked in process: a ledger
//! that runs the rule, so the shelf is changed, and then stores nothing, as
//! if the process had died before the commit. `tests/kill.rs` does the same
//! with real kills.

use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use passalong_server_core::clock::ManualClock;
use passalong_server_core::error::ApiError;
use passalong_server_core::ids::{ApiKeyId, ItemId, KeyId, UploadId};
use passalong_server_core::ledger::{Ledger, MemoryLedger, Rule, WorkspaceId, WorkspaceRecord};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::rewrite::{RewriteKind, RewriteRequest};
use passalong_server_core::shelf::{ItemShelf, MemoryShelf};
use passalong_server_core::upload::{Begun, UploadRequest};
use passalong_server_core::workspace::{Caller, EncryptionState, Engine, Limits, Partition, Role};

/// Dies, once, between the rule and the commit.
#[derive(Default)]
struct DyingLedger {
    inner: MemoryLedger,
    armed: AtomicBool,
}

impl Ledger for DyingLedger {
    fn create(&self, workspace: &WorkspaceId) -> Result<(), ApiError> {
        self.inner.create(workspace)
    }
    fn load(&self, workspace: &WorkspaceId) -> Result<WorkspaceRecord, ApiError> {
        self.inner.load(workspace)
    }
    fn transact<T>(&self, workspace: &WorkspaceId, rule: Rule<'_, T>) -> Result<T, ApiError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            // The rule runs against a copy that is thrown away.
            let mut copy = self.inner.load(workspace)?;
            rule(&mut copy)?;
            return Err(ApiError::ServiceUnavailable);
        }
        self.inner.transact(workspace, rule)
    }
}

type Stores = (Arc<MemoryShelf>, Arc<DyingLedger>);

fn caller(name: &str) -> Caller {
    Caller::new(ApiKeyId::new(name), Role::ReadWrite)
}

fn key(text: &str) -> KeyId {
    KeyId::parse(text).unwrap()
}

/// A fresh engine over the same stores: what a restarted server has.
fn open((shelf, ledger): &Stores) -> Engine<Arc<MemoryShelf>, Arc<DyingLedger>> {
    Engine::open(
        shelf.clone(),
        ledger.clone(),
        WorkspaceId::parse("00000000000000aa").unwrap(),
        Arc::new(ManualClock::at(1_000)),
        Box::new(SeededRandom::new(7)),
        Limits::default(),
    )
    .unwrap()
}

fn request(id: &str, content: &[u8], under: Option<&str>, in_rewrite: bool) -> UploadRequest {
    UploadRequest {
        id: ItemId::parse(id).unwrap(),
        meta: b"{}".to_vec(),
        size: content.len() as u64,
        expected_key_id: under.map(key),
        in_rewrite,
    }
}

fn staged(
    engine: &mut Engine<Arc<MemoryShelf>, Arc<DyingLedger>>,
    who: &str,
    req: UploadRequest,
    content: &[u8],
) -> UploadId {
    let Begun::Ticket(ticket) = engine.begin_upload(&caller(who), req).unwrap() else {
        panic!("already stored");
    };
    engine
        .put_upload_content(
            &caller(who),
            &ticket.upload_id,
            &mut Cursor::new(content.to_vec()),
        )
        .unwrap();
    ticket.upload_id
}

fn put(
    engine: &mut Engine<Arc<MemoryShelf>, Arc<DyingLedger>>,
    id: &str,
    content: &[u8],
    under: Option<&str>,
) {
    let upload = staged(engine, "setup", request(id, content, under, false), content);
    assert!(
        engine
            .commit_upload(&caller("setup"), &upload)
            .unwrap()
            .created
    );
}

/// The books agree with the shelf, and nothing is left lying about.
fn assert_consistent(engine: &Engine<Arc<MemoryShelf>, Arc<DyingLedger>>) {
    let live = engine.live_generations().unwrap();
    let on_shelf: u64 = live.iter().map(|g| engine.shelf().bytes(*g).unwrap()).sum();
    assert_eq!(
        engine.used_bytes().unwrap(),
        on_shelf,
        "bytes on the books and on the shelf"
    );
    for generation in engine.shelf().generations().unwrap() {
        assert!(
            live.contains(&generation),
            "generation {generation} is an orphan"
        );
    }
    assert!(engine.shelf().staged().unwrap().len() <= engine.pending_uploads().unwrap());
}

#[test]
fn a_publish_that_was_never_recorded_is_finished_by_the_repeated_commit() {
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    let upload = staged(
        &mut engine,
        "a",
        request("00000001-aaaaaaaaaaaa", b"hello", None, false),
        b"hello",
    );

    stores.1.armed.store(true, Ordering::SeqCst);
    assert_eq!(
        engine.commit_upload(&caller("a"), &upload).unwrap_err(),
        ApiError::ServiceUnavailable
    );
    // The item is on the shelf; the books know nothing of it.
    assert_eq!(engine.shelf().ids(0).unwrap().len(), 1);
    assert_eq!(engine.used_bytes().unwrap(), 0);
    drop(engine);

    let mut engine = open(&stores);
    assert_consistent(&engine);
    assert_eq!(engine.used_bytes().unwrap(), 5);
    // The client, which never got its answer, asks again, and gets the
    // answer the first commit would have given.
    let outcome = engine.commit_upload(&caller("a"), &upload).unwrap();
    assert!(outcome.created);
    assert_eq!(
        engine.commit_upload(&caller("a"), &upload).unwrap(),
        outcome
    );
    assert_eq!(engine.used_bytes().unwrap(), 5);
    assert_eq!(engine.pending_uploads().unwrap(), 0);
    assert_consistent(&engine);
}

#[test]
fn the_repeated_commit_needs_no_restart_in_between() {
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    let upload = staged(
        &mut engine,
        "a",
        request("00000001-aaaaaaaaaaaa", b"hello", None, false),
        b"hello",
    );
    stores.1.armed.store(true, Ordering::SeqCst);
    engine.commit_upload(&caller("a"), &upload).unwrap_err();
    assert!(engine.commit_upload(&caller("a"), &upload).unwrap().created);
    assert_eq!(engine.used_bytes().unwrap(), 5);
    assert_consistent(&engine);
}

#[test]
fn someone_elses_item_is_not_mistaken_for_an_unrecorded_publish() {
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    // Two uploads of one id: the second finds the first's item, and its own
    // staging place is still there, so it is a duplicate, not a repeat.
    let first = staged(
        &mut engine,
        "a",
        request("00000001-aaaaaaaaaaaa", b"hello", None, false),
        b"hello",
    );
    let second = staged(
        &mut engine,
        "b",
        request("00000001-aaaaaaaaaaaa", b"hello", None, false),
        b"hello",
    );
    assert!(engine.commit_upload(&caller("a"), &first).unwrap().created);
    assert!(!engine.commit_upload(&caller("b"), &second).unwrap().created);
    assert_eq!(engine.used_bytes().unwrap(), 5);
    assert_consistent(&engine);
}

#[test]
fn a_removal_that_was_never_recorded_is_found_at_the_next_opening() {
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    put(&mut engine, "00000001-aaaaaaaaaaaa", b"12345", None);
    put(&mut engine, "00000002-bbbbbbbbbbbb", b"678", None);
    let id = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();

    stores.1.armed.store(true, Ordering::SeqCst);
    engine
        .delete_item(&caller("a"), Partition::Current, &id, None)
        .unwrap_err();
    assert_eq!(engine.used_bytes().unwrap(), 8);
    drop(engine);

    let mut engine = open(&stores);
    assert_eq!(engine.used_bytes().unwrap(), 3);
    assert_consistent(&engine);
    // Asked again, the delete finds nothing, which the client takes as done.
    assert_eq!(
        engine
            .delete_item(&caller("a"), Partition::Current, &id, None)
            .unwrap_err(),
        ApiError::NotFound
    );
}

#[test]
fn a_commit_of_a_rewrite_that_was_never_recorded_loses_nothing() {
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    put(&mut engine, "00000001-aaaaaaaaaaaa", b"plain", None);
    let rotate = RewriteRequest {
        kind: RewriteKind::Migrate,
        expected_key_id: None,
        new_key_id: key("bb"),
        new_header: b"header".to_vec(),
    };
    engine.begin_rewrite(&caller("a"), rotate).unwrap();
    let upload = staged(
        &mut engine,
        "a",
        request("00000001-cccccccccccc", b"sealed", Some("bb"), true),
        b"sealed",
    );
    engine.commit_upload(&caller("a"), &upload).unwrap();

    stores.1.armed.store(true, Ordering::SeqCst);
    engine.commit_rewrite(&caller("a"), &key("bb")).unwrap_err();
    drop(engine);

    // Nothing was destroyed: the old generation is dropped only after the
    // record points to the new one, and the record never did.
    let mut engine = open(&stores);
    assert_eq!(
        engine.encryption().unwrap().state,
        EncryptionState::Rewriting
    );
    assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 1);
    assert_eq!(engine.session_view().unwrap().unwrap().staged_ids.len(), 1);
    assert_consistent(&engine);
    let view = engine.commit_rewrite(&caller("a"), &key("bb")).unwrap();
    assert_eq!(view.state, EncryptionState::Sealed);
    assert_consistent(&engine);
    assert_eq!(engine.shelf().generations().unwrap().len(), 1);
}

#[test]
fn an_abort_and_a_fresh_start_that_were_never_recorded_can_be_repeated() {
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    put(&mut engine, "00000001-aaaaaaaaaaaa", b"plain", None);

    stores.1.armed.store(true, Ordering::SeqCst);
    engine
        .fresh_start(&caller("a"), key("aa"), b"h".to_vec())
        .unwrap_err();
    let mut engine = {
        drop(engine);
        open(&stores)
    };
    assert_eq!(
        engine.encryption().unwrap().state,
        EncryptionState::Plaintext
    );
    assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 1);
    engine
        .fresh_start(&caller("a"), key("aa"), b"h".to_vec())
        .unwrap();
    assert_eq!(engine.item_ids(Partition::Plain).unwrap().len(), 1);

    let rotate = RewriteRequest {
        kind: RewriteKind::Rotate,
        expected_key_id: Some(key("aa")),
        new_key_id: key("bb"),
        new_header: b"header".to_vec(),
    };
    engine.begin_rewrite(&caller("a"), rotate).unwrap();
    stores.1.armed.store(true, Ordering::SeqCst);
    engine.abort_rewrite(&caller("a"), &key("bb")).unwrap_err();
    let mut engine = {
        drop(engine);
        open(&stores)
    };
    assert_eq!(
        engine.encryption().unwrap().state,
        EncryptionState::Rewriting
    );
    engine.abort_rewrite(&caller("a"), &key("bb")).unwrap();
    assert_eq!(engine.encryption().unwrap().key_id, Some(key("aa")));
    assert_consistent(&engine);
}

#[test]
fn opening_removes_what_nothing_points_to() {
    let stores: Stores = Default::default();
    let orphan = UploadId::generate(&mut SeededRandom::new(3));
    stores.0.stage_create(&orphan).unwrap();
    let engine = open(&stores);
    assert!(engine.shelf().staged().unwrap().is_empty());
    assert_consistent(&engine);
}

#[test]
fn an_upload_id_is_never_handed_out_twice() {
    // A restarted process whose random source repeats itself, as the kill
    // harness's child does, mints the ids the last process minted. Found by
    // the kill harness: the second upload got the id of the first one's
    // remembered outcome, and its commit was answered with that outcome.
    let stores: Stores = Default::default();
    let mut engine = open(&stores);
    put(&mut engine, "00000001-aaaaaaaaaaaa", b"one", None);
    drop(engine);

    let mut engine = open(&stores);
    put(&mut engine, "00000002-bbbbbbbbbbbb", b"two", None);
    assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 2);

    // And not the id of an upload still in flight, either.
    drop(engine);
    let mut engine = open(&stores);
    let pending = staged(
        &mut engine,
        "a",
        request("00000003-cccccccccccc", b"three", None, false),
        b"three",
    );
    drop(engine);
    let mut engine = open(&stores);
    let other = staged(
        &mut engine,
        "b",
        request("00000004-dddddddddddd", b"four", None, false),
        b"four",
    );
    assert_ne!(pending, other);
    assert!(
        engine
            .commit_upload(&caller("a"), &pending)
            .unwrap()
            .created
    );
    assert!(engine.commit_upload(&caller("b"), &other).unwrap().created);
    assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 4);
}

#[test]
fn the_janitor_destroys_nothing_before_its_record_is_stored() {
    // Found by the kill harness: the janitor removed an expired upload's
    // staging place inside its transaction. Killed before the commit, it
    // left a ticket whose staging place was gone, and the client's next
    // `putUploadContent` was answered NOT_FOUND.
    let stores: Stores = Default::default();
    let clock = ManualClock::at(1_000);
    let mut engine = Engine::open(
        stores.0.clone(),
        stores.1.clone(),
        WorkspaceId::parse("00000000000000aa").unwrap(),
        Arc::new(clock.clone()),
        Box::new(SeededRandom::new(7)),
        Limits::default(),
    )
    .unwrap();
    let upload = staged(
        &mut engine,
        "a",
        request("00000001-aaaaaaaaaaaa", b"late", None, false),
        b"late",
    );
    clock.advance(Limits::default().staging_secs + 1);
    stores.1.armed.store(true, Ordering::SeqCst);
    engine.clean_staging().unwrap_err();
    // The record still has the ticket, so the staging place must be there.
    assert_eq!(engine.shelf().staged().unwrap(), vec![upload]);

    // And if a staging place does go missing, a repeated `beginUpload`
    // brings it back instead of handing out a ticket that cannot be used.
    let mut engine = open(&stores);
    let upload = staged(
        &mut engine,
        "b",
        request("00000002-bbbbbbbbbbbb", b"x", None, false),
        b"x",
    );
    stores.0.stage_remove(&upload).unwrap();
    let again = staged(
        &mut engine,
        "b",
        request("00000002-bbbbbbbbbbbb", b"x", None, false),
        b"x",
    );
    assert_eq!(again, upload);
    assert!(engine.commit_upload(&caller("b"), &upload).unwrap().created);
}
