//! The process the kill harness kills, and the workers of the two-process
//! test (PLAN-00002, REQ-06 and REQ-08). Test support: built only with the
//! `fault-injection` feature, never into the server.
//!
//! `storage_child script <dir> <scenario>` plays one scenario over the real
//! stores in `<dir>`, the way a client does that may have been interrupted
//! before: it looks at the workspace first and carries on from there. So
//! running it again after a kill *is* sending the interrupted requests
//! again. `storage_child worker <dir> <name> <count>` uploads in a loop,
//! and `storage_child rewriter <dir> <count>` begins and aborts rewrites,
//! both beside other processes.
//!
//! The "client" seals by prefixing, as in `tests/model.rs`: content is
//! `<key id or "plain">|<text>`.

use std::io::{Cursor, Read};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use passalong_server_core::clock::ManualClock;
use passalong_server_core::error::ApiError;
use passalong_server_core::ids::{ApiKeyId, ItemId, KeyId};
use passalong_server_core::ledger::{SqliteLedger, WorkspaceId};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::rewrite::{RewriteKind, RewriteRequest};
use passalong_server_core::shelf::FsShelf;
use passalong_server_core::upload::{Begun, UploadRequest};
use passalong_server_core::workspace::{Caller, EncryptionState, Engine, Limits, Partition, Role};

type Stores = Engine<FsShelf, SqliteLedger>;

const STAGING_SECS: u64 = 3_600;

fn caller(name: &str) -> Caller {
    Caller::new(ApiKeyId::new(name), Role::ReadWrite)
}

fn key(text: &str) -> KeyId {
    KeyId::parse(text).expect("a key id")
}

fn label(under: Option<&KeyId>) -> &str {
    under.map_or("plain", KeyId::as_str)
}

fn id_for(ts: u32, under: Option<&KeyId>, text: &str) -> ItemId {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in label(under).bytes().chain(*b"|").chain(text.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    ItemId::parse(&format!("{ts:08x}-{:012x}", hash & 0xffff_ffff_ffff)).expect("an item id")
}

fn seal(under: Option<&KeyId>, text: &str) -> Vec<u8> {
    format!("{}|{text}", label(under)).into_bytes()
}

fn open(dir: &Path, seed: u64, clock: &ManualClock) -> Result<Stores, ApiError> {
    let limits = Limits {
        staging_secs: STAGING_SECS,
        ..Limits::default()
    };
    Engine::open(
        FsShelf::open(dir.join("workspaces").join("00000000000000aa"))?,
        SqliteLedger::open(dir.join("control.sqlite"), Duration::from_secs(20))?,
        WorkspaceId::parse("00000000000000aa")?,
        Arc::new(clock.clone()),
        Box::new(SeededRandom::new(seed)),
        limits,
    )
}

/// One whole upload. `Ok(false)` when the content was there already.
fn put(
    engine: &mut Stores,
    who: &str,
    ts: u32,
    under: Option<&KeyId>,
    text: &str,
    in_rewrite: bool,
) -> Result<bool, ApiError> {
    let content = seal(under, text);
    let request = UploadRequest {
        id: id_for(ts, under, text),
        meta: b"{}".to_vec(),
        size: content.len() as u64,
        expected_key_id: under.cloned(),
        in_rewrite,
    };
    match engine.begin_upload(&caller(who), request)? {
        Begun::Stored(_) => Ok(false),
        Begun::Ticket(ticket) => {
            engine.put_upload_content(
                &caller(who),
                &ticket.upload_id,
                &mut Cursor::new(content),
            )?;
            Ok(engine
                .commit_upload(&caller(who), &ticket.upload_id)?
                .created)
        }
    }
}

fn text_of(engine: &Stores, id: &ItemId) -> Result<String, ApiError> {
    let mut bytes = Vec::new();
    let mut content = engine
        .item_content(Partition::Current, id)?
        .ok_or(ApiError::NotFound)?;
    content
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::ServiceUnavailable)?;
    let content = String::from_utf8(bytes).map_err(|_| ApiError::ServiceUnavailable)?;
    Ok(content
        .split_once('|')
        .map(|(_, text)| text.to_owned())
        .unwrap_or_default())
}

/// The client's rewrite engine: seal and stage what is missing, read every
/// staged item back, commit.
fn rewrite(
    engine: &mut Stores,
    kind: RewriteKind,
    old: Option<&KeyId>,
    new: &KeyId,
) -> Result<(), ApiError> {
    engine.begin_rewrite(
        &caller("a"),
        RewriteRequest {
            kind,
            expected_key_id: old.cloned(),
            new_key_id: new.clone(),
            new_header: format!("header-{}", new.as_str()).into_bytes(),
        },
    )?;
    let mut sources = engine.item_ids(Partition::Current)?;
    sources.reverse();
    for source in &sources {
        let text = text_of(engine, source)?;
        let ts = u32::from_str_radix(&source.as_str()[..8], 16).unwrap_or(0);
        put(engine, "a", ts, Some(new), &text, true)?;
        engine.heartbeat_rewrite(&caller("a"))?;
    }
    for staged in engine.staged_item_ids(&caller("a"))? {
        let mut bytes = Vec::new();
        let mut content = engine.staged_item_content(&caller("a"), &staged)?;
        content
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::ServiceUnavailable)?;
        if !bytes.starts_with(new.as_str().as_bytes()) {
            return Err(ApiError::ContentMismatch);
        }
    }
    engine.commit_rewrite(&caller("a"), new)?;
    Ok(())
}

fn script(dir: &Path, scenario: &str) -> Result<(), ApiError> {
    let clock = ManualClock::at(1_000);
    let mut engine = open(dir, 11, &clock)?;
    let (aa, bb) = (key("aa"), key("bb"));
    let view = engine.encryption()?;
    let plaintext = view.state == EncryptionState::Plaintext;
    match scenario {
        "upload" => {
            put(&mut engine, "a", 1, None, "one", false)?;
            put(&mut engine, "a", 2, None, "two", false)?;
        }
        "fresh-start" => {
            if plaintext {
                put(&mut engine, "a", 1, None, "one", false)?;
                put(&mut engine, "a", 2, None, "two", false)?;
                engine.fresh_start(&caller("a"), aa.clone(), b"header-aa".to_vec())?;
            }
            put(&mut engine, "a", 3, Some(&aa), "three", false)?;
        }
        "migrate" => {
            if view.key_id.as_ref() != Some(&bb) {
                if plaintext {
                    put(&mut engine, "a", 1, None, "one", false)?;
                    put(&mut engine, "a", 2, None, "two", false)?;
                }
                rewrite(&mut engine, RewriteKind::Migrate, None, &bb)?;
            }
            put(&mut engine, "a", 3, Some(&bb), "three", false)?;
        }
        "rotate" => {
            if view.key_id.as_ref() != Some(&bb) {
                if plaintext {
                    engine.enable_encryption(&caller("a"), aa.clone(), b"header-aa".to_vec())?;
                }
                if view.state != EncryptionState::Rewriting {
                    put(&mut engine, "a", 1, Some(&aa), "one", false)?;
                    put(&mut engine, "a", 2, Some(&aa), "two", false)?;
                }
                rewrite(&mut engine, RewriteKind::Rotate, Some(&aa), &bb)?;
            }
            put(&mut engine, "a", 3, Some(&bb), "three", false)?;
        }
        "abort" => {
            if plaintext {
                put(&mut engine, "a", 1, None, "one", false)?;
                put(&mut engine, "a", 2, None, "two", false)?;
            }
            let begun = engine.begin_rewrite(
                &caller("a"),
                RewriteRequest {
                    kind: RewriteKind::Migrate,
                    expected_key_id: None,
                    new_key_id: bb.clone(),
                    new_header: b"header-bb".to_vec(),
                },
            );
            match begun {
                // An earlier run aborted it already: nothing is left to do.
                Err(ApiError::RewriteEnded) => {}
                Err(err) => return Err(err),
                Ok(_) => {
                    put(&mut engine, "a", 1, Some(&bb), "one", true)?;
                    engine.abort_rewrite(&caller("a"), &bb)?;
                }
            }
        }
        "janitor" => {
            put(&mut engine, "a", 1, None, "one", false)?;
            let content = seal(None, "never finished");
            let request = UploadRequest {
                id: id_for(9, None, "never finished"),
                meta: b"{}".to_vec(),
                size: content.len() as u64,
                expected_key_id: None,
                in_rewrite: false,
            };
            if let Begun::Ticket(ticket) = engine.begin_upload(&caller("a"), request)? {
                engine.put_upload_content(
                    &caller("a"),
                    &ticket.upload_id,
                    &mut Cursor::new(content),
                )?;
            }
            clock.advance(STAGING_SECS + 1);
            engine.clean_staging()?;
        }
        _ => {
            return Err(ApiError::InvalidRequest(format!(
                "no scenario `{scenario}`"
            )));
        }
    }
    Ok(())
}

/// Uploads `count` texts, some of which every worker uploads, beside other
/// processes. A rewrite in progress is waited out, as `serve` does.
fn worker(dir: &Path, name: &str, count: u32) -> Result<(), ApiError> {
    let seed = name.bytes().map(u64::from).sum::<u64>() + u64::from(std::process::id());
    let mut engine = open(dir, seed, &ManualClock::at(1_000))?;
    for n in 0..count {
        let (ts, text) = if n % 2 == 0 {
            (n, format!("shared {n}"))
        } else {
            (n, format!("{name} {n}"))
        };
        loop {
            match put(&mut engine, name, ts, None, &text, false) {
                Ok(_) => break,
                Err(ApiError::RewriteInProgress) => std::thread::sleep(Duration::from_millis(2)),
                Err(err) => return Err(err),
            }
        }
    }
    Ok(())
}

/// Begins and aborts `count` migrations, each under a key of its own.
fn rewriter(dir: &Path, count: u32) -> Result<(), ApiError> {
    let mut engine = open(dir, 5, &ManualClock::at(1_000))?;
    for n in 0..count {
        let new = key(&format!("{:04x}", 0xb000 + n));
        engine.begin_rewrite(
            &caller("rewriter"),
            RewriteRequest {
                kind: RewriteKind::Migrate,
                expected_key_id: None,
                new_key_id: new.clone(),
                new_header: b"header".to_vec(),
            },
        )?;
        std::thread::sleep(Duration::from_millis(3));
        engine.abort_rewrite(&caller("rewriter"), &new)?;
        std::thread::sleep(Duration::from_millis(3));
    }
    Ok(())
}

/// Prints warnings and errors to standard error, so that a failure of the
/// harness says what the stores said.
struct Stderr;

struct Line(String);

impl tracing::field::Visit for Line {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl tracing::Subscriber for Stderr {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = Line(format!(
            "{} {}",
            event.metadata().level(),
            event.metadata().target()
        ));
        event.record(&mut line);
        eprintln!("{}", line.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn main() -> ExitCode {
    let _ = tracing::subscriber::set_global_default(Stderr);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let done = match args.as_slice() {
        ["script", dir, scenario] => script(Path::new(dir), scenario),
        ["worker", dir, name, count] => worker(Path::new(dir), name, count.parse().unwrap_or(0)),
        ["rewriter", dir, count] => rewriter(Path::new(dir), count.parse().unwrap_or(0)),
        _ => Err(ApiError::InvalidRequest(
            "usage: script|worker|rewriter …".to_owned(),
        )),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The harness reads this: a refusal the contract names, or not.
            eprintln!("storage_child: {} ({err})", err.code());
            ExitCode::from(3)
        }
    }
}
