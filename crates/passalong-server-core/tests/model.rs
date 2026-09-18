//! The interruption-and-replay model test of PLAN-00001 (REQ-05).
//!
//! A scripted client runs each scenario as a list of requests. For every
//! scenario and every request index `n` the driver runs four variants:
//!
//! - **stop**: the client dies after request `n`. A second client waits for
//!   the lease to end and recovers, once by resuming and once by aborting
//!   when a rewrite is open. Then the dead client's remaining requests
//!   arrive after all, as a zombie's would, and must do no harm.
//! - **repeat**: request `n` is sent twice in a row, as after a lost answer,
//!   and both answers must be equal.
//! - **late replay**: request `n` is sent again after the script ended.
//!
//! After every single request the invariants I1 to I5 are checked.
//!
//! The "client" seals by prefixing: content is `<key id or "plain">|<text>`,
//! and an item's content key is a hash of both, so that, as in the real
//! client, the same text has another id under another key.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use passalong_server_core::clock::ManualClock;
use passalong_server_core::error::ApiError;
use passalong_server_core::ids::{ApiKeyId, ItemId, KeyId, UploadId};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::rewrite::{RewriteKind, RewriteRequest};
use passalong_server_core::shelf::{Envelope, ItemShelf, MemoryShelf, StoredItem};
use passalong_server_core::upload::{Begun, PutOutcome, UploadRequest};
use passalong_server_core::workspace::{Caller, EncryptionState, Engine, Limits, Partition, Role};

// ---------- the pretend client ----------

fn caller(name: &str) -> Caller {
    Caller::new(ApiKeyId::new(name), Role::ReadWrite)
}

fn key(text: &str) -> KeyId {
    KeyId::parse(text).unwrap()
}

fn label(under: Option<&KeyId>) -> &str {
    under.map_or("plain", KeyId::as_str)
}

/// The id the client gives `text` created at `ts` under `under`.
fn id_for(ts: u32, under: Option<&KeyId>, text: &str) -> ItemId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in label(under).bytes().chain(*b"|").chain(text.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    ItemId::parse(&format!("{ts:08x}-{:012x}", hash & 0xffff_ffff_ffff)).unwrap()
}

fn seal(under: Option<&KeyId>, text: &str) -> Vec<u8> {
    format!("{}|{text}", label(under)).into_bytes()
}

/// What the client reads back: the key label and the text.
fn open(item: &StoredItem) -> (String, String) {
    let content = String::from_utf8(item.content.clone()).unwrap();
    let (under, text) = content.split_once('|').unwrap();
    (under.to_owned(), text.to_owned())
}

fn ts_of(id: &ItemId) -> u32 {
    u32::from_str_radix(&id.as_str()[..8], 16).unwrap()
}

// ---------- invariants ----------

/// What the driver knows must hold, beyond the engine's own state.
#[derive(Debug, Default, Clone)]
struct Expect {
    /// Texts that must be stored.
    must: BTreeSet<String>,
    /// Texts that may be stored: uploads that may or may not have finished.
    may: BTreeSet<String>,
}

fn texts<S: ItemShelf>(engine: &Engine<S>) -> BTreeSet<String> {
    [Partition::Current, Partition::Plain]
        .into_iter()
        .flat_map(|partition| {
            engine
                .item_ids(partition)
                .into_iter()
                .map(move |id| (partition, id))
        })
        .map(|(partition, id)| open(&engine.item(partition, &id).unwrap()).1)
        .collect()
}

/// I1 to I4. I5 is checked by the driver, which sees the answers.
fn check<S: ItemShelf>(engine: &Engine<S>, expect: &Expect) -> Result<(), String> {
    let view = engine.encryption();

    // I1: one state, and a session exactly when it is `Rewriting`.
    if (view.state == EncryptionState::Rewriting) != view.rewrite.is_some() {
        return Err(format!(
            "I1: state {:?} with session {:?}",
            view.state, view.rewrite
        ));
    }
    // A plaintext workspace has no key and a sealed one has one; during a
    // rewrite the key is the one before it, which a migration lacks.
    let key_fits = match view.state {
        EncryptionState::Plaintext => view.key_id.is_none(),
        EncryptionState::Sealed => view.key_id.is_some(),
        EncryptionState::Rewriting => true,
    };
    if !key_fits || view.key_id.is_some() != view.header.is_some() {
        return Err(format!(
            "I1: state {:?} with key {:?}",
            view.state, view.key_id
        ));
    }

    // I2: every item was uploaded under the key its generation is under.
    for id in engine.item_ids(Partition::Current) {
        let item = engine.item(Partition::Current, &id).unwrap();
        if item.envelope.under != view.key_id || open(&item).0 != label(view.key_id.as_ref()) {
            return Err(format!(
                "I2: {id} is under {:?}, the workspace under {:?}",
                item.envelope.under, view.key_id
            ));
        }
    }
    for id in engine.item_ids(Partition::Plain) {
        let item = engine.item(Partition::Plain, &id).unwrap();
        if item.envelope.under.is_some() {
            return Err(format!(
                "I2: plain item {id} is under {:?}",
                item.envelope.under
            ));
        }
    }
    if let Some(session) = &view.rewrite {
        let staged_generation = *engine.live_generations().last().unwrap();
        for id in &session.staged_ids {
            let item = engine.shelf().get(staged_generation, id).unwrap();
            if item.envelope.under.as_ref() != Some(&session.new_key_id) {
                return Err(format!(
                    "I2: staged {id} is under {:?}",
                    item.envelope.under
                ));
            }
        }
    }

    // I3: nothing lost, nothing invented.
    let stored = texts(engine);
    if let Some(lost) = expect.must.difference(&stored).next() {
        return Err(format!("I3: `{lost}` is lost; stored: {stored:?}"));
    }
    if let Some(extra) = stored
        .iter()
        .find(|text| !expect.must.contains(*text) && !expect.may.contains(*text))
    {
        return Err(format!("I3: `{extra}` was never sent"));
    }

    // I4: the books agree with the shelf.
    let on_shelf: u64 = engine
        .live_generations()
        .iter()
        .map(|g| engine.shelf().bytes(*g))
        .sum();
    if engine.used_bytes() != on_shelf {
        return Err(format!(
            "I4: {} bytes on the books, {on_shelf} on the shelf",
            engine.used_bytes()
        ));
    }
    if engine.shelf().staged().len() > engine.pending_uploads() {
        return Err(format!(
            "I4: {} staging places for {} uploads",
            engine.shelf().staged().len(),
            engine.pending_uploads()
        ));
    }
    Ok(())
}

/// What must hold once everything has settled and the janitor has passed.
fn check_settled<S: ItemShelf>(engine: &Engine<S>, expect: &Expect) -> Result<(), String> {
    check(engine, expect)?;
    if engine.encryption().state == EncryptionState::Rewriting {
        return Err("settled: a rewrite is still open".to_owned());
    }
    if !engine.shelf().staged().is_empty()
        || engine.pending_uploads() != 0
        || engine.reserved_bytes() != 0
    {
        return Err(format!(
            "settled: {} staging places, {} uploads, {} bytes reserved",
            engine.shelf().staged().len(),
            engine.pending_uploads(),
            engine.reserved_bytes()
        ));
    }
    let live = engine.live_generations();
    if let Some(orphan) = engine
        .shelf()
        .generations()
        .into_iter()
        .find(|g| !live.contains(g))
    {
        return Err(format!(
            "settled: generation {orphan} is on the shelf and nothing points to it"
        ));
    }
    Ok(())
}

// ---------- scripts ----------

/// What a script remembers between its requests.
#[derive(Default)]
struct Ctx {
    uploads: BTreeMap<String, UploadId>,
    /// I5: the one outcome of each upload id.
    outcomes: BTreeMap<UploadId, PutOutcome>,
}

type Answer = Result<String, ApiError>;
type Step<S> = Box<dyn Fn(&mut Ctx, &mut Engine<S>) -> Answer>;

struct Scenario<S> {
    name: &'static str,
    /// Texts stored before the script starts, with the key they are under.
    initial: Vec<&'static str>,
    initial_key: Option<&'static str>,
    steps: Vec<Step<S>>,
    /// Texts the script uploads.
    uploads: Vec<&'static str>,
}

/// I5: an upload id has one outcome, whoever asks and however often.
fn remember(ctx: &mut Ctx, upload: &UploadId, outcome: &PutOutcome) {
    let first = ctx
        .outcomes
        .entry(upload.clone())
        .or_insert_with(|| outcome.clone());
    assert_eq!(
        first,
        outcome,
        "I5: upload {} changed its outcome",
        upload.as_str()
    );
}

/// The three requests of one upload, as three steps.
fn upload<S: ItemShelf + 'static>(
    who: &'static str,
    name: &'static str,
    ts: u32,
    under: Option<&'static str>,
    text: &'static str,
    in_rewrite: bool,
) -> Vec<Step<S>> {
    let under = under.map(key);
    let (u1, u2) = (under.clone(), under.clone());
    vec![
        Box::new(move |ctx, engine| {
            let request = UploadRequest {
                id: id_for(ts, u1.as_ref(), text),
                meta: b"{}".to_vec(),
                size: seal(u1.as_ref(), text).len() as u64,
                expected_key_id: u1.clone(),
                in_rewrite,
            };
            match engine.begin_upload(&caller(who), request)? {
                Begun::Ticket(ticket) => {
                    ctx.uploads
                        .insert(name.to_owned(), ticket.upload_id.clone());
                    Ok(format!("{ticket:?}"))
                }
                Begun::Stored(outcome) => Ok(format!("{outcome:?}")),
            }
        }),
        Box::new(move |ctx, engine| {
            let Some(upload) = ctx.uploads.get(name).cloned() else {
                return Err(ApiError::NotFound);
            };
            engine.put_upload_content(&caller(who), &upload, seal(u2.as_ref(), text))?;
            Ok("204".to_owned())
        }),
        Box::new(move |ctx, engine| {
            let Some(upload) = ctx.uploads.get(name).cloned() else {
                return Err(ApiError::NotFound);
            };
            let outcome = engine.commit_upload(&caller(who), &upload)?;
            remember(ctx, &upload, &outcome);
            Ok(format!("{outcome:?}"))
        }),
    ]
}

fn rewrite_request(kind: RewriteKind, old: Option<&str>, new: &str) -> RewriteRequest {
    RewriteRequest {
        kind,
        expected_key_id: old.map(key),
        new_key_id: key(new),
        new_header: format!("header-{new}").into_bytes(),
    }
}

fn begin_rewrite<S: ItemShelf + 'static>(
    who: &'static str,
    kind: RewriteKind,
    old: Option<&'static str>,
    new: &'static str,
) -> Step<S> {
    Box::new(move |_, engine| {
        engine
            .begin_rewrite(&caller(who), rewrite_request(kind, old, new))
            .map(|view| format!("{view:?}"))
    })
}

/// Seals and stages every source item that is not staged yet, as the
/// client's rewrite engine does, so that a resumed run repeats nothing.
fn stage_missing<S: ItemShelf>(
    ctx: &mut Ctx,
    engine: &mut Engine<S>,
    who: &str,
    new: &KeyId,
) -> Result<usize, ApiError> {
    let session = engine.session_view().ok_or(ApiError::NotFound)?;
    let mut staged = 0;
    let mut sources = engine.item_ids(Partition::Current);
    sources.reverse();
    for source in sources {
        let text = open(&engine.item(Partition::Current, &source).unwrap()).1;
        let id = id_for(ts_of(&source), Some(new), &text);
        if session.staged_ids.contains(&id) {
            continue;
        }
        let request = UploadRequest {
            id,
            meta: b"{}".to_vec(),
            size: seal(Some(new), &text).len() as u64,
            expected_key_id: Some(new.clone()),
            in_rewrite: true,
        };
        if let Begun::Ticket(ticket) = engine.begin_upload(&caller(who), request)? {
            engine.put_upload_content(&caller(who), &ticket.upload_id, seal(Some(new), &text))?;
            let outcome = engine.commit_upload(&caller(who), &ticket.upload_id)?;
            remember(ctx, &ticket.upload_id, &outcome);
        }
        staged += 1;
    }
    Ok(staged)
}

fn scenarios<S: ItemShelf + 'static>() -> Vec<Scenario<S>> {
    let mut all = Vec::new();

    all.push(Scenario {
        name: "upload",
        initial: vec!["one"],
        initial_key: None,
        steps: upload("a", "u", 10, None, "two", false),
        uploads: vec!["two"],
    });

    let mut steps: Vec<Step<S>> = vec![Box::new(|_, engine| {
        engine
            .enable_encryption(&caller("a"), key("aa"), b"header-aa".to_vec())
            .map(|view| format!("{view:?}"))
    })];
    steps.extend(upload("a", "u", 10, Some("aa"), "two", false));
    all.push(Scenario {
        name: "enable on empty",
        initial: vec![],
        initial_key: None,
        steps,
        uploads: vec!["two"],
    });

    let mut steps: Vec<Step<S>> = vec![Box::new(|_, engine| {
        engine
            .replace_header(&caller("a"), &key("aa"), b"header-aa-new-words".to_vec())
            .map(|view| format!("{view:?}"))
    })];
    steps.extend(upload("b", "u", 10, Some("aa"), "two", false));
    all.push(Scenario {
        name: "change of words",
        initial: vec!["one"],
        initial_key: Some("aa"),
        steps,
        uploads: vec!["two"],
    });

    let mut steps: Vec<Step<S>> = vec![Box::new(|_, engine| {
        engine
            .fresh_start(&caller("a"), key("aa"), b"header-aa".to_vec())
            .map(|view| format!("{view:?}"))
    })];
    steps.extend(upload("a", "u", 10, Some("aa"), "three", false));
    all.push(Scenario {
        name: "fresh start",
        initial: vec!["one", "two"],
        initial_key: None,
        steps,
        uploads: vec!["three"],
    });

    for (name, kind, old, abort) in [
        ("migrate", RewriteKind::Migrate, None, false),
        ("rotate", RewriteKind::Rotate, Some("aa"), false),
        ("migrate, aborted", RewriteKind::Migrate, None, true),
        ("rotate, aborted", RewriteKind::Rotate, Some("aa"), true),
    ] {
        let mut steps: Vec<Step<S>> = vec![begin_rewrite("a", kind, old, "bb")];
        // The initial items are created at 1 and 2; see `prepare`.
        steps.extend(upload("a", "s1", 1, Some("bb"), "one", true));
        steps.push(Box::new(|_, engine| {
            engine
                .heartbeat_rewrite(&caller("a"))
                .map(|view| format!("{view:?}"))
        }));
        if abort {
            steps.push(Box::new(|_, engine| {
                engine
                    .abort_rewrite(&caller("a"), &key("bb"))
                    .map(|view| format!("{view:?}"))
            }));
        } else {
            steps.extend(upload("a", "s2", 2, Some("bb"), "two", true));
            steps.push(Box::new(|_, engine| {
                engine
                    .commit_rewrite(&caller("a"), &key("bb"))
                    .map(|view| format!("{view:?}"))
            }));
            // Life goes on under the new key.
            steps.extend(upload("b", "u", 10, Some("bb"), "three", false));
        }
        all.push(Scenario {
            name,
            initial: vec!["one", "two"],
            initial_key: old,
            steps,
            uploads: if abort { vec![] } else { vec!["three"] },
        });
    }

    let mut steps: Vec<Step<S>> = vec![begin_rewrite("a", RewriteKind::Rotate, Some("aa"), "bb")];
    steps.extend(upload("a", "s1", 1, Some("bb"), "one", true));
    steps.push(Box::new(|_, engine| {
        // The second device finds the lease running: it must wait.
        match engine.take_over_rewrite(&caller("b")) {
            Err(ApiError::LeaseHeld) => Ok("LEASE_HELD".to_owned()),
            Err(err) => Err(err),
            Ok(view) => Err(ApiError::InvalidRequest(format!(
                "took over a running lease: {view:?}"
            ))),
        }
    }));
    steps.push(Box::new(|_, engine| {
        engine
            .take_over_rewrite(&caller("b"))
            .map(|view| format!("{view:?}"))
    }));
    steps.push(Box::new(|ctx, engine| {
        stage_missing(ctx, engine, "b", &key("bb"))
            .map(|_| format!("{:?}", engine.session_view().map(|view| view.staged_ids)))
    }));
    steps.push(Box::new(|_, engine| {
        engine
            .commit_rewrite(&caller("b"), &key("bb"))
            .map(|view| format!("{view:?}"))
    }));
    all.push(Scenario {
        name: "take-over by a second client",
        initial: vec!["one", "two"],
        initial_key: Some("aa"),
        steps,
        uploads: vec![],
    });

    all
}

// ---------- the driver ----------

const LEASE: u64 = 600;
const STAGING: u64 = 3_600;

fn limits() -> Limits {
    Limits {
        quota_bytes: 1_000,
        max_item_bytes: Some(100),
        staging_secs: STAGING,
        lease_secs: LEASE,
    }
}

/// A workspace holding the scenario's initial items, put there by uploads.
fn prepare<S: ItemShelf>(shelf: S, scenario: &Scenario<S>) -> (Engine<S>, ManualClock, Expect) {
    let clock = ManualClock::at(1_000);
    let mut engine = Engine::new(
        shelf,
        Arc::new(clock.clone()),
        Box::new(SeededRandom::new(1)),
        limits(),
    );
    let under = scenario.initial_key.map(key);
    if let Some(under) = &under {
        engine
            .enable_encryption(&caller("setup"), under.clone(), b"header".to_vec())
            .unwrap();
    }
    for (index, text) in scenario.initial.iter().enumerate() {
        let request = UploadRequest {
            id: id_for(index as u32 + 1, under.as_ref(), text),
            meta: b"{}".to_vec(),
            size: seal(under.as_ref(), text).len() as u64,
            expected_key_id: under.clone(),
            in_rewrite: false,
        };
        let Begun::Ticket(ticket) = engine.begin_upload(&caller("setup"), request).unwrap() else {
            panic!("initial item already stored");
        };
        engine
            .put_upload_content(
                &caller("setup"),
                &ticket.upload_id,
                seal(under.as_ref(), text),
            )
            .unwrap();
        assert!(
            engine
                .commit_upload(&caller("setup"), &ticket.upload_id)
                .unwrap()
                .created
        );
    }
    let expect = Expect {
        must: scenario
            .initial
            .iter()
            .map(|text| (*text).to_owned())
            .collect(),
        may: scenario
            .uploads
            .iter()
            .map(|text| (*text).to_owned())
            .collect(),
    };
    (engine, clock, expect)
}

/// The take-over scenario's third step passes only once the lease ended.
fn before_step<S>(scenario: &Scenario<S>, index: usize, clock: &ManualClock) {
    if scenario.name.starts_with("take-over") && index == 5 {
        clock.advance(LEASE + 1);
    }
}

fn run_step<S: ItemShelf>(
    scenario: &Scenario<S>,
    index: usize,
    ctx: &mut Ctx,
    engine: &mut Engine<S>,
    clock: &ManualClock,
    expect: &Expect,
) -> String {
    before_step(scenario, index, clock);
    send(scenario, index, ctx, engine, expect)
}

/// Sends request `index` and checks the invariants, without moving time.
fn send<S: ItemShelf>(
    scenario: &Scenario<S>,
    index: usize,
    ctx: &mut Ctx,
    engine: &mut Engine<S>,
    expect: &Expect,
) -> String {
    let answer = (scenario.steps[index])(ctx, engine)
        .unwrap_or_else(|err| panic!("{}: request {index} was refused: {err}", scenario.name));
    check(engine, expect)
        .unwrap_or_else(|err| panic!("{}: after request {index}: {err}", scenario.name));
    answer
}

fn settle<S: ItemShelf>(engine: &mut Engine<S>, clock: &ManualClock, expect: &Expect, what: &str) {
    clock.advance(STAGING + 1);
    engine.clean_staging();
    check_settled(engine, expect).unwrap_or_else(|err| panic!("{what}: {err}"));
}

#[derive(Clone, Copy, Debug)]
enum Recovery {
    Resume,
    Abort,
}

/// The client dies after request `n`; a rescuer recovers; the zombie's
/// remaining requests arrive anyway.
fn stop_after<S: ItemShelf>(
    shelf: S,
    scenario: &Scenario<S>,
    n: usize,
    recovery: Recovery,
) -> bool {
    let (mut engine, clock, expect) = prepare(shelf, scenario);
    let mut ctx = Ctx::default();
    for index in 0..=n {
        run_step(scenario, index, &mut ctx, &mut engine, &clock, &expect);
    }
    let what = format!("{}: stop after {n}, {recovery:?}", scenario.name);

    let open = engine.encryption().state == EncryptionState::Rewriting;
    if open {
        let rescuer = caller("rescuer");
        let new = engine.session_view().unwrap().new_key_id;
        assert_eq!(
            engine.take_over_rewrite(&rescuer).unwrap_err(),
            ApiError::LeaseHeld,
            "{what}"
        );
        clock.advance(LEASE + 1);
        engine.take_over_rewrite(&rescuer).unwrap();
        check(&engine, &expect).unwrap_or_else(|err| panic!("{what}: after the take-over: {err}"));
        match recovery {
            Recovery::Resume => {
                stage_missing(&mut ctx, &mut engine, "rescuer", &new).unwrap();
                engine.commit_rewrite(&rescuer, &new).unwrap();
            }
            Recovery::Abort => {
                engine.abort_rewrite(&rescuer, &new).unwrap();
            }
        }
        check(&engine, &expect).unwrap_or_else(|err| panic!("{what}: after the recovery: {err}"));
    }

    // The zombie: whatever it still sends is answered or refused, and
    // nothing it sends may break an invariant.
    for index in n + 1..scenario.steps.len() {
        before_step(scenario, index, &clock);
        let _ = (scenario.steps[index])(&mut ctx, &mut engine);
        check(&engine, &expect)
            .unwrap_or_else(|err| panic!("{what}: zombie request {index}: {err}"));
    }
    // A zombie may have opened nothing new: every script's rewrite begins
    // with its first request, which already ran.
    settle(&mut engine, &clock, &expect, &what);
    open
}

/// Request `n` is sent twice in a row; both answers must be equal.
fn repeat<S: ItemShelf>(shelf: S, scenario: &Scenario<S>, n: usize) {
    let (mut engine, clock, mut expect) = prepare(shelf, scenario);
    let mut ctx = Ctx::default();
    for index in 0..scenario.steps.len() {
        let first = run_step(scenario, index, &mut ctx, &mut engine, &clock, &expect);
        if index == n {
            // A lost answer is asked for again at once: no time passes. A
            // heartbeat, a take-over, and a repeated `beginRewrite` renew
            // the lease, and with the clock standing still even they agree.
            let second = send(scenario, index, &mut ctx, &mut engine, &expect);
            assert_eq!(
                first, second,
                "{}: request {n} changed its answer",
                scenario.name
            );
        }
    }
    expect.must.extend(expect.may.clone());
    settle(
        &mut engine,
        &clock,
        &expect,
        &format!("{}: repeat {n}", scenario.name),
    );
}

/// Request `n` is sent again after the script ended.
fn replay_late<S: ItemShelf>(shelf: S, scenario: &Scenario<S>, n: usize) {
    let (mut engine, clock, mut expect) = prepare(shelf, scenario);
    let mut ctx = Ctx::default();
    for index in 0..scenario.steps.len() {
        run_step(scenario, index, &mut ctx, &mut engine, &clock, &expect);
    }
    expect.must.extend(expect.may.clone());
    let _ = (scenario.steps[n])(&mut ctx, &mut engine);
    check(&engine, &expect)
        .unwrap_or_else(|err| panic!("{}: late replay of {n}: {err}", scenario.name));
    settle(
        &mut engine,
        &clock,
        &expect,
        &format!("{}: late replay of {n}", scenario.name),
    );
}

fn run_everything<S: ItemShelf + 'static>(make: impl Fn() -> S) -> (usize, usize) {
    let (mut variants, mut requests) = (0, 0);
    for scenario in scenarios::<S>() {
        requests += scenario.steps.len();
        for n in 0..scenario.steps.len() {
            let open = stop_after(make(), &scenario, n, Recovery::Resume);
            variants += 1;
            if open {
                stop_after(make(), &scenario, n, Recovery::Abort);
                variants += 1;
            }
            repeat(make(), &scenario, n);
            replay_late(make(), &scenario, n);
            variants += 2;
        }
    }
    (variants, requests)
}

// ---------- shelves that misbehave ----------

/// Forwards everything to a [`MemoryShelf`], except what a test overrides.
macro_rules! forward {
    () => {
        fn stage_create(&mut self, upload: &UploadId) {
            self.inner.stage_create(upload);
        }
        fn stage_write(&mut self, upload: &UploadId, content: Vec<u8>) -> Result<(), ApiError> {
            self.inner.stage_write(upload, content)
        }
        fn stage_size(&self, upload: &UploadId) -> Option<u64> {
            self.inner.stage_size(upload)
        }
        fn stage_remove(&mut self, upload: &UploadId) {
            self.inner.stage_remove(upload);
        }
        fn staged(&self) -> Vec<UploadId> {
            self.inner.staged()
        }
        fn get(&self, generation: u64, id: &ItemId) -> Option<StoredItem> {
            self.inner.get(generation, id)
        }
        fn ids(&self, generation: u64) -> Vec<ItemId> {
            self.inner.ids(generation)
        }
        fn remove(&mut self, generation: u64, id: &ItemId) -> Option<StoredItem> {
            self.inner.remove(generation, id)
        }
        fn generations(&self) -> Vec<u64> {
            self.inner.generations()
        }
        fn bytes(&self, generation: u64) -> u64 {
            self.inner.bytes(generation)
        }
    };
}

/// Broken: says it published, and stores nothing.
#[derive(Default)]
struct LossyShelf {
    inner: MemoryShelf,
}

impl ItemShelf for LossyShelf {
    forward!();
    fn publish(
        &mut self,
        upload: &UploadId,
        _: u64,
        _: &ItemId,
        _: Envelope,
    ) -> Result<bool, ApiError> {
        self.inner.stage_remove(upload);
        Ok(true)
    }
    fn drop_generation(&mut self, generation: u64) {
        self.inner.drop_generation(generation);
    }
}

/// Broken: never removes a generation.
#[derive(Default)]
struct HoardingShelf {
    inner: MemoryShelf,
}

impl ItemShelf for HoardingShelf {
    forward!();
    fn publish(
        &mut self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError> {
        self.inner.publish(upload, generation, id, envelope)
    }
    fn drop_generation(&mut self, _: u64) {}
}

/// Not broken, only unlucky: the first removal of a generation is cut
/// short, as by a crash between `commitRewrite`'s transaction and its
/// clean-up. The janitor must finish the job.
#[derive(Default)]
struct InterruptedShelf {
    inner: MemoryShelf,
    interrupted: bool,
}

impl ItemShelf for InterruptedShelf {
    forward!();
    fn publish(
        &mut self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError> {
        self.inner.publish(upload, generation, id, envelope)
    }
    fn drop_generation(&mut self, generation: u64) {
        if self.interrupted {
            self.inner.drop_generation(generation);
        }
        self.interrupted = true;
    }
}

// ---------- the tests ----------

#[test]
fn the_checker_notices_a_shelf_that_loses_items() {
    let scenario = &scenarios::<LossyShelf>()[0];
    let clock = ManualClock::at(1_000);
    let mut engine = Engine::new(
        LossyShelf::default(),
        Arc::new(clock),
        Box::new(SeededRandom::new(1)),
        limits(),
    );
    let mut ctx = Ctx::default();
    let expect = Expect {
        must: ["two".to_owned()].into(),
        may: BTreeSet::new(),
    };
    for step in &scenario.steps {
        step(&mut ctx, &mut engine).unwrap();
    }
    let err = check(&engine, &expect).unwrap_err();
    assert!(err.starts_with("I3") || err.starts_with("I4"), "{err}");
}

#[test]
fn the_checker_notices_a_shelf_that_keeps_dropped_generations() {
    let all = scenarios::<HoardingShelf>();
    let scenario = all
        .iter()
        .find(|scenario| scenario.name == "rotate")
        .unwrap();
    let (mut engine, clock, mut expect) = prepare(HoardingShelf::default(), scenario);
    let mut ctx = Ctx::default();
    for index in 0..scenario.steps.len() {
        run_step(scenario, index, &mut ctx, &mut engine, &clock, &expect);
    }
    expect.must.extend(expect.may.clone());
    clock.advance(STAGING + 1);
    engine.clean_staging();
    let err = check_settled(&engine, &expect).unwrap_err();
    assert!(err.contains("nothing points to it"), "{err}");
}

#[test]
fn every_scenario_survives_a_client_that_stops_or_repeats_at_every_request() {
    let (variants, requests) = run_everything(MemoryShelf::default);
    println!(
        "model test: {variants} variants over {requests} requests in 9 scenarios, I1 to I5 after every request"
    );
    assert!(
        variants >= requests * 2,
        "{variants} variants for {requests} requests"
    );
}

#[test]
fn the_janitor_finishes_a_clean_up_that_was_cut_short() {
    let (variants, _) = run_everything(InterruptedShelf::default);
    println!("model test, interrupted clean-up: {variants} variants");
}
