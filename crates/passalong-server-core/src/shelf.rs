//! Where items are kept. The rules of this crate need only the few
//! operations below, each of which a POSIX filesystem performs in one step,
//! so nothing proved over [`MemoryShelf`] rests on atomicity the real store
//! cannot give.

use std::collections::BTreeMap;

use crate::error::ApiError;
use crate::ids::{ItemId, KeyId, UploadId};

/// What the server records beside an item's content. `meta` is the
/// client's `meta.json`, byte for byte, and is never looked into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// The client's opaque metadata document.
    pub meta: Vec<u8>,
    /// The data key the item was uploaded under; `None` for plaintext.
    pub under: Option<KeyId>,
    /// When the server published the item, by its own clock.
    pub received_at: u64,
}

/// A published item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    /// The item's bytes: plaintext, or sealed by the client.
    pub content: Vec<u8>,
    /// See [`Envelope`].
    pub envelope: Envelope,
}

/// Storage for the items of one workspace, in numbered generations.
///
/// The filesystem implementation of v0.1.0 maps each method to one step:
/// `publish` is a rename of `staging/<upload>` to `gen-<n>/items/<id>` that
/// fails when the target exists; `drop_generation` removes a tree that
/// nothing points to any more, so it may be interrupted and repeated.
pub trait ItemShelf {
    /// Creates the staging place of an upload; harmless when it exists.
    fn stage_create(&mut self, upload: &UploadId);

    /// Replaces the staged content.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] when nothing is staged under `upload`.
    fn stage_write(&mut self, upload: &UploadId, content: Vec<u8>) -> Result<(), ApiError>;

    /// The staged content's size, or `None` when nothing is staged.
    fn stage_size(&self, upload: &UploadId) -> Option<u64>;

    /// Removes a staging place; harmless when there is none.
    fn stage_remove(&mut self, upload: &UploadId);

    /// Every staging place, for the janitor and for leak checks.
    fn staged(&self) -> Vec<UploadId>;

    /// Turns a staged upload into the item `id` of `generation`, in one
    /// step, unless that item exists: then it answers `false`, replaces
    /// nothing, and leaves the staging place for the caller to remove.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] when nothing is staged under `upload`.
    fn publish(
        &mut self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError>;

    /// One item.
    fn get(&self, generation: u64, id: &ItemId) -> Option<StoredItem>;

    /// A generation's ids, newest first.
    fn ids(&self, generation: u64) -> Vec<ItemId>;

    /// Removes and returns one item.
    fn remove(&mut self, generation: u64, id: &ItemId) -> Option<StoredItem>;

    /// Removes a whole generation; harmless when there is none.
    fn drop_generation(&mut self, generation: u64);

    /// The generations that hold anything.
    fn generations(&self) -> Vec<u64>;

    /// The content bytes a generation holds.
    fn bytes(&self, generation: u64) -> u64;
}

/// An [`ItemShelf`] in memory, for the model and for tests.
#[derive(Debug, Default, Clone)]
pub struct MemoryShelf {
    staging: BTreeMap<UploadId, Vec<u8>>,
    generations: BTreeMap<u64, BTreeMap<ItemId, StoredItem>>,
}

impl ItemShelf for MemoryShelf {
    fn stage_create(&mut self, upload: &UploadId) {
        self.staging.entry(upload.clone()).or_default();
    }

    fn stage_write(&mut self, upload: &UploadId, content: Vec<u8>) -> Result<(), ApiError> {
        let staged = self.staging.get_mut(upload).ok_or(ApiError::NotFound)?;
        *staged = content;
        Ok(())
    }

    fn stage_size(&self, upload: &UploadId) -> Option<u64> {
        self.staging.get(upload).map(|content| content.len() as u64)
    }

    fn stage_remove(&mut self, upload: &UploadId) {
        self.staging.remove(upload);
    }

    fn staged(&self) -> Vec<UploadId> {
        self.staging.keys().cloned().collect()
    }

    fn publish(
        &mut self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError> {
        if !self.staging.contains_key(upload) {
            return Err(ApiError::NotFound);
        }
        let items = self.generations.entry(generation).or_default();
        if items.contains_key(id) {
            return Ok(false);
        }
        let content = self.staging.remove(upload).unwrap_or_default();
        items.insert(id.clone(), StoredItem { content, envelope });
        Ok(true)
    }

    fn get(&self, generation: u64, id: &ItemId) -> Option<StoredItem> {
        self.generations.get(&generation)?.get(id).cloned()
    }

    fn ids(&self, generation: u64) -> Vec<ItemId> {
        self.generations
            .get(&generation)
            .map(|items| items.keys().rev().cloned().collect())
            .unwrap_or_default()
    }

    fn remove(&mut self, generation: u64, id: &ItemId) -> Option<StoredItem> {
        self.generations.get_mut(&generation)?.remove(id)
    }

    fn drop_generation(&mut self, generation: u64) {
        self.generations.remove(&generation);
    }

    fn generations(&self) -> Vec<u64> {
        self.generations
            .iter()
            .filter(|(_, items)| !items.is_empty())
            .map(|(generation, _)| *generation)
            .collect()
    }

    fn bytes(&self, generation: u64) -> u64 {
        self.generations
            .get(&generation)
            .map(|items| items.values().map(|item| item.content.len() as u64).sum())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::SeededRandom;

    fn id(text: &str) -> ItemId {
        ItemId::parse(text).unwrap()
    }

    fn envelope() -> Envelope {
        Envelope {
            meta: b"{}".to_vec(),
            under: None,
            received_at: 1,
        }
    }

    #[test]
    fn a_staged_upload_is_published_once_and_is_then_an_item() {
        let mut shelf = MemoryShelf::default();
        let up = UploadId::generate(&mut SeededRandom::new(1));
        shelf.stage_create(&up);
        assert_eq!(shelf.stage_size(&up), Some(0));
        shelf.stage_write(&up, b"hello".to_vec()).unwrap();
        shelf.stage_write(&up, b"hi".to_vec()).unwrap();
        assert_eq!(shelf.stage_size(&up), Some(2));
        assert_eq!(shelf.staged(), vec![up.clone()]);

        let a = id("00000001-aaaaaaaaaaaa");
        assert!(shelf.publish(&up, 0, &a, envelope()).unwrap());
        assert!(shelf.staged().is_empty());
        let item = shelf.get(0, &a).unwrap();
        assert_eq!(item.content, b"hi");
        assert_eq!(shelf.bytes(0), 2);
        assert_eq!(shelf.ids(0), vec![a]);
    }

    #[test]
    fn publishing_never_replaces_an_item() {
        let mut shelf = MemoryShelf::default();
        let mut rng = SeededRandom::new(2);
        let a = id("00000001-aaaaaaaaaaaa");
        for (content, expect) in [(b"one".to_vec(), true), (b"two".to_vec(), false)] {
            let up = UploadId::generate(&mut rng);
            shelf.stage_create(&up);
            shelf.stage_write(&up, content).unwrap();
            assert_eq!(shelf.publish(&up, 0, &a, envelope()).unwrap(), expect);
        }
        assert_eq!(shelf.get(0, &a).unwrap().content, b"one");
        // The loser's staging is still there, for its owner to remove.
        assert_eq!(shelf.staged().len(), 1);
    }

    #[test]
    fn what_was_never_staged_cannot_be_written_or_published() {
        let mut shelf = MemoryShelf::default();
        let up = UploadId::generate(&mut SeededRandom::new(3));
        assert_eq!(
            shelf.stage_write(&up, vec![1]).unwrap_err().code(),
            "NOT_FOUND"
        );
        let a = id("00000001-aaaaaaaaaaaa");
        assert_eq!(
            shelf.publish(&up, 0, &a, envelope()).unwrap_err().code(),
            "NOT_FOUND"
        );
        shelf.stage_remove(&up);
    }

    #[test]
    fn generations_are_separate_listed_newest_first_and_dropped_whole() {
        let mut shelf = MemoryShelf::default();
        let mut rng = SeededRandom::new(4);
        for (generation, text) in [
            (0, "00000001-aaaaaaaaaaaa"),
            (0, "00000002-bbbbbbbbbbbb"),
            (1, "00000003-cccccccccccc"),
        ] {
            let up = UploadId::generate(&mut rng);
            shelf.stage_create(&up);
            shelf.stage_write(&up, vec![0; 3]).unwrap();
            shelf
                .publish(&up, generation, &id(text), envelope())
                .unwrap();
        }
        assert_eq!(
            shelf.ids(0),
            vec![id("00000002-bbbbbbbbbbbb"), id("00000001-aaaaaaaaaaaa")]
        );
        assert_eq!(shelf.generations(), vec![0, 1]);
        assert_eq!(
            shelf
                .remove(0, &id("00000001-aaaaaaaaaaaa"))
                .unwrap()
                .content
                .len(),
            3
        );
        assert!(shelf.remove(0, &id("00000001-aaaaaaaaaaaa")).is_none());
        shelf.drop_generation(0);
        shelf.drop_generation(0);
        assert_eq!(shelf.generations(), vec![1]);
        assert_eq!(shelf.bytes(0), 0);
        assert!(shelf.ids(7).is_empty());
    }
}
