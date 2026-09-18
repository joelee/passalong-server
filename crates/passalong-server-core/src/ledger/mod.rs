//! What a workspace's engine remembers between requests, and where it is
//! kept: the control database, behind the [`Ledger`] trait.

//!
//! The ledger is also the workspace lock. A transaction is exclusive, across
//! threads and, with SQLite, across processes, so whatever the rules do
//! inside one, the shelf steps included, no other writer sees half of. That
//! is why a rule runs its shelf step *inside* the transaction, before the
//! record is written: if the process dies in between, the shelf holds
//! something the record does not know, which reconciliation removes or a
//! repeated request completes. The reverse, a record of something that is
//! not there, cannot happen.

mod memory;
mod sqlite;

pub use crate::ids::WorkspaceId;
pub use memory::MemoryLedger;
pub(crate) use sqlite::open_connection;
pub use sqlite::{SCHEMA_VERSION, SqliteLedger};

use std::collections::{BTreeMap, BTreeSet};

use crate::error::ApiError;
use crate::ids::KeyId;
use crate::upload::Uploads;
use crate::workspace::State;

/// Everything the rules remember about one workspace. The items themselves
/// are not here: the shelf is their only record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    pub(crate) state: State,
    /// The generation that holds the workspace's items.
    pub(crate) generation: u64,
    /// The generation a fresh start set aside, if it held anything.
    pub(crate) plain: Option<u64>,
    pub(crate) next_generation: u64,
    /// Bytes published per live generation, as the quota counts them.
    pub(crate) used: BTreeMap<u64, u64>,
    pub(crate) uploads: Uploads,
    /// The new key ids of aborted rewrites, kept for good.
    pub(crate) ended_rewrites: BTreeSet<KeyId>,
}

impl Default for WorkspaceRecord {
    /// A new workspace: plaintext and empty.
    fn default() -> Self {
        Self {
            state: State::Settled(None),
            generation: 0,
            plain: None,
            next_generation: 1,
            used: BTreeMap::new(),
            uploads: Uploads::default(),
            ended_rewrites: BTreeSet::new(),
        }
    }
}

/// A rule run inside a transaction. A ledger calls it at most once.
pub type Rule<'a, T> = &'a mut dyn FnMut(&mut WorkspaceRecord) -> Result<T, ApiError>;

/// The control database, as the rules need it.
pub trait Ledger {
    /// Creates a workspace's record; harmless when it exists.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`] when the ledger cannot be used.
    fn create(&self, workspace: &WorkspaceId) -> Result<(), ApiError>;

    /// A copy of a workspace's record, for reading.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] for an unknown workspace, and
    /// [`ApiError::ServiceUnavailable`].
    fn load(&self, workspace: &WorkspaceId) -> Result<WorkspaceRecord, ApiError>;

    /// Runs `rule` on the workspace's record, alone: no other transaction
    /// runs meanwhile. When the rule succeeds, the record as it left it is
    /// stored, whole; when it is refused, the record stays as it was.
    ///
    /// # Errors
    ///
    /// The rule's refusal, [`ApiError::NotFound`] for an unknown workspace,
    /// and [`ApiError::ServiceUnavailable`].
    fn transact<T>(&self, workspace: &WorkspaceId, rule: Rule<'_, T>) -> Result<T, ApiError>;
}

/// A ledger shared between the engines of many workspaces.
impl<L: Ledger> Ledger for std::sync::Arc<L> {
    fn create(&self, workspace: &WorkspaceId) -> Result<(), ApiError> {
        (**self).create(workspace)
    }
    fn load(&self, workspace: &WorkspaceId) -> Result<WorkspaceRecord, ApiError> {
        (**self).load(workspace)
    }
    fn transact<T>(&self, workspace: &WorkspaceId, rule: Rule<'_, T>) -> Result<T, ApiError> {
        (**self).transact(workspace, rule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiError;
    use crate::ids::{ApiKeyId, ItemId, UploadId};
    use crate::random::SeededRandom;
    use crate::rewrite::{RewriteKind, Session};
    use crate::upload::{PutOutcome, Ticket, Tombstone, UploadRequest};
    use crate::workspace::Seal;

    fn ids() -> (WorkspaceId, WorkspaceId) {
        let mut rng = SeededRandom::new(5);
        (
            WorkspaceId::generate(&mut rng),
            WorkspaceId::generate(&mut rng),
        )
    }

    /// A record with something in every field.
    fn full_record() -> WorkspaceRecord {
        let mut rng = SeededRandom::new(8);
        let key = |text: &str| KeyId::parse(text).unwrap();
        let mut record = WorkspaceRecord {
            state: State::Rewriting(Session {
                kind: RewriteKind::Rotate,
                prior: Some(Seal {
                    key_id: key("aa"),
                    header: vec![0, 159, 255],
                }),
                next: Seal {
                    key_id: key("bb"),
                    header: b"{\"wrapped\":1}".to_vec(),
                },
                holder: ApiKeyId::new("7f3k9q2m"),
                lease_expires_at: 1_600,
                staged_generation: 5,
            }),
            generation: 4,
            plain: Some(2),
            next_generation: 6,
            ..WorkspaceRecord::default()
        };
        record
            .used
            .extend([(2, 10), (4, u64::from(u32::MAX) * 8), (5, 0)]);
        record.ended_rewrites.extend([key("cc"), key("dd")]);
        for (n, in_rewrite) in [(1_u64, false), (2, true)] {
            record.uploads.tickets.insert(
                UploadId::generate(&mut rng),
                Ticket {
                    owner: ApiKeyId::new("laptop"),
                    request: UploadRequest {
                        id: ItemId::parse(&format!("0000000{n}-aaaaaaaaaaaa")).unwrap(),
                        meta: vec![123, 0, 200, 125],
                        size: n * 1_000,
                        expected_key_id: in_rewrite.then(|| key("bb")),
                        in_rewrite,
                    },
                    expires_at: 90_000 + n,
                },
            );
        }
        record.uploads.tombstones.insert(
            UploadId::generate(&mut rng),
            Tombstone {
                owner: ApiKeyId::new("phone"),
                outcome: PutOutcome {
                    id: ItemId::parse("00000009-bbbbbbbbbbbb").unwrap(),
                    created: true,
                },
                expires_at: 99_999,
            },
        );
        record
    }

    #[test]
    fn a_workspace_id_is_sixteen_hex_digits_minted_by_the_server() {
        let (a, b) = ids();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 16);
        assert_eq!(WorkspaceId::parse(a.as_str()).unwrap(), a);
        for bad in ["", "../etc", "ABCDEF0123456789", "0123456789abcdef0"] {
            assert_eq!(
                WorkspaceId::parse(bad).unwrap_err().code(),
                "INVALID_ID",
                "{bad:?}"
            );
        }
    }

    // ---------- the conformance suite: every ledger, the same behaviour ----------

    fn a_transaction_is_all_or_nothing<L: Ledger>(ledger: &L) {
        let (a, _) = ids();
        assert_eq!(ledger.load(&a).unwrap_err(), ApiError::NotFound);
        ledger.create(&a).unwrap();
        ledger.create(&a).unwrap();
        assert_eq!(ledger.load(&a).unwrap(), WorkspaceRecord::default());

        let answer = ledger.transact(&a, &mut |record| {
            record.generation = 7;
            Ok("done")
        });
        assert_eq!(answer.unwrap(), "done");
        assert_eq!(ledger.load(&a).unwrap().generation, 7);

        let refused: Result<(), _> = ledger.transact(&a, &mut |record| {
            *record = full_record();
            Err(ApiError::QuotaExceeded)
        });
        assert_eq!(refused.unwrap_err(), ApiError::QuotaExceeded);
        let kept = ledger.load(&a).unwrap();
        assert_eq!(kept.generation, 7);
        assert!(kept.uploads.tickets.is_empty());
        // Creating again never resets what is there.
        ledger.create(&a).unwrap();
        assert_eq!(ledger.load(&a).unwrap().generation, 7);
    }

    fn workspaces_do_not_see_each_other<L: Ledger>(ledger: &L) {
        let (a, b) = ids();
        ledger.create(&a).unwrap();
        ledger.create(&b).unwrap();
        ledger
            .transact(&a, &mut |record| {
                *record = full_record();
                Ok(())
            })
            .unwrap();
        assert_eq!(ledger.load(&b).unwrap(), WorkspaceRecord::default());
        assert_eq!(ledger.load(&a).unwrap(), full_record());

        let unknown = WorkspaceId::generate(&mut SeededRandom::new(99));
        let gone: Result<(), _> = ledger.transact(&unknown, &mut |_| Ok(()));
        assert_eq!(gone.unwrap_err(), ApiError::NotFound);
    }

    fn every_field_comes_back_and_goes_away_again<L: Ledger>(ledger: &L) {
        let (a, _) = ids();
        ledger.create(&a).unwrap();
        ledger
            .transact(&a, &mut |record| {
                *record = full_record();
                Ok(())
            })
            .unwrap();
        assert_eq!(ledger.load(&a).unwrap(), full_record());
        // Settled and plaintext again, with nothing pending: rows must go,
        // not only come.
        let plain = WorkspaceRecord {
            generation: 9,
            next_generation: 10,
            ..WorkspaceRecord::default()
        };
        let expect = plain.clone();
        ledger
            .transact(&a, &mut |record| {
                *record = plain.clone();
                Ok(())
            })
            .unwrap();
        assert_eq!(ledger.load(&a).unwrap(), expect);
    }

    macro_rules! ledger_suite {
        ($module:ident, $make:expr) => {
            mod $module {
                use super::*;
                #[test]
                fn a_transaction_is_all_or_nothing() {
                    let (ledger, _guard) = $make;
                    super::a_transaction_is_all_or_nothing(&ledger);
                }
                #[test]
                fn workspaces_do_not_see_each_other() {
                    let (ledger, _guard) = $make;
                    super::workspaces_do_not_see_each_other(&ledger);
                }
                #[test]
                fn every_field_comes_back_and_goes_away_again() {
                    let (ledger, _guard) = $make;
                    super::every_field_comes_back_and_goes_away_again(&ledger);
                }
            }
        };
    }

    fn sqlite() -> (SqliteLedger, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = SqliteLedger::open(dir.path().join("control.sqlite"), TIMEOUT).unwrap();
        (ledger, dir)
    }

    const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

    ledger_suite!(memory, (MemoryLedger::default(), ()));
    ledger_suite!(sqlite, sqlite());

    // ---------- what only a database can get wrong ----------

    #[test]
    fn what_one_connection_stores_another_reads_also_after_reopening() {
        let (first, dir) = sqlite();
        let path = dir.path().join("control.sqlite");
        let second = SqliteLedger::open(&path, TIMEOUT).unwrap();
        let (a, _) = ids();
        first.create(&a).unwrap();
        first
            .transact(&a, &mut |record| {
                *record = full_record();
                Ok(())
            })
            .unwrap();
        assert_eq!(second.load(&a).unwrap(), full_record());
        drop((first, second));
        let reopened = SqliteLedger::open(&path, TIMEOUT).unwrap();
        assert_eq!(reopened.load(&a).unwrap(), full_record());
    }

    #[cfg(unix)]
    #[test]
    fn the_database_is_its_owners_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_ledger, dir) = sqlite();
        let mode = std::fs::metadata(dir.path().join("control.sqlite"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_damaged_database_is_refused_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sqlite");
        std::fs::write(
            &path,
            b"this is not a database, and it is longer than a header",
        )
        .unwrap();
        let err = SqliteLedger::open(&path, TIMEOUT).err().unwrap();
        assert_eq!(err, ApiError::ServiceUnavailable);
        assert!(std::fs::read(&path).unwrap().starts_with(b"this is not"));
    }

    #[test]
    fn a_database_from_a_newer_server_is_refused() {
        let (ledger, dir) = sqlite();
        drop(ledger);
        let path = dir.path().join("control.sqlite");
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("UPDATE schema_version SET version = version + 1", [])
            .unwrap();
        drop(raw);
        assert_eq!(
            SqliteLedger::open(&path, TIMEOUT).err().unwrap(),
            ApiError::ServiceUnavailable
        );
    }

    // ---------- migrations ----------

    const SCHEMA_V1: &str = include_str!("../../tests/fixtures/schema_v1.sql");

    /// A database as PLAN-00002's server left it, holding `record`.
    fn version_one(path: &std::path::Path, workspace: &WorkspaceId, record: &WorkspaceRecord) {
        let mut raw = rusqlite::Connection::open(path).unwrap();
        raw.execute_batch(SCHEMA_V1).unwrap();
        raw.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        raw.execute(
            "INSERT INTO workspaces (id, generation, next_generation) VALUES (?1, 0, 1)",
            [workspace.as_str()],
        )
        .unwrap();
        let tx = raw.transaction().unwrap();
        // Version 1 stored a record with the statements that still do.
        super::sqlite::write_for_tests(&tx, workspace, record);
        tx.commit().unwrap();
    }

    fn version_of(path: &std::path::Path) -> i64 {
        let raw = rusqlite::Connection::open(path).unwrap();
        raw.query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn a_version_one_database_is_migrated_with_every_record_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sqlite");
        let (a, b) = ids();
        version_one(&path, &a, &full_record());
        {
            let raw = rusqlite::Connection::open(&path).unwrap();
            raw.execute(
                "INSERT INTO workspaces (id, generation, next_generation) VALUES (?1, 0, 1)",
                [b.as_str()],
            )
            .unwrap();
        }
        assert_eq!(version_of(&path), 1);

        let ledger = SqliteLedger::open(&path, TIMEOUT).unwrap();
        assert_eq!(version_of(&path), super::sqlite::SCHEMA_VERSION);
        const { assert!(super::sqlite::SCHEMA_VERSION >= 2) };
        assert_eq!(ledger.load(&a).unwrap(), full_record());
        assert_eq!(ledger.load(&b).unwrap(), WorkspaceRecord::default());
        // A workspace that had no name gets its id as one.
        let raw = rusqlite::Connection::open(&path).unwrap();
        let name: String = raw
            .query_row(
                "SELECT name FROM workspaces WHERE id = ?1",
                [a.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, a.as_str());
        // And it still works as a ledger.
        ledger
            .transact(&a, &mut |record| {
                record.generation = 40;
                Ok(())
            })
            .unwrap();
        assert_eq!(ledger.load(&a).unwrap().generation, 40);
    }

    #[test]
    fn a_current_database_reopens_unchanged() {
        let (ledger, dir) = sqlite();
        let (a, _) = ids();
        ledger.create(&a).unwrap();
        ledger
            .transact(&a, &mut |record| {
                *record = full_record();
                Ok(())
            })
            .unwrap();
        drop(ledger);
        let path = dir.path().join("control.sqlite");
        let before = std::fs::read(&path).unwrap().len();
        let ledger = SqliteLedger::open(&path, TIMEOUT).unwrap();
        assert_eq!(ledger.load(&a).unwrap(), full_record());
        assert_eq!(version_of(&path), super::sqlite::SCHEMA_VERSION);
        assert_eq!(std::fs::read(&path).unwrap().len(), before);
    }

    #[test]
    fn a_migration_that_fails_leaves_the_database_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sqlite");
        let (a, _) = ids();
        version_one(&path, &a, &full_record());
        let mut raw = rusqlite::Connection::open(&path).unwrap();
        let broken = [
            "(version 1, already there)",
            "ALTER TABLE workspaces ADD COLUMN half_done TEXT; THIS IS NOT SQL;",
        ];
        assert_eq!(
            super::sqlite::migrate_with(&mut raw, &broken).unwrap_err(),
            ApiError::ServiceUnavailable
        );
        assert_eq!(version_of(&path), 1);
        let columns: i64 = raw
            .query_row(
                "SELECT count(*) FROM pragma_table_info('workspaces') WHERE name = 'half_done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(columns, 0, "the failed step was rolled back");
        drop(raw);
        // The real migration then succeeds.
        assert_eq!(
            SqliteLedger::open(&path, TIMEOUT)
                .unwrap()
                .load(&a)
                .unwrap(),
            full_record()
        );
    }

    #[test]
    fn a_lock_held_past_the_timeout_fails_closed() {
        let (ledger, dir) = sqlite();
        let (a, _) = ids();
        ledger.create(&a).unwrap();
        let blocker = rusqlite::Connection::open(dir.path().join("control.sqlite")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let refused: Result<(), _> = ledger.transact(&a, &mut |record| {
            record.generation = 1;
            Ok(())
        });
        assert_eq!(refused.unwrap_err(), ApiError::ServiceUnavailable);
        blocker.execute_batch("ROLLBACK").unwrap();
        // Once the lock is gone the ledger works again, and nothing was lost.
        assert_eq!(ledger.load(&a).unwrap().generation, 0);
        ledger
            .transact(&a, &mut |record| {
                record.generation = 1;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn many_openers_of_a_new_database_all_succeed() {
        // Found by the two-process test: switching a database to WAL needs a
        // moment alone with it, and SQLite does not apply the busy timeout
        // to that, so of several processes opening a new database at once
        // some were told "database is locked". The CLI starting beside the
        // server would be one of them.
        for _ in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("control.sqlite");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
            let openers: Vec<_> = (0..8)
                .map(|_| {
                    let (path, barrier) = (path.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        SqliteLedger::open(&path, std::time::Duration::from_secs(10)).map(|_| ())
                    })
                })
                .collect();
            for opener in openers {
                assert_eq!(opener.join().unwrap(), Ok(()));
            }
        }
    }

    #[test]
    fn writers_queue_instead_of_failing() {
        let (ledger, dir) = sqlite();
        let (a, _) = ids();
        ledger.create(&a).unwrap();
        let path = dir.path().join("control.sqlite");
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let (path, a) = (path.clone(), a.clone());
                std::thread::spawn(move || {
                    let ledger =
                        SqliteLedger::open(&path, std::time::Duration::from_secs(10)).unwrap();
                    for _ in 0..25 {
                        ledger
                            .transact(&a, &mut |record| {
                                record.next_generation += 1;
                                Ok(())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        assert_eq!(ledger.load(&a).unwrap().next_generation, 101);
    }
}
