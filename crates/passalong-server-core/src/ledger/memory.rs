//! A ledger in memory, for unit tests and as the reference the SQLite
//! ledger is compared with.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use super::{Ledger, Rule, WorkspaceId, WorkspaceRecord};
use crate::error::ApiError;

/// A [`Ledger`] that forgets everything when dropped.
#[derive(Debug, Default)]
pub struct MemoryLedger(Mutex<BTreeMap<WorkspaceId, WorkspaceRecord>>);

impl Ledger for MemoryLedger {
    fn create(&self, workspace: &WorkspaceId) -> Result<(), ApiError> {
        let mut records = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        records.entry(workspace.clone()).or_default();
        Ok(())
    }

    fn load(&self, workspace: &WorkspaceId) -> Result<WorkspaceRecord, ApiError> {
        let records = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        records.get(workspace).cloned().ok_or(ApiError::NotFound)
    }

    fn transact<T>(&self, workspace: &WorkspaceId, rule: Rule<'_, T>) -> Result<T, ApiError> {
        // Holding the mutex for the whole rule is what makes it exclusive.
        let mut records = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let mut record = records.get(workspace).cloned().ok_or(ApiError::NotFound)?;
        let answer = rule(&mut record)?;
        records.insert(workspace.clone(), record);
        Ok(answer)
    }
}
