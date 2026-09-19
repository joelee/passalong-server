//! The kill harness (PLAN-00002, REQ-06): the process is killed at every
//! named boundary between a shelf step and the ledger's commit, restarted,
//! and must find the workspace consistent and every interrupted request
//! repeatable.
//!
//! For each scenario of `storage_child` and each fault point, the harness
//! starts the child with `PASSALONG_FAULT` naming the point and
//! `PASSALONG_FAULT_SKIP` counting up from zero, so that the child dies at
//! the point's first passage, then its second, and so on until a run passes
//! the point no more and ends by itself. After every death it opens the same
//! directory, which repairs it, checks the invariants, runs the child again
//! without a fault, which is the client sending its requests again, and
//! checks the invariants and the scenario's expected end.

#![cfg(feature = "fault-injection")]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use passalong_server_core::clock::ManualClock;
use passalong_server_core::fault::POINTS;
use passalong_server_core::ids::KeyId;
use passalong_server_core::ledger::{SqliteLedger, WorkspaceId};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::shelf::{FsShelf, ItemShelf};
use passalong_server_core::workspace::{EncryptionState, Engine, Limits, Partition};

const CHILD: &str = env!("CARGO_BIN_EXE_storage_child");
const STAGING_SECS: u64 = 3_600;

type Stores = Engine<FsShelf, SqliteLedger>;

fn stores(dir: &Path) -> (FsShelf, SqliteLedger, WorkspaceId) {
    (
        FsShelf::open(dir.join("workspaces").join("00000000000000aa")).unwrap(),
        SqliteLedger::open(dir.join("control.sqlite"), Duration::from_secs(20)).unwrap(),
        WorkspaceId::parse("00000000000000aa").unwrap(),
    )
}

fn limits() -> Limits {
    Limits {
        staging_secs: STAGING_SECS,
        // Made-up ids: the content check has tests of its own.
        check_plaintext_content: false,
        ..Limits::default()
    }
}

/// Opens the directory as a restarted server does, which repairs it.
fn open(dir: &Path, clock: &ManualClock) -> Stores {
    let (shelf, ledger, workspace) = stores(dir);
    let rng = Box::new(SeededRandom::new(1_234));
    Engine::open(
        shelf,
        ledger,
        workspace,
        Arc::new(clock.clone()),
        rng,
        limits(),
    )
    .unwrap()
}

/// I1, I2, and I4 of `tests/model.rs`, over the real stores.
fn consistent(engine: &Stores) -> Result<(), String> {
    let view = engine.encryption().unwrap();
    if (view.state == EncryptionState::Rewriting) != view.rewrite.is_some() {
        return Err(format!(
            "I1: state {:?} with session {:?}",
            view.state, view.rewrite
        ));
    }
    for id in engine.item_ids(Partition::Current).unwrap() {
        let item = engine.item(Partition::Current, &id).unwrap().unwrap();
        if item.envelope.under != view.key_id {
            return Err(format!(
                "I2: {id} is under {:?}, the workspace under {:?}",
                item.envelope.under, view.key_id
            ));
        }
    }
    let live = engine.live_generations().unwrap();
    let on_shelf: u64 = live.iter().map(|g| engine.shelf().bytes(*g).unwrap()).sum();
    if engine.used_bytes().unwrap() != on_shelf {
        return Err(format!(
            "I4: {} bytes on the books, {on_shelf} on the shelf",
            engine.used_bytes().unwrap()
        ));
    }
    if let Some(orphan) = engine
        .shelf()
        .generations()
        .unwrap()
        .into_iter()
        .find(|g| !live.contains(g))
    {
        return Err(format!(
            "I4: generation {orphan} is on the shelf and nothing points to it"
        ));
    }
    let staged = engine.shelf().staged().unwrap().len();
    if staged > engine.pending_uploads().unwrap() {
        return Err(format!(
            "I4: {staged} staging places for {} uploads",
            engine.pending_uploads().unwrap()
        ));
    }
    Ok(())
}

fn texts(engine: &Stores, partition: Partition) -> BTreeSet<String> {
    engine
        .item_ids(partition)
        .unwrap()
        .iter()
        .map(|id| {
            let mut bytes = Vec::new();
            engine
                .item_content(partition, id)
                .unwrap()
                .unwrap()
                .read_to_end(&mut bytes)
                .unwrap();
            let content = String::from_utf8(bytes).unwrap();
            content.split_once('|').unwrap().1.to_owned()
        })
        .collect()
}

struct Scenario {
    name: &'static str,
    state: EncryptionState,
    key: Option<&'static str>,
    current: &'static [&'static str],
    plain: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "upload",
        state: EncryptionState::Plaintext,
        key: None,
        current: &["one", "two"],
        plain: &[],
    },
    Scenario {
        name: "fresh-start",
        state: EncryptionState::Sealed,
        key: Some("aa"),
        current: &["three"],
        plain: &["one", "two"],
    },
    Scenario {
        name: "migrate",
        state: EncryptionState::Sealed,
        key: Some("bb"),
        current: &["one", "two", "three"],
        plain: &[],
    },
    Scenario {
        name: "rotate",
        state: EncryptionState::Sealed,
        key: Some("bb"),
        current: &["one", "two", "three"],
        plain: &[],
    },
    Scenario {
        name: "abort",
        state: EncryptionState::Plaintext,
        key: None,
        current: &["one", "two"],
        plain: &[],
    },
    Scenario {
        name: "janitor",
        state: EncryptionState::Plaintext,
        key: None,
        current: &["one"],
        plain: &[],
    },
];

/// The scenario's end, once everything has settled and the janitor passed.
fn ended_well(dir: &Path, scenario: &Scenario) -> Result<(), String> {
    let clock = ManualClock::at(1_000);
    let engine = open(dir, &clock);
    consistent(&engine)?;
    clock.advance(2 * STAGING_SECS);
    engine.clean_staging().unwrap();
    consistent(&engine)?;
    let view = engine.encryption().unwrap();
    let set = |texts: &[&str]| {
        texts
            .iter()
            .map(|text| (*text).to_owned())
            .collect::<BTreeSet<_>>()
    };
    if view.state != scenario.state
        || view.key_id != scenario.key.map(|key| KeyId::parse(key).unwrap())
    {
        return Err(format!("ended {:?} under {:?}", view.state, view.key_id));
    }
    if texts(&engine, Partition::Current) != set(scenario.current)
        || texts(&engine, Partition::Plain) != set(scenario.plain)
    {
        return Err(format!(
            "ended with {:?} and, set aside, {:?}",
            texts(&engine, Partition::Current),
            texts(&engine, Partition::Plain)
        ));
    }
    if !engine.shelf().staged().unwrap().is_empty() || engine.pending_uploads().unwrap() != 0 {
        return Err("ended with uploads or staging places left".to_owned());
    }
    Ok(())
}

enum Ended {
    Killed,
    ByItself,
}

fn run_child(dir: &Path, scenario: &str, fault: Option<(&str, u32)>) -> Ended {
    // The child aborts on purpose, hundreds of times. Where the kernel writes
    // core dumps beside the process (`kernel.core_pattern=core`), that would
    // be hundreds of files in this crate; so it runs with core dumps off.
    // `exec` makes the child the process that is waited for, signal and all.
    let mut command = Command::new("sh");
    command.args(["-c", "ulimit -c 0; exec \"$0\" \"$@\"", CHILD]);
    command.args(["script", dir.to_str().unwrap(), scenario]);
    command
        .env_remove("PASSALONG_FAULT")
        .env_remove("PASSALONG_FAULT_SKIP");
    if let Some((point, skip)) = fault {
        command
            .env("PASSALONG_FAULT", point)
            .env("PASSALONG_FAULT_SKIP", skip.to_string());
    }
    let output = command.output().unwrap();
    match output.status.code() {
        Some(0) => Ended::ByItself,
        // Aborted: no exit code, a signal.
        None => Ended::Killed,
        Some(code) => panic!(
            "{scenario}, {fault:?}: the child ended with {code}: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

/// Every passage of every fault point in one scenario. Returns how often
/// each point fired.
fn kill_everywhere(scenario: &'static Scenario) -> BTreeMap<&'static str, u32> {
    let mut fired = BTreeMap::new();
    for point in POINTS {
        for skip in 0.. {
            assert!(
                skip < 500,
                "{}: `{point}` never stops firing",
                scenario.name
            );
            let dir = tempfile::tempdir().unwrap();
            let what = format!("{}, killed at `{point}`, passage {skip}", scenario.name);
            match run_child(dir.path(), scenario.name, Some((point, skip))) {
                Ended::ByItself => {
                    ended_well(dir.path(), scenario)
                        .unwrap_or_else(|err| panic!("{}, unharmed: {err}", scenario.name));
                    break;
                }
                Ended::Killed => {}
            }
            *fired.entry(*point).or_insert(0) += 1;
            // A restarted server: opening repairs, and then all must hold.
            consistent(&open(dir.path(), &ManualClock::at(1_000)))
                .unwrap_or_else(|err| panic!("{what}: after the restart: {err}"));
            // The client sends its requests again.
            match run_child(dir.path(), scenario.name, None) {
                Ended::ByItself => {}
                Ended::Killed => panic!("{what}: the second run died too"),
            }
            ended_well(dir.path(), scenario)
                .unwrap_or_else(|err| panic!("{what}: after the second run: {err}"));
        }
    }
    fired
}

#[test]
fn every_scenario_survives_a_kill_at_every_passage_of_every_fault_point() {
    let runs: Vec<_> = SCENARIOS
        .iter()
        .map(|scenario| std::thread::spawn(move || (scenario.name, kill_everywhere(scenario))))
        .collect();
    let mut total: BTreeMap<&str, u32> = BTreeMap::new();
    for run in runs {
        let (name, fired) = run.join().unwrap();
        println!("kill harness, {name}: {fired:?}");
        for (point, count) in fired {
            *total.entry(point).or_insert(0) += count;
        }
    }
    println!("kill harness, all scenarios: {total:?}");
    for point in POINTS {
        assert!(
            total.get(point).copied().unwrap_or(0) > 0,
            "fault point `{point}` never fired"
        );
    }
}

#[test]
fn without_the_repair_a_kill_does_leave_the_books_and_the_shelf_apart() {
    // The control: if this passed with the repair switched off, the harness
    // above would prove nothing.
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        run_child(dir.path(), "upload", Some(("upload: after the publish", 0))),
        Ended::Killed
    ));
    let (shelf, ledger, workspace) = stores(dir.path());
    let unrepaired = Engine::open_without_repair(
        shelf,
        ledger,
        workspace,
        Arc::new(ManualClock::at(1_000)),
        Box::new(SeededRandom::new(1_234)),
        limits(),
    )
    .unwrap();
    let err = consistent(&unrepaired).unwrap_err();
    assert!(err.starts_with("I4"), "{err}");
    drop(unrepaired);
    consistent(&open(dir.path(), &ManualClock::at(1_000))).unwrap();
}
