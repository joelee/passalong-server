//! The control database: SQLite, in WAL mode, flushed at every commit.
//!
//! Every transaction begins `IMMEDIATE`, which takes SQLite's write lock at
//! once. Writers therefore queue, in this process and in any other that has
//! the file open (the operations CLI will be one), for as long as the busy
//! timeout; and since the rules run *inside* the transaction, that lock is
//! the workspace lock across processes. SQLite has one writer per database,
//! so workspaces queue behind each other too; a transaction is a few row
//! writes and at most one rename, so the queue is short.
//!
//! Whatever SQLite reports, the caller sees
//! [`ApiError::ServiceUnavailable`]: the server fails closed, and the detail
//! goes to the log.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{Ledger, Rule, WorkspaceId, WorkspaceRecord};
use crate::error::ApiError;
use crate::ids::{ApiKeyId, ItemId, KeyId, UploadId};
use crate::rewrite::{RewriteKind, Session};
use crate::upload::{PutOutcome, Ticket, Tombstone, UploadRequest, Uploads};
use crate::workspace::{Seal, State};

/// The schema this build reads and writes. An older database is migrated
/// when it is opened; one that says more is from a newer server and is
/// refused.
pub const SCHEMA_VERSION: i64 = crate::control::schema::STEPS.len() as i64;

/// A [`Ledger`] in a SQLite file.
#[derive(Debug)]
pub struct SqliteLedger {
    connection: Mutex<Connection>,
    path: PathBuf,
}

/// Logs what went wrong and reports only that the ledger cannot be used.
fn unavailable(action: &'static str, err: &dyn std::fmt::Display) -> ApiError {
    tracing::error!(target: "passalong_server::ledger", action, %err, "the control database cannot be used");
    ApiError::ServiceUnavailable
}

/// What a row held does not parse: the database was written by something
/// else, or is damaged.
fn damaged(what: &'static str) -> ApiError {
    unavailable("read a record", &format!("{what} does not parse"))
}

fn to_db(number: u64) -> Result<i64, ApiError> {
    i64::try_from(number).map_err(|_| unavailable("store a number", &"larger than SQLite holds"))
}

fn from_db(number: i64) -> Result<u64, ApiError> {
    u64::try_from(number).map_err(|_| damaged("a negative number"))
}

fn key_from(text: Option<String>) -> Result<Option<KeyId>, ApiError> {
    text.map(|text| KeyId::parse(&text).map_err(|_| damaged("a key id")))
        .transpose()
}

/// Puts the database into WAL mode, which is recorded in the file, so only
/// its first opener has to. That needs a moment alone with the database, and
/// SQLite answers "locked" at once instead of waiting as the busy timeout
/// says; several processes opening a new database together, the server and
/// the CLI for instance, would otherwise fail. So this waits itself.
fn use_wal(connection: &Connection, patience: Duration) -> Result<(), ApiError> {
    let deadline = std::time::Instant::now() + patience;
    loop {
        let mode: Result<String, _> =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get(0));
        if mode
            .as_deref()
            .is_ok_and(|mode| mode.eq_ignore_ascii_case("wal"))
        {
            return Ok(());
        }
        let switched: Result<String, _> =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0));
        match switched {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) => {
                return Err(unavailable(
                    "switch to WAL",
                    &format!("the database stays in {mode} mode"),
                ));
            }
            Err(err) if is_busy(&err) && std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => return Err(unavailable("switch to WAL", &err)),
        }
    }
}

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

impl SqliteLedger {
    /// Opens the database at `path`, creating it, readable by its owner
    /// only, when it is new. `busy_timeout` is how long a transaction waits
    /// for another to finish before it fails closed.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`] for a file that is not a database,
    /// is damaged, or was written by a newer server. Such a file is left as
    /// it is.
    pub fn open(path: impl AsRef<Path>, busy_timeout: Duration) -> Result<Self, ApiError> {
        let path = path.as_ref().to_path_buf();
        let connection = open_connection(&path, busy_timeout)?;
        Ok(Self {
            connection: Mutex::new(connection),
            path,
        })
    }

    /// Where the database is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A connection to the control database at `path`: created 0600 when new,
/// in WAL mode, flushed at every commit, and at the current schema version.
/// The ledger and [`Control`](crate::control::Control) each open one.
pub(crate) fn open_connection(path: &Path, busy_timeout: Duration) -> Result<Connection, ApiError> {
    // SQLite would create the file with the process's umask.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|err| unavailable("create the database file", &err))?;
    let mut connection =
        Connection::open(path).map_err(|err| unavailable("open the database", &err))?;
    connection
        .busy_timeout(busy_timeout)
        .map_err(|err| unavailable("set the busy timeout", &err))?;
    use_wal(&connection, busy_timeout)?;
    connection
        .execute_batch("PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;")
        .map_err(|err| unavailable("configure the database", &err))?;
    migrate_with(&mut connection, crate::control::schema::STEPS)?;
    Ok(connection)
}

/// Brings the database to version `steps.len()`, running the steps it has
/// not had yet, all in one transaction: either every step takes, or the
/// database stays exactly as it was.
pub(crate) fn migrate_with(connection: &mut Connection, steps: &[&str]) -> Result<(), ApiError> {
    let latest = steps.len() as i64;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|err| unavailable("begin the schema check", &err))?;
    let has_version: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_version')",
            [],
            |row| row.get(0),
        )
        .map_err(|err| unavailable("read the schema", &err))?;
    let version: i64 = if has_version {
        tx.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .map_err(|err| unavailable("read the schema version", &err))?
    } else {
        0
    };
    if version > latest || version < 0 {
        return Err(unavailable(
            "check the schema version",
            &format!("the database has schema {version}, this server {latest}"),
        ));
    }
    if version == latest {
        return Ok(());
    }
    for step in &steps[version as usize..] {
        tx.execute_batch(step)
            .map_err(|err| unavailable("migrate the schema", &err))?;
    }
    let recorded = if has_version {
        tx.execute("UPDATE schema_version SET version = ?1", [latest])
    } else {
        tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [latest])
    };
    recorded.map_err(|err| unavailable("record the schema version", &err))?;
    tx.commit()
        .map_err(|err| unavailable("commit the migration", &err))?;
    tracing::info!(target: "passalong_server::ledger", from = version, to = latest, "control database schema brought up to date");
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_for_tests(
    tx: &Transaction<'_>,
    workspace: &WorkspaceId,
    record: &WorkspaceRecord,
) {
    write(tx, workspace, record).unwrap();
}

fn read(tx: &Connection, workspace: &WorkspaceId) -> Result<WorkspaceRecord, ApiError> {
    type Row = (
        i64,
        Option<i64>,
        i64,
        Option<String>,
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<Vec<u8>>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    );
    let sql = |err: rusqlite::Error| unavailable("read a record", &err);
    let row: Option<Row> = tx
        .query_row(
            "SELECT generation, plain, next_generation, seal_key, seal_header, rw_kind,
                    rw_next_key, rw_next_header, rw_holder, rw_lease_expires_at, rw_staged_generation
             FROM workspaces WHERE id = ?1",
            [workspace.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ))
            },
        )
        .optional()
        .map_err(sql)?;
    let Some((
        generation,
        plain,
        next,
        seal_key,
        seal_header,
        kind,
        next_key,
        next_header,
        holder,
        lease,
        staged,
    )) = row
    else {
        return Err(ApiError::NotFound);
    };
    let seal = match (key_from(seal_key)?, seal_header) {
        (Some(key_id), Some(header)) => Some(Seal { key_id, header }),
        (None, None) => None,
        _ => return Err(damaged("a seal")),
    };
    let state = match kind.as_deref() {
        None => State::Settled(seal),
        Some(kind) => State::Rewriting(Session {
            kind: match kind {
                "migrate" => RewriteKind::Migrate,
                "rotate" => RewriteKind::Rotate,
                _ => return Err(damaged("a rewrite kind")),
            },
            prior: seal,
            next: Seal {
                key_id: key_from(next_key)?.ok_or_else(|| damaged("a session"))?,
                header: next_header.ok_or_else(|| damaged("a session"))?,
            },
            holder: ApiKeyId::new(holder.ok_or_else(|| damaged("a session"))?),
            lease_expires_at: from_db(lease.ok_or_else(|| damaged("a session"))?)?,
            staged_generation: from_db(staged.ok_or_else(|| damaged("a session"))?)?,
        }),
    };

    let mut used = BTreeMap::new();
    let mut statement = tx
        .prepare("SELECT generation, used_bytes FROM generations WHERE workspace = ?1")
        .map_err(sql)?;
    let rows = statement
        .query_map([workspace.as_str()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(sql)?;
    for row in rows {
        let (generation, bytes) = row.map_err(sql)?;
        used.insert(from_db(generation)?, from_db(bytes)?);
    }

    let mut uploads = Uploads::default();
    let mut statement = tx
        .prepare(
            "SELECT upload_id, owner, item_id, meta, size, expected_key, in_rewrite, expires_at
             FROM uploads WHERE workspace = ?1",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map([workspace.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .map_err(sql)?;
    for row in rows {
        let (upload, owner, item, meta, size, expected, in_rewrite, expires) = row.map_err(sql)?;
        uploads.tickets.insert(
            UploadId::parse(&upload).map_err(|_| damaged("an upload id"))?,
            Ticket {
                owner: ApiKeyId::new(owner),
                request: UploadRequest {
                    id: ItemId::parse(&item).map_err(|_| damaged("an item id"))?,
                    meta,
                    size: from_db(size)?,
                    expected_key_id: key_from(expected)?,
                    in_rewrite,
                },
                expires_at: from_db(expires)?,
            },
        );
    }

    let mut statement = tx
        .prepare("SELECT upload_id, owner, item_id, created, expires_at, staged FROM tombstones WHERE workspace = ?1")
        .map_err(sql)?;
    let rows = statement
        .query_map([workspace.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })
        .map_err(sql)?;
    for row in rows {
        let (upload, owner, item, created, expires, staged) = row.map_err(sql)?;
        uploads.tombstones.insert(
            UploadId::parse(&upload).map_err(|_| damaged("an upload id"))?,
            Tombstone {
                owner: ApiKeyId::new(owner),
                outcome: PutOutcome {
                    id: ItemId::parse(&item).map_err(|_| damaged("an item id"))?,
                    created,
                    staged,
                },
                expires_at: from_db(expires)?,
            },
        );
    }

    let mut ended_rewrites = BTreeSet::new();
    let mut statement = tx
        .prepare("SELECT key_id FROM ended_rewrites WHERE workspace = ?1")
        .map_err(sql)?;
    let rows = statement
        .query_map([workspace.as_str()], |row| row.get::<_, String>(0))
        .map_err(sql)?;
    for row in rows {
        ended_rewrites.insert(KeyId::parse(&row.map_err(sql)?).map_err(|_| damaged("a key id"))?);
    }

    Ok(WorkspaceRecord {
        state,
        generation: from_db(generation)?,
        plain: plain.map(from_db).transpose()?,
        next_generation: from_db(next)?,
        used,
        uploads,
        ended_rewrites,
    })
}

/// Stores `record` whole: the workspace's row is updated and its other rows
/// are replaced. They are few: one per live generation and per upload in
/// flight.
fn write(
    tx: &Transaction<'_>,
    workspace: &WorkspaceId,
    record: &WorkspaceRecord,
) -> Result<(), ApiError> {
    let sql = |err: rusqlite::Error| unavailable("store a record", &err);
    let id = workspace.as_str();
    let (seal, session) = match &record.state {
        State::Settled(seal) => (seal.as_ref(), None),
        State::Rewriting(session) => (session.prior.as_ref(), Some(session)),
    };
    tx.execute(
        "UPDATE workspaces SET generation = ?2, plain = ?3, next_generation = ?4, seal_key = ?5,
                seal_header = ?6, rw_kind = ?7, rw_next_key = ?8, rw_next_header = ?9, rw_holder = ?10,
                rw_lease_expires_at = ?11, rw_staged_generation = ?12
         WHERE id = ?1",
        params![
            id,
            to_db(record.generation)?,
            record.plain.map(to_db).transpose()?,
            to_db(record.next_generation)?,
            seal.map(|seal| seal.key_id.as_str()),
            seal.map(|seal| seal.header.as_slice()),
            session.map(|session| match session.kind {
                RewriteKind::Migrate => "migrate",
                RewriteKind::Rotate => "rotate",
            }),
            session.map(|session| session.next.key_id.as_str()),
            session.map(|session| session.next.header.as_slice()),
            session.map(|session| session.holder.as_str()),
            session.map(|session| to_db(session.lease_expires_at)).transpose()?,
            session.map(|session| to_db(session.staged_generation)).transpose()?,
        ],
    )
    .map_err(sql)?;
    for table in ["generations", "uploads", "tombstones", "ended_rewrites"] {
        tx.execute(&format!("DELETE FROM {table} WHERE workspace = ?1"), [id])
            .map_err(sql)?;
    }
    for (generation, bytes) in &record.used {
        tx.execute(
            "INSERT INTO generations (workspace, generation, used_bytes) VALUES (?1, ?2, ?3)",
            params![id, to_db(*generation)?, to_db(*bytes)?],
        )
        .map_err(sql)?;
    }
    for (upload, ticket) in &record.uploads.tickets {
        tx.execute(
            "INSERT INTO uploads (workspace, upload_id, owner, item_id, meta, size, expected_key, in_rewrite, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                upload.as_str(),
                ticket.owner.as_str(),
                ticket.request.id.as_str(),
                ticket.request.meta,
                to_db(ticket.request.size)?,
                ticket.request.expected_key_id.as_ref().map(KeyId::as_str),
                ticket.request.in_rewrite,
                to_db(ticket.expires_at)?,
            ],
        )
        .map_err(sql)?;
    }
    for (upload, stone) in &record.uploads.tombstones {
        tx.execute(
            "INSERT INTO tombstones (workspace, upload_id, owner, item_id, created, expires_at, staged)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                upload.as_str(),
                stone.owner.as_str(),
                stone.outcome.id.as_str(),
                stone.outcome.created,
                to_db(stone.expires_at)?,
                stone.outcome.staged,
            ],
        )
        .map_err(sql)?;
    }
    for key in &record.ended_rewrites {
        tx.execute(
            "INSERT INTO ended_rewrites (workspace, key_id) VALUES (?1, ?2)",
            params![id, key.as_str()],
        )
        .map_err(sql)?;
    }
    Ok(())
}

impl Ledger for SqliteLedger {
    fn create(&self, workspace: &WorkspaceId) -> Result<(), ApiError> {
        let fresh = WorkspaceRecord::default();
        self.lock()
            .execute(
                "INSERT OR IGNORE INTO workspaces (id, generation, next_generation) VALUES (?1, ?2, ?3)",
                params![workspace.as_str(), to_db(fresh.generation)?, to_db(fresh.next_generation)?],
            )
            .map(|_| ())
            .map_err(|err| unavailable("create a workspace", &err))
    }

    fn load(&self, workspace: &WorkspaceId) -> Result<WorkspaceRecord, ApiError> {
        read(&self.lock(), workspace)
    }

    fn transact<T>(&self, workspace: &WorkspaceId, rule: Rule<'_, T>) -> Result<T, ApiError> {
        let mut connection = self.lock();
        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| unavailable("begin a transaction", &err))?;
        let before = read(&tx, workspace)?;
        let mut record = before.clone();
        // A refusal drops `tx`, which rolls back; nothing was written anyway.
        let answer = rule(&mut record)?;
        if record != before {
            write(&tx, workspace, &record)?;
        }
        crate::fault::point("ledger: before the commit");
        tx.commit()
            .map_err(|err| unavailable("commit a transaction", &err))?;
        Ok(answer)
    }
}
