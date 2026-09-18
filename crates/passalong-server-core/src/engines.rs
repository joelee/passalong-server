//! The engines of a running server: one per workspace, opened when first
//! asked for and shared from then on.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::clock::Clock;
use crate::config::Config;
use crate::control::Control;
use crate::error::ApiError;
use crate::ids::WorkspaceId;
use crate::ledger::SqliteLedger;
use crate::random::OsRandom;
use crate::shelf::FsShelf;
use crate::workspace::Engine;

/// A workspace's engine over the server's real stores.
pub type ServerEngine = Engine<FsShelf, SqliteLedger>;

/// Workspace id to engine. Every operation of an engine takes `&self`, the
/// ledger's transaction being the lock, so an engine is shared as it is and
/// no request waits for another's content to arrive.
pub struct Engines {
    config: Config,
    busy: Duration,
    clock: Arc<dyn Clock>,
    open: Mutex<BTreeMap<WorkspaceId, Arc<ServerEngine>>>,
}

impl Engines {
    /// No engine is opened until it is asked for. Every engine tells the
    /// time by `clock`: leases, upload tickets, and `receivedAt`.
    pub fn new(config: Config, busy: Duration, clock: Arc<dyn Clock>) -> Self {
        Self {
            config,
            busy,
            clock,
            open: Mutex::new(BTreeMap::new()),
        }
    }

    /// The configuration the engines are opened with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The engine of `workspace`, opened, and so repaired, on first use.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] for a workspace the control database does not
    /// have, before anything is created for it;
    /// [`ApiError::ServiceUnavailable`].
    pub fn get(&self, workspace: &WorkspaceId) -> Result<Arc<ServerEngine>, ApiError> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(engine) = open.get(workspace) {
            return Ok(engine.clone());
        }
        let control = Control::open(
            self.config.control_database(),
            self.busy,
            self.clock.clone(),
            Box::new(OsRandom),
        )
        .map_err(|_| ApiError::ServiceUnavailable)?;
        let info = control
            .workspace_by_id(workspace)
            .map_err(|_| ApiError::ServiceUnavailable)?
            .ok_or(ApiError::NotFound)?;
        let engine = Arc::new(Engine::open(
            FsShelf::open(self.config.workspace_dir(workspace))?,
            SqliteLedger::open(self.config.control_database(), self.busy)?,
            workspace.clone(),
            self.clock.clone(),
            Box::new(OsRandom),
            self.config.limits_for(info.quota_bytes),
        )?);
        open.insert(workspace.clone(), engine.clone());
        Ok(engine)
    }

    /// The workspaces whose engines are open.
    pub fn open_workspaces(&self) -> BTreeSet<WorkspaceId> {
        let open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        open.keys().cloned().collect()
    }

    /// Forgets a workspace's engine, as after the workspace was deleted.
    pub fn forget(&self, workspace: &WorkspaceId) {
        self.open
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(workspace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::control::Control;
    use crate::random::OsRandom;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn a_workspaces_engine_is_opened_once_and_shared() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::parse(
            &crate::config::initial_file(dir.path(), dir.path()),
            std::path::Path::new("/c.toml"),
            &crate::config::Process,
        )
        .unwrap();
        let control = Control::open(
            config.control_database(),
            Duration::from_secs(1),
            Arc::new(ManualClock::at(1_800_000_000)),
            Box::new(OsRandom),
        )
        .unwrap();
        let home = control.create_workspace("home", Some(1_000)).unwrap();
        let other = control.create_workspace("other", None).unwrap();

        let clock = ManualClock::at(1_800_000_000);
        let engines = Engines::new(
            config.clone(),
            Duration::from_secs(1),
            Arc::new(clock.clone()),
        );
        let first = engines.get(&home.id).unwrap();
        let again = engines.get(&home.id).unwrap();
        assert!(Arc::ptr_eq(&first, &again));
        assert!(!Arc::ptr_eq(&first, &engines.get(&other.id).unwrap()));
        assert_eq!(
            engines.open_workspaces(),
            vec![home.id.clone(), other.id.clone()]
                .into_iter()
                .collect()
        );

        // The workspace's own quota, or the configured default.
        assert_eq!(first.limits().quota_bytes, 1_000);
        assert_eq!(
            engines.get(&other.id).unwrap().limits().quota_bytes,
            config.limits.workspace_quota_bytes
        );
        assert!(
            first.limits().check_plaintext_content,
            "the server checks plaintext content"
        );

        // The engines tell the time by the clock they were given: a lease
        // begins when that clock says, whatever the machine's says.
        let caller = crate::workspace::Caller {
            key: crate::ids::ApiKeyId::new("0123456789ab"),
            role: crate::workspace::Role::ReadWrite,
        };
        clock.advance(100);
        let session = first
            .begin_rewrite(
                &caller,
                crate::rewrite::RewriteRequest {
                    kind: crate::rewrite::RewriteKind::Migrate,
                    expected_key_id: None,
                    new_key_id: crate::ids::KeyId::parse("aa").unwrap(),
                    new_header: b"{}".to_vec(),
                },
            )
            .unwrap();
        assert_eq!(
            session.lease_expires_at,
            1_800_000_100 + config.rewrite.lease_secs
        );

        // What the database does not have cannot be opened, and leaves no
        // directory behind.
        let unknown = crate::ids::WorkspaceId::parse("00000000000000ff").unwrap();
        assert_eq!(
            engines.get(&unknown).err().unwrap(),
            crate::error::ApiError::NotFound
        );
        assert!(!config.workspace_dir(&unknown).exists());
    }
}
