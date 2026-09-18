//! A shelf in memory, for unit tests and as the reference the filesystem
//! shelf is compared with. By its nature it holds items whole; what it
//! shares with a real shelf is that it never *reads* one whole.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::{Content, Envelope, ItemShelf, StoredItem, pump};
use crate::error::ApiError;
use crate::ids::{ItemId, UploadId};

/// An [`ItemShelf`] that forgets everything when dropped.
#[derive(Debug, Default)]
pub struct MemoryShelf(Mutex<Shelves>);

#[derive(Debug, Default)]
struct Shelves {
    staging: BTreeMap<UploadId, (Vec<u8>, Option<[u8; 32]>)>,
    generations: BTreeMap<u64, BTreeMap<ItemId, (Vec<u8>, Envelope)>>,
}

impl MemoryShelf {
    fn write(
        &self,
        upload: &UploadId,
        content: &mut dyn Read,
        announced: u64,
        hashed: bool,
    ) -> Result<u64, ApiError> {
        if !self.lock().staging.contains_key(upload) {
            return Err(ApiError::NotFound);
        }
        // The lock is not held while the content arrives: that may take long.
        let mut bytes = Vec::new();
        let mut hasher = hashed.then(sha2::Sha256::default);
        let written = pump(content, announced, hasher.as_mut(), |piece| {
            bytes.extend_from_slice(piece);
            Ok(())
        });
        let digest = hasher.map(|hasher| sha2::Digest::finalize(hasher).into());
        let mut shelves = self.lock();
        let staged = shelves.staging.get_mut(upload).ok_or(ApiError::NotFound)?;
        *staged = if written.is_ok() {
            (bytes, digest)
        } else {
            (Vec::new(), None)
        };
        written
    }

    fn lock(&self) -> MutexGuard<'_, Shelves> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn stored((content, envelope): &(Vec<u8>, Envelope)) -> StoredItem {
    StoredItem {
        size: content.len() as u64,
        envelope: envelope.clone(),
    }
}

impl ItemShelf for MemoryShelf {
    fn stage_create(&self, upload: &UploadId) -> Result<(), ApiError> {
        self.lock().staging.entry(upload.clone()).or_default();
        Ok(())
    }

    fn stage_write(
        &self,
        upload: &UploadId,
        content: &mut dyn Read,
        announced: u64,
    ) -> Result<u64, ApiError> {
        self.write(upload, content, announced, false)
    }

    fn stage_write_hashed(
        &self,
        upload: &UploadId,
        content: &mut dyn Read,
        announced: u64,
    ) -> Result<u64, ApiError> {
        self.write(upload, content, announced, true)
    }

    fn stage_digest(&self, upload: &UploadId) -> Result<Option<[u8; 32]>, ApiError> {
        Ok(self
            .lock()
            .staging
            .get(upload)
            .and_then(|(_, digest)| *digest))
    }

    fn stage_size(&self, upload: &UploadId) -> Result<Option<u64>, ApiError> {
        Ok(self
            .lock()
            .staging
            .get(upload)
            .map(|(bytes, _)| bytes.len() as u64))
    }

    fn stage_remove(&self, upload: &UploadId) -> Result<(), ApiError> {
        self.lock().staging.remove(upload);
        Ok(())
    }

    fn staged(&self) -> Result<Vec<UploadId>, ApiError> {
        Ok(self.lock().staging.keys().cloned().collect())
    }

    fn publish(
        &self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError> {
        let mut shelves = self.lock();
        if !shelves.staging.contains_key(upload) {
            return Err(ApiError::NotFound);
        }
        if shelves
            .generations
            .get(&generation)
            .is_some_and(|items| items.contains_key(id))
        {
            return Ok(false);
        }
        let (content, _) = shelves.staging.remove(upload).unwrap_or_default();
        shelves
            .generations
            .entry(generation)
            .or_default()
            .insert(id.clone(), (content, envelope));
        Ok(true)
    }

    fn get(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        let shelves = self.lock();
        Ok(shelves
            .generations
            .get(&generation)
            .and_then(|items| items.get(id))
            .map(stored))
    }

    fn open_content_from(
        &self,
        generation: u64,
        id: &ItemId,
        offset: u64,
    ) -> Result<Option<Content>, ApiError> {
        let shelves = self.lock();
        let item = shelves
            .generations
            .get(&generation)
            .and_then(|items| items.get(id));
        Ok(item.map(|(content, _)| {
            let from = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(content.len());
            Box::new(Cursor::new(content[from..].to_vec())) as Content
        }))
    }

    fn ids(&self, generation: u64) -> Result<Vec<ItemId>, ApiError> {
        let shelves = self.lock();
        Ok(shelves
            .generations
            .get(&generation)
            .map(|items| items.keys().rev().cloned().collect())
            .unwrap_or_default())
    }

    fn remove(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        let mut shelves = self.lock();
        let removed = shelves
            .generations
            .get_mut(&generation)
            .and_then(|items| items.remove(id));
        Ok(removed.as_ref().map(stored))
    }

    fn drop_generation(&self, generation: u64) -> Result<(), ApiError> {
        self.lock().generations.remove(&generation);
        Ok(())
    }

    fn generations(&self) -> Result<Vec<u64>, ApiError> {
        let shelves = self.lock();
        Ok(shelves
            .generations
            .iter()
            .filter(|(_, items)| !items.is_empty())
            .map(|(generation, _)| *generation)
            .collect())
    }

    fn bytes(&self, generation: u64) -> Result<u64, ApiError> {
        let shelves = self.lock();
        Ok(shelves
            .generations
            .get(&generation)
            .map(|items| {
                items
                    .values()
                    .map(|(content, _)| content.len() as u64)
                    .sum()
            })
            .unwrap_or(0))
    }
}
