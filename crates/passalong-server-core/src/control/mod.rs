//! The control database beyond the ledger: workspaces as an operator sees
//! them, API keys, and the audit trail.

pub(crate) mod schema;

use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::auth::{ApiKey, SecretHash};
use crate::clock::Clock;
use crate::error::ApiError;
use crate::ids::{ApiKeyId, WorkspaceId};
use crate::ledger::open_connection;
use crate::random::RandomSource;
use crate::workspace::{Caller, Role};

/// How often, at most, a key's last use is written down.
const LAST_USE_EVERY_SECS: u64 = 60;

/// A workspace, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    /// The id the server minted: the directory's name, and the rows' key.
    pub id: WorkspaceId,
    /// The name the operator gave.
    pub name: String,
    /// The quota; `None` is the configured default.
    pub quota_bytes: Option<u64>,
    /// When it was created.
    pub created_at: u64,
}

/// What a key is good for right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// It opens its workspace.
    Active,
    /// Its time is up; `key extend` brings it back.
    Expired,
    /// It was revoked; nothing brings it back.
    Revoked,
}

/// A key, as an operator sees it: never its secret, nor its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    /// The public id.
    pub id: ApiKeyId,
    /// Its workspace.
    pub workspace: WorkspaceId,
    /// Its workspace's name.
    pub workspace_name: String,
    /// What the operator called it: a device, usually.
    pub label: String,
    /// What it may do.
    pub role: Role,
    /// When it was created.
    pub created_at: u64,
    /// When it stops working; `None` is never.
    pub expires_at: Option<u64>,
    /// When it was revoked.
    pub revoked_at: Option<u64>,
    /// When it last opened its workspace, to the minute.
    pub last_used_at: Option<u64>,
}

impl KeyInfo {
    /// The key's state at `now`. Revoked is said before expired.
    pub fn state(&self, now: u64) -> KeyState {
        if self.revoked_at.is_some() {
            KeyState::Revoked
        } else if self.expires_at.is_some_and(|at| at <= now) {
            KeyState::Expired
        } else {
            KeyState::Active
        }
    }
}

/// Who a request is from: what [`Control::authenticate`] answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    /// The workspace the key opens. No request names one; this is the only
    /// source.
    pub workspace: WorkspaceId,
    /// The key's id and role.
    pub caller: Caller,
    /// When the key expires, for `getViewer` and the client's warning.
    pub expires_at: Option<u64>,
    /// What the operator called the key.
    pub label: String,
}

/// One line of the audit trail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// When.
    pub at: u64,
    /// What: `workspace.create`, `key.revoke`, and so on.
    pub action: String,
    /// The workspace concerned.
    pub workspace: Option<String>,
    /// The key concerned.
    pub key_id: Option<String>,
    /// Anything else worth keeping; never a secret.
    pub detail: Option<String>,
}

/// Why an operator's command was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlError {
    /// Not lower-case letters, digits, and dashes, 1 to 32, letter first.
    InvalidName(String),
    /// Another workspace has the name.
    NameTaken(String),
    /// No workspace of that name.
    NoSuchWorkspace(String),
    /// No key of that id.
    NoSuchKey(String),
    /// The control database cannot be used; the log says why.
    Unavailable,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "`{name}` is not a workspace name: use lower-case letters, digits, and dashes, at most 32, starting with a letter"
            ),
            Self::NameTaken(name) => write!(f, "there is a workspace `{name}` already"),
            Self::NoSuchWorkspace(name) => write!(f, "there is no workspace `{name}`"),
            Self::NoSuchKey(id) => write!(f, "there is no key `{id}`"),
            Self::Unavailable => f.write_str("the control database cannot be used; see the log"),
        }
    }
}

impl std::error::Error for ControlError {}

fn sql(action: &'static str) -> impl Fn(rusqlite::Error) -> ControlError {
    move |err| {
        tracing::error!(target: "passalong_server::control", action, %err, "the control database cannot be used");
        ControlError::Unavailable
    }
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && name.len() <= 32
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn role_text(role: Role) -> &'static str {
    match role {
        Role::ReadWrite => "read-write",
        Role::ReadOnly => "read-only",
    }
}

fn to_db(number: u64) -> i64 {
    i64::try_from(number).unwrap_or(i64::MAX)
}

fn from_db(number: Option<i64>) -> Option<u64> {
    number.and_then(|number| u64::try_from(number).ok())
}

const KEY_COLUMNS: &str = "k.key_id, k.workspace, w.name, k.label, k.role, k.created_at, k.expires_at, k.revoked_at, k.last_used_at";

fn key_from(row: &Row<'_>) -> rusqlite::Result<KeyInfo> {
    let workspace: String = row.get(1)?;
    let role: String = row.get(4)?;
    Ok(KeyInfo {
        id: ApiKeyId::new(row.get::<_, String>(0)?),
        // Minted here and stored here; a row that does not parse is damage.
        workspace: WorkspaceId::parse(&workspace).map_err(|_| rusqlite::Error::InvalidQuery)?,
        workspace_name: row.get::<_, Option<String>>(2)?.unwrap_or(workspace),
        label: row.get(3)?,
        role: if role == "read-only" {
            Role::ReadOnly
        } else {
            Role::ReadWrite
        },
        created_at: from_db(row.get(5)?).unwrap_or(0),
        expires_at: from_db(row.get(6)?),
        revoked_at: from_db(row.get(7)?),
        last_used_at: from_db(row.get(8)?),
    })
}

fn workspace_from(row: &Row<'_>) -> rusqlite::Result<WorkspaceInfo> {
    let id: String = row.get(0)?;
    Ok(WorkspaceInfo {
        name: row
            .get::<_, Option<String>>(1)?
            .unwrap_or_else(|| id.clone()),
        id: WorkspaceId::parse(&id).map_err(|_| rusqlite::Error::InvalidQuery)?,
        quota_bytes: from_db(row.get(2)?),
        created_at: from_db(row.get(3)?).unwrap_or(0),
    })
}

/// The control database, as the operator's commands and the authentication
/// layer use it. It opens the same file as the
/// [`SqliteLedger`](crate::ledger::SqliteLedger), with a connection of its
/// own; SQLite keeps the two, and any other process's, apart.
pub struct Control {
    connection: Mutex<Connection>,
    clock: Arc<dyn Clock>,
    rng: Mutex<Box<dyn RandomSource>>,
}

impl Control {
    /// Opens the database at `path`, creating or migrating it as the ledger
    /// does.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unavailable`].
    pub fn open(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
        clock: Arc<dyn Clock>,
        rng: Box<dyn RandomSource>,
    ) -> Result<Self, ControlError> {
        let connection =
            open_connection(path.as_ref(), busy_timeout).map_err(|_| ControlError::Unavailable)?;
        Ok(Self {
            connection: Mutex::new(connection),
            clock,
            rng: Mutex::new(rng),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn note(
        tx: &Connection,
        at: u64,
        action: &str,
        workspace: Option<&str>,
        key: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), ControlError> {
        tx.execute(
            "INSERT INTO audit (at, action, workspace, key_id, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![to_db(at), action, workspace, key, detail],
        )
        .map(|_| ())
        .map_err(sql("write the audit trail"))
    }

    /// Whether the database answers, for `readyz`: it reads what every
    /// request's authentication reads.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unavailable`].
    pub fn ready(&self) -> Result<(), ControlError> {
        self.lock()
            .query_row("SELECT count(*) FROM api_keys", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|_| ())
            .map_err(sql("check readiness"))
    }

    /// `workspace create`.
    ///
    /// # Errors
    ///
    /// [`ControlError::InvalidName`], [`ControlError::NameTaken`].
    pub fn create_workspace(
        &self,
        name: &str,
        quota_bytes: Option<u64>,
    ) -> Result<WorkspaceInfo, ControlError> {
        if !valid_name(name) {
            return Err(ControlError::InvalidName(name.to_owned()));
        }
        let now = self.clock.now();
        let mut connection = self.lock();
        let tx = connection.transaction().map_err(sql("begin"))?;
        let taken: bool = tx
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM workspaces WHERE name = ?1)",
                [name],
                |row| row.get(0),
            )
            .map_err(sql("look for the name"))?;
        if taken {
            return Err(ControlError::NameTaken(name.to_owned()));
        }
        let id = WorkspaceId::generate(
            self.rng
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut(),
        );
        tx.execute(
            "INSERT INTO workspaces (id, generation, next_generation, name, quota_bytes, created_at) VALUES (?1, 0, 1, ?2, ?3, ?4)",
            params![id.as_str(), name, quota_bytes.map(to_db), to_db(now)],
        )
        .map_err(sql("create a workspace"))?;
        Self::note(
            &tx,
            now,
            "workspace.create",
            Some(id.as_str()),
            None,
            Some(name),
        )?;
        tx.commit().map_err(sql("commit"))?;
        tracing::info!(target: "passalong_server::control", workspace = %id.as_str(), "workspace created");
        Ok(WorkspaceInfo {
            id,
            name: name.to_owned(),
            quota_bytes,
            created_at: now,
        })
    }

    /// `workspace list`: by name.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unavailable`].
    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>, ControlError> {
        let connection = self.lock();
        let mut statement = connection
            .prepare("SELECT id, name, quota_bytes, created_at FROM workspaces ORDER BY name")
            .map_err(sql("list workspaces"))?;
        let rows = statement
            .query_map([], workspace_from)
            .map_err(sql("list workspaces"))?;
        rows.collect::<Result<_, _>>()
            .map_err(sql("list workspaces"))
    }

    /// One workspace, by name.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchWorkspace`].
    pub fn workspace(&self, name: &str) -> Result<WorkspaceInfo, ControlError> {
        self.lock()
            .query_row(
                "SELECT id, name, quota_bytes, created_at FROM workspaces WHERE name = ?1",
                [name],
                workspace_from,
            )
            .optional()
            .map_err(sql("find a workspace"))?
            .ok_or_else(|| ControlError::NoSuchWorkspace(name.to_owned()))
    }

    /// One workspace, by id: what an engine is opened from.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unavailable`].
    pub fn workspace_by_id(&self, id: &WorkspaceId) -> Result<Option<WorkspaceInfo>, ControlError> {
        self.lock()
            .query_row(
                "SELECT id, name, quota_bytes, created_at FROM workspaces WHERE id = ?1",
                [id.as_str()],
                workspace_from,
            )
            .optional()
            .map_err(sql("find a workspace"))
    }

    /// `workspace delete`: the rows, keys and record included, in one
    /// transaction. The directory is the caller's to remove afterwards.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchWorkspace`].
    pub fn delete_workspace(&self, name: &str) -> Result<WorkspaceInfo, ControlError> {
        let found = self.workspace(name)?;
        let now = self.clock.now();
        let mut connection = self.lock();
        let tx = connection.transaction().map_err(sql("begin"))?;
        tx.execute("DELETE FROM workspaces WHERE id = ?1", [found.id.as_str()])
            .map_err(sql("delete a workspace"))?;
        Self::note(
            &tx,
            now,
            "workspace.delete",
            Some(found.id.as_str()),
            None,
            Some(name),
        )?;
        tx.commit().map_err(sql("commit"))?;
        tracing::info!(target: "passalong_server::control", workspace = %found.id.as_str(), "workspace deleted");
        Ok(found)
    }

    /// `key create`. The key it returns is the only copy of the secret there
    /// will ever be. `expires_in` is seconds from now; `None` is never.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchWorkspace`].
    pub fn create_key(
        &self,
        workspace: &str,
        label: &str,
        role: Role,
        expires_in: Option<u64>,
    ) -> Result<(ApiKey, KeyInfo), ControlError> {
        let found = self.workspace(workspace)?;
        let now = self.clock.now();
        let expires_at = expires_in.map(|secs| now.saturating_add(secs));
        let mut connection = self.lock();
        let tx = connection.transaction().map_err(sql("begin"))?;
        // Never an id that names a key already: it is what commands and the
        // audit trail go by.
        let key = loop {
            let candidate = ApiKey::generate(
                self.rng
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut(),
            );
            let known: bool = tx
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM api_keys WHERE key_id = ?1)",
                    [candidate.id().as_str()],
                    |row| row.get(0),
                )
                .map_err(sql("look for the key id"))?;
            if !known {
                break candidate;
            }
        };
        tx.execute(
            "INSERT INTO api_keys (key_id, workspace, secret_hash, label, role, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![key.id().as_str(), found.id.as_str(), key.hash().to_bytes().as_slice(), label, role_text(role), to_db(now), expires_at.map(to_db)],
        )
        .map_err(sql("create a key"))?;
        Self::note(
            &tx,
            now,
            "key.create",
            Some(found.id.as_str()),
            Some(key.id().as_str()),
            Some(label),
        )?;
        tx.commit().map_err(sql("commit"))?;
        tracing::info!(target: "passalong_server::control", workspace = %found.id.as_str(), key = %key.id().as_str(), "key created");
        let info = KeyInfo {
            id: key.id().clone(),
            workspace: found.id,
            workspace_name: found.name,
            label: label.to_owned(),
            role,
            created_at: now,
            expires_at,
            revoked_at: None,
            last_used_at: None,
        };
        Ok((key, info))
    }

    /// `key list`: by workspace name, then creation.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchWorkspace`].
    pub fn keys(&self, workspace: Option<&str>) -> Result<Vec<KeyInfo>, ControlError> {
        let only = workspace.map(|name| self.workspace(name)).transpose()?;
        let connection = self.lock();
        let mut statement = connection
            .prepare(&format!(
                "SELECT {KEY_COLUMNS} FROM api_keys k JOIN workspaces w ON w.id = k.workspace
                 WHERE ?1 IS NULL OR k.workspace = ?1 ORDER BY w.name, k.created_at, k.rowid"
            ))
            .map_err(sql("list keys"))?;
        let rows = statement
            .query_map([only.as_ref().map(|found| found.id.as_str())], key_from)
            .map_err(sql("list keys"))?;
        rows.collect::<Result<_, _>>().map_err(sql("list keys"))
    }

    fn key(connection: &Connection, id: &str) -> Result<KeyInfo, ControlError> {
        connection
            .query_row(
                &format!("SELECT {KEY_COLUMNS} FROM api_keys k JOIN workspaces w ON w.id = k.workspace WHERE k.key_id = ?1"),
                [id],
                key_from,
            )
            .optional()
            .map_err(sql("find a key"))?
            .ok_or_else(|| ControlError::NoSuchKey(id.to_owned()))
    }

    /// Changes one key's row and notes it, in one transaction.
    fn change_key(
        &self,
        id: &str,
        action: &str,
        statement: &str,
        value: Option<i64>,
    ) -> Result<KeyInfo, ControlError> {
        let now = self.clock.now();
        let mut connection = self.lock();
        let tx = connection.transaction().map_err(sql("begin"))?;
        let before = Self::key(&tx, id)?;
        tx.execute(statement, params![id, value])
            .map_err(sql("change a key"))?;
        Self::note(
            &tx,
            now,
            action,
            Some(before.workspace.as_str()),
            Some(id),
            None,
        )?;
        let after = Self::key(&tx, id).or(Ok::<_, ControlError>(before))?;
        tx.commit().map_err(sql("commit"))?;
        tracing::info!(target: "passalong_server::control", key = %id, action, "key changed");
        Ok(after)
    }

    /// `key extend`: a new expiry, counted from now, for the same secret.
    /// `None` is never. It does not bring a revoked key back.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchKey`].
    pub fn extend_key(&self, id: &str, expires_in: Option<u64>) -> Result<KeyInfo, ControlError> {
        let at = expires_in.map(|secs| to_db(self.clock.now().saturating_add(secs)));
        self.change_key(
            id,
            "key.extend",
            "UPDATE api_keys SET expires_at = ?2 WHERE key_id = ?1",
            at,
        )
    }

    /// `key revoke`: refused from the next request on, by every process.
    /// The row stays, for the audit trail; revoking again changes nothing.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchKey`].
    pub fn revoke_key(&self, id: &str) -> Result<KeyInfo, ControlError> {
        let now = to_db(self.clock.now());
        self.change_key(
            id,
            "key.revoke",
            "UPDATE api_keys SET revoked_at = coalesce(revoked_at, ?2) WHERE key_id = ?1",
            Some(now),
        )
    }

    /// `key delete`: the row goes; the audit trail remembers.
    ///
    /// # Errors
    ///
    /// [`ControlError::NoSuchKey`].
    pub fn delete_key(&self, id: &str) -> Result<KeyInfo, ControlError> {
        self.change_key(
            id,
            "key.delete",
            "DELETE FROM api_keys WHERE key_id = ?1 AND ?2 IS NULL",
            None,
        )
    }

    /// `key prune`: deletes keys that expired or were revoked at least
    /// `older_than` seconds ago, and says how many.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unavailable`].
    pub fn prune_keys(&self, older_than: u64) -> Result<usize, ControlError> {
        let now = self.clock.now();
        let before = to_db(now.saturating_sub(older_than));
        let mut connection = self.lock();
        let tx = connection.transaction().map_err(sql("begin"))?;
        let pruned = tx
            .execute(
                "DELETE FROM api_keys WHERE coalesce(revoked_at, expires_at) <= ?1",
                [before],
            )
            .map_err(sql("prune keys"))?;
        if pruned > 0 {
            Self::note(&tx, now, "key.prune", None, None, Some(&pruned.to_string()))?;
        }
        tx.commit().map_err(sql("commit"))?;
        Ok(pruned)
    }

    /// The audit trail, newest first.
    ///
    /// # Errors
    ///
    /// [`ControlError::Unavailable`].
    pub fn audit(&self, limit: usize) -> Result<Vec<AuditEntry>, ControlError> {
        let connection = self.lock();
        let mut statement = connection
            .prepare(
                "SELECT at, action, workspace, key_id, detail FROM audit ORDER BY id DESC LIMIT ?1",
            )
            .map_err(sql("read the audit trail"))?;
        let rows = statement
            .query_map([to_db(limit as u64)], |row| {
                Ok(AuditEntry {
                    at: from_db(row.get(0)?).unwrap_or(0),
                    action: row.get(1)?,
                    workspace: row.get(2)?,
                    key_id: row.get(3)?,
                    detail: row.get(4)?,
                })
            })
            .map_err(sql("read the audit trail"))?;
        rows.collect::<Result<_, _>>()
            .map_err(sql("read the audit trail"))
    }

    /// Who a bearer token is from. Read from the database every time, so
    /// what the CLI changed in another process holds from the next request.
    ///
    /// # Errors
    ///
    /// [`ApiError::Unauthenticated`] for a token that is malformed, names no
    /// key, or has the wrong secret, which are not told apart, and an unknown
    /// key id costs the same comparison as a known one. Then, only for the
    /// right secret: [`ApiError::KeyRevoked`], [`ApiError::KeyExpired`].
    /// [`ApiError::ServiceUnavailable`] when the database cannot answer: the
    /// server does not know, so it refuses.
    pub fn authenticate(&self, token: &str) -> Result<Authenticated, ApiError> {
        let presented = ApiKey::parse(token).ok_or(ApiError::Unauthenticated)?;
        let now = self.clock.now();
        let connection = self.lock();
        let unavailable = |err: rusqlite::Error| {
            tracing::error!(target: "passalong_server::control", %err, "cannot authenticate: the control database cannot be read");
            ApiError::ServiceUnavailable
        };
        type Found = (
            Vec<u8>,
            String,
            String,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            String,
        );
        let row: Option<Found> = connection
            .query_row(
                "SELECT secret_hash, workspace, role, expires_at, revoked_at, last_used_at, label FROM api_keys WHERE key_id = ?1",
                [presented.id().as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(unavailable)?;
        let stored = row
            .as_ref()
            .and_then(|found| SecretHash::from_bytes(&found.0))
            .unwrap_or_else(SecretHash::of_nothing);
        let right = stored.matches(&presented.hash());
        let Some((_, workspace, role, expires_at, revoked_at, last_used_at, label)) =
            row.filter(|_| right)
        else {
            return Err(ApiError::Unauthenticated);
        };
        if revoked_at.is_some() {
            return Err(ApiError::KeyRevoked);
        }
        if from_db(expires_at).is_some_and(|at| at <= now) {
            return Err(ApiError::KeyExpired);
        }
        let stale =
            from_db(last_used_at).is_none_or(|at| now.saturating_sub(at) >= LAST_USE_EVERY_SECS);
        if stale {
            // A note, not a gate: a writer elsewhere must not lock devices out.
            let noted = connection.execute(
                "UPDATE api_keys SET last_used_at = ?2 WHERE key_id = ?1",
                params![presented.id().as_str(), to_db(now)],
            );
            if let Err(err) = noted {
                tracing::debug!(target: "passalong_server::control", key = %presented.id().as_str(), %err, "last use not noted");
            }
        }
        Ok(Authenticated {
            workspace: WorkspaceId::parse(&workspace).map_err(|_| ApiError::ServiceUnavailable)?,
            caller: Caller::new(
                presented.id().clone(),
                if role == "read-only" {
                    Role::ReadOnly
                } else {
                    Role::ReadWrite
                },
            ),
            expires_at: from_db(expires_at),
            label,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::error::ApiError;
    use crate::random::SeededRandom;
    use crate::workspace::Role;
    use std::sync::Arc;
    use std::time::Duration;

    const DAY: u64 = 24 * 60 * 60;

    fn control_at(path: &std::path::Path, clock: &ManualClock, seed: u64) -> Control {
        Control::open(
            path,
            Duration::from_millis(500),
            Arc::new(clock.clone()),
            Box::new(SeededRandom::new(seed)),
        )
        .unwrap()
    }

    fn control() -> (Control, ManualClock, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let clock = ManualClock::at(1_000_000);
        let control = control_at(&dir.path().join("control.sqlite"), &clock, 1);
        (control, clock, dir)
    }

    #[test]
    fn a_workspace_has_a_name_a_quota_and_an_id_of_the_servers_making() {
        let (control, _, _dir) = control();
        let home = control.create_workspace("home", Some(1_000)).unwrap();
        assert_eq!(
            (home.name.as_str(), home.quota_bytes, home.created_at),
            ("home", Some(1_000), 1_000_000)
        );
        assert_eq!(home.id.as_str().len(), 16);
        let work = control.create_workspace("work-2", None).unwrap();
        assert_ne!(work.id, home.id);
        assert_eq!(work.quota_bytes, None);

        assert_eq!(
            control.create_workspace("home", None).unwrap_err(),
            ControlError::NameTaken("home".into())
        );
        for bad in [
            "",
            "Home",
            "1st",
            "-x",
            "has space",
            "a/b",
            "../x",
            &"a".repeat(33),
        ] {
            assert!(
                matches!(
                    control.create_workspace(bad, None).unwrap_err(),
                    ControlError::InvalidName(_)
                ),
                "{bad:?}"
            );
        }
        let names: Vec<String> = control
            .workspaces()
            .unwrap()
            .into_iter()
            .map(|w| w.name)
            .collect();
        assert_eq!(names, ["home", "work-2"]);
        assert_eq!(control.workspace("home").unwrap(), home);
        assert_eq!(
            control.workspace("nope").unwrap_err(),
            ControlError::NoSuchWorkspace("nope".into())
        );
    }

    #[test]
    fn deleting_a_workspace_takes_its_keys_and_its_record_along() {
        let (control, _, _dir) = control();
        let home = control.create_workspace("home", None).unwrap();
        control.create_workspace("other", None).unwrap();
        let (key, _) = control
            .create_key("home", "laptop", Role::ReadWrite, Some(DAY))
            .unwrap();
        let (kept, _) = control
            .create_key("other", "phone", Role::ReadWrite, None)
            .unwrap();
        assert_eq!(control.delete_workspace("home").unwrap(), home);
        assert_eq!(
            control.delete_workspace("home").unwrap_err(),
            ControlError::NoSuchWorkspace("home".into())
        );
        assert_eq!(
            control.authenticate(&key.reveal()).unwrap_err(),
            ApiError::Unauthenticated
        );
        assert!(control.authenticate(&kept.reveal()).is_ok());
        assert_eq!(control.keys(None).unwrap().len(), 1);
    }

    #[test]
    fn a_key_opens_its_workspace_and_no_other() {
        let (control, _, _dir) = control();
        let home = control.create_workspace("home", None).unwrap();
        let (key, info) = control
            .create_key("home", "laptop", Role::ReadOnly, Some(90 * DAY))
            .unwrap();
        assert_eq!(
            (info.label.as_str(), info.role, info.expires_at),
            ("laptop", Role::ReadOnly, Some(1_000_000 + 90 * DAY))
        );
        assert_eq!(info.workspace_name, "home");
        assert_eq!(info.state(1_000_000), KeyState::Active);

        let who = control.authenticate(&key.reveal()).unwrap();
        assert_eq!(who.workspace, home.id);
        assert_eq!(who.caller.key, *key.id());
        assert_eq!(who.caller.role, Role::ReadOnly);
        assert_eq!(who.expires_at, Some(1_000_000 + 90 * DAY));

        assert_eq!(
            control
                .create_key("nope", "x", Role::ReadWrite, None)
                .unwrap_err(),
            ControlError::NoSuchWorkspace("nope".into())
        );
    }

    #[test]
    fn whatever_is_wrong_with_a_token_the_answer_is_the_same() {
        let (control, _, _dir) = control();
        control.create_workspace("home", None).unwrap();
        let (key, _) = control
            .create_key("home", "laptop", Role::ReadWrite, None)
            .unwrap();
        let text = key.reveal();
        let wrong_secret = format!(
            "{}{}",
            &text[..text.len() - 1],
            if text.ends_with('0') { '1' } else { '0' }
        );
        let unknown_id = format!("pal_{}_{}", "0".repeat(12), &text[17..]);
        for token in [
            "",
            "Bearer x",
            "pal_zz",
            &wrong_secret,
            &unknown_id,
            &text.to_uppercase(),
        ] {
            assert_eq!(
                control.authenticate(token).unwrap_err(),
                ApiError::Unauthenticated,
                "{token:?}"
            );
        }
    }

    #[test]
    fn a_key_stops_at_the_second_it_expires_and_can_be_given_more_time() {
        let (control, clock, _dir) = control();
        control.create_workspace("home", None).unwrap();
        let (key, info) = control
            .create_key("home", "laptop", Role::ReadWrite, Some(DAY))
            .unwrap();
        clock.advance(DAY - 1);
        assert!(control.authenticate(&key.reveal()).is_ok());
        clock.advance(1);
        assert_eq!(
            control.authenticate(&key.reveal()).unwrap_err(),
            ApiError::KeyExpired
        );
        assert_eq!(
            control.keys(None).unwrap()[0].state(clock_now(&clock)),
            KeyState::Expired
        );

        // The same secret, a later date.
        let extended = control
            .extend_key(info.id.as_str(), Some(30 * DAY))
            .unwrap();
        assert_eq!(extended.expires_at, Some(clock_now(&clock) + 30 * DAY));
        assert!(control.authenticate(&key.reveal()).is_ok());
        assert_eq!(
            control
                .extend_key(info.id.as_str(), None)
                .unwrap()
                .expires_at,
            None
        );
        assert_eq!(
            control.extend_key("000000000000", None).unwrap_err(),
            ControlError::NoSuchKey("000000000000".into())
        );
    }

    fn clock_now(clock: &ManualClock) -> u64 {
        crate::clock::Clock::now(clock)
    }

    #[test]
    fn a_revocation_made_elsewhere_holds_from_the_very_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sqlite");
        let clock = ManualClock::at(1_000_000);
        let server = control_at(&path, &clock, 1);
        let cli = control_at(&path, &clock, 2);
        cli.create_workspace("home", None).unwrap();
        let (key, info) = cli
            .create_key("home", "laptop", Role::ReadWrite, None)
            .unwrap();
        assert!(server.authenticate(&key.reveal()).is_ok());

        let revoked = cli.revoke_key(info.id.as_str()).unwrap();
        assert_eq!(revoked.revoked_at, Some(1_000_000));
        assert_eq!(
            server.authenticate(&key.reveal()).unwrap_err(),
            ApiError::KeyRevoked
        );
        // Revoked is said before expired, and revoking twice keeps the date.
        clock.advance(5);
        assert_eq!(
            cli.revoke_key(info.id.as_str()).unwrap().revoked_at,
            Some(1_000_000)
        );
        assert_eq!(
            cli.keys(Some("home")).unwrap()[0].state(clock_now(&clock)),
            KeyState::Revoked
        );
        // Extending does not bring a revoked key back.
        cli.extend_key(info.id.as_str(), None).unwrap();
        assert_eq!(
            server.authenticate(&key.reveal()).unwrap_err(),
            ApiError::KeyRevoked
        );
    }

    #[test]
    fn last_use_is_noted_at_most_once_a_minute() {
        let (control, clock, _dir) = control();
        control.create_workspace("home", None).unwrap();
        let (key, _) = control
            .create_key("home", "laptop", Role::ReadWrite, None)
            .unwrap();
        assert_eq!(control.keys(None).unwrap()[0].last_used_at, None);
        control.authenticate(&key.reveal()).unwrap();
        assert_eq!(control.keys(None).unwrap()[0].last_used_at, Some(1_000_000));
        clock.advance(59);
        control.authenticate(&key.reveal()).unwrap();
        assert_eq!(control.keys(None).unwrap()[0].last_used_at, Some(1_000_000));
        clock.advance(1);
        control.authenticate(&key.reveal()).unwrap();
        assert_eq!(control.keys(None).unwrap()[0].last_used_at, Some(1_000_060));
    }

    #[test]
    fn old_dead_keys_are_pruned_and_live_ones_are_not() {
        let (control, clock, _dir) = control();
        control.create_workspace("home", None).unwrap();
        let (_, short) = control
            .create_key("home", "short", Role::ReadWrite, Some(DAY))
            .unwrap();
        let (_, gone) = control
            .create_key("home", "gone", Role::ReadWrite, None)
            .unwrap();
        let (_, live) = control
            .create_key("home", "live", Role::ReadWrite, None)
            .unwrap();
        control.revoke_key(gone.id.as_str()).unwrap();
        clock.advance(10 * DAY);
        assert_eq!(control.prune_keys(30 * DAY).unwrap(), 0);
        clock.advance(25 * DAY);
        assert_eq!(control.prune_keys(30 * DAY).unwrap(), 2);
        let left: Vec<String> = control
            .keys(None)
            .unwrap()
            .into_iter()
            .map(|k| k.label)
            .collect();
        assert_eq!(left, ["live"]);
        control.delete_key(live.id.as_str()).unwrap();
        assert_eq!(
            control.delete_key(short.id.as_str()).unwrap_err(),
            ControlError::NoSuchKey(short.id.as_str().into())
        );
    }

    #[test]
    fn every_change_is_in_the_audit_trail() {
        let (control, _, _dir) = control();
        control.create_workspace("home", None).unwrap();
        let (_, info) = control
            .create_key("home", "laptop", Role::ReadWrite, Some(DAY))
            .unwrap();
        control.extend_key(info.id.as_str(), None).unwrap();
        control.revoke_key(info.id.as_str()).unwrap();
        control.delete_key(info.id.as_str()).unwrap();
        control.delete_workspace("home").unwrap();
        let actions: Vec<String> = control
            .audit(10)
            .unwrap()
            .into_iter()
            .rev()
            .map(|entry| entry.action)
            .collect();
        assert_eq!(
            actions,
            [
                "workspace.create",
                "key.create",
                "key.extend",
                "key.revoke",
                "key.delete",
                "workspace.delete"
            ]
        );
        let entry = &control.audit(10).unwrap()[3];
        assert_eq!(
            (entry.at, entry.key_id.as_deref()),
            (1_000_000, Some(info.id.as_str()))
        );
    }

    #[test]
    fn nothing_of_a_secret_is_in_the_database() {
        let (control, _, dir) = control();
        control.create_workspace("home", None).unwrap();
        let (key, _) = control
            .create_key("home", "laptop", Role::ReadWrite, None)
            .unwrap();
        control.authenticate(&key.reveal()).unwrap();
        drop(control);
        let secret = key.reveal()[17..].to_owned();
        let secret_bytes: Vec<u8> = (0..32)
            .map(|i| u8::from_str_radix(&secret[2 * i..2 * i + 2], 16).unwrap())
            .collect();

        // Every file SQLite keeps, read raw: the database and its WAL.
        let mut found_hash = false;
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let bytes = std::fs::read(entry.unwrap().path()).unwrap();
            let contains =
                |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
            for start in 0..secret.len() - 8 {
                assert!(
                    !contains(&secret.as_bytes()[start..start + 8]),
                    "part of the secret is on disk as text"
                );
            }
            assert!(
                !contains(&secret_bytes[..8]),
                "part of the secret is on disk as bytes"
            );
            found_hash |= contains(&key.hash().to_bytes());
        }
        assert!(found_hash, "the hash is what is kept");
    }

    #[test]
    fn a_writer_elsewhere_does_not_lock_devices_out_but_a_broken_database_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.sqlite");
        let clock = ManualClock::at(1_000_000);
        let control = control_at(&path, &clock, 1);
        control.create_workspace("home", None).unwrap();
        let (key, _) = control
            .create_key("home", "laptop", Role::ReadWrite, None)
            .unwrap();

        // The CLI in the middle of a transaction: readers do not wait for a
        // writer, so requests are still authenticated. Only the note of the
        // key's last use, which needs to write, is skipped.
        let other = rusqlite::Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(control.authenticate(&key.reveal()).is_ok());
        other.execute_batch("ROLLBACK").unwrap();
        assert_eq!(control.keys(None).unwrap()[0].last_used_at, None);

        // A database that cannot answer: not UNAUTHENTICATED, and not yes.
        // The server does not know, so it refuses.
        other.execute_batch("DROP TABLE api_keys").unwrap();
        assert_eq!(
            control.authenticate(&key.reveal()).unwrap_err(),
            ApiError::ServiceUnavailable
        );
    }
}
