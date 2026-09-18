//! Where items are kept. The rules of this crate need only the few
//! operations below, each of which a POSIX filesystem performs in one step,
//! so nothing proved over [`MemoryShelf`] rests on atomicity the real store
//! cannot give. `tests/shelf_conformance.rs` holds every implementation to
//! the same behaviour.
//!
//! Content streams in and out: no method takes or returns a whole item.
//! Every method can fail, because a filesystem can; a shelf reports what it
//! cannot do as [`ApiError::ServiceUnavailable`], and the server fails
//! closed.

mod fs;
mod memory;

pub use fs::FsShelf;
pub use memory::MemoryShelf;

use std::io::Read;

use crate::error::ApiError;
use crate::ids::{ItemId, KeyId, UploadId};

/// The most a shelf asks of a content reader at once, and so the most of an
/// item a shelf holds in memory while it stores it.
pub const CHUNK_BYTES: usize = 64 * 1024;

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

/// A published item, without its content; see [`ItemShelf::open_content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItem {
    /// The content's size in bytes, as stored.
    pub size: u64,
    /// See [`Envelope`].
    pub envelope: Envelope,
}

/// An item's content, as a stream.
pub type Content = Box<dyn Read + Send>;

/// Storage for the items of one workspace, in numbered generations.
pub trait ItemShelf {
    /// Creates the staging place of an upload; harmless when it exists.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`], as every method.
    fn stage_create(&self, upload: &UploadId) -> Result<(), ApiError>;

    /// Replaces the staged content with what `content` yields, read in
    /// pieces of at most [`CHUNK_BYTES`], and returns how much that was.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] when nothing is staged under `upload`;
    /// [`ApiError::ContentMismatch`], leaving the staging place empty, when
    /// `content` yields more than `announced` bytes;
    /// [`ApiError::InvalidRequest`] when `content` fails, as when a client
    /// goes away mid-upload.
    fn stage_write(
        &self,
        upload: &UploadId,
        content: &mut dyn Read,
        announced: u64,
    ) -> Result<u64, ApiError>;

    /// The staged content's size, or `None` when nothing is staged.
    fn stage_size(&self, upload: &UploadId) -> Result<Option<u64>, ApiError>;

    /// Removes a staging place; harmless when there is none.
    fn stage_remove(&self, upload: &UploadId) -> Result<(), ApiError>;

    /// Every staging place, for the janitor and for leak checks.
    fn staged(&self) -> Result<Vec<UploadId>, ApiError>;

    /// Turns a staged upload into the item `id` of `generation`, in one
    /// step, unless that item exists: then it answers `false`, replaces
    /// nothing, and leaves the staging place for the caller to remove.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] when nothing is staged under `upload`.
    fn publish(
        &self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError>;

    /// One item, without its content.
    fn get(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError>;

    /// One item's content, from its first byte.
    fn open_content(&self, generation: u64, id: &ItemId) -> Result<Option<Content>, ApiError>;

    /// A generation's ids, newest first.
    fn ids(&self, generation: u64) -> Result<Vec<ItemId>, ApiError>;

    /// Removes one item and returns what it was.
    fn remove(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError>;

    /// Removes a whole generation. Harmless when there is none, and safe to
    /// repeat after it was cut short.
    fn drop_generation(&self, generation: u64) -> Result<(), ApiError>;

    /// The generations that hold anything.
    fn generations(&self) -> Result<Vec<u64>, ApiError>;

    /// The content bytes a generation holds.
    fn bytes(&self, generation: u64) -> Result<u64, ApiError>;
}

/// A shelf shared between engines, or between an engine and a janitor.
impl<S: ItemShelf + ?Sized> ItemShelf for std::sync::Arc<S> {
    fn stage_create(&self, upload: &UploadId) -> Result<(), ApiError> {
        (**self).stage_create(upload)
    }
    fn stage_write(
        &self,
        upload: &UploadId,
        content: &mut dyn Read,
        announced: u64,
    ) -> Result<u64, ApiError> {
        (**self).stage_write(upload, content, announced)
    }
    fn stage_size(&self, upload: &UploadId) -> Result<Option<u64>, ApiError> {
        (**self).stage_size(upload)
    }
    fn stage_remove(&self, upload: &UploadId) -> Result<(), ApiError> {
        (**self).stage_remove(upload)
    }
    fn staged(&self) -> Result<Vec<UploadId>, ApiError> {
        (**self).staged()
    }
    fn publish(
        &self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError> {
        (**self).publish(upload, generation, id, envelope)
    }
    fn get(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        (**self).get(generation, id)
    }
    fn open_content(&self, generation: u64, id: &ItemId) -> Result<Option<Content>, ApiError> {
        (**self).open_content(generation, id)
    }
    fn ids(&self, generation: u64) -> Result<Vec<ItemId>, ApiError> {
        (**self).ids(generation)
    }
    fn remove(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        (**self).remove(generation, id)
    }
    fn drop_generation(&self, generation: u64) -> Result<(), ApiError> {
        (**self).drop_generation(generation)
    }
    fn generations(&self) -> Result<Vec<u64>, ApiError> {
        (**self).generations()
    }
    fn bytes(&self, generation: u64) -> Result<u64, ApiError> {
        (**self).bytes(generation)
    }
}

/// Reads `content` to its end in pieces of [`CHUNK_BYTES`], handing each to
/// `sink`, and refuses more than `announced` bytes.
pub(crate) fn pump(
    content: &mut dyn Read,
    announced: u64,
    mut sink: impl FnMut(&[u8]) -> Result<(), ApiError>,
) -> Result<u64, ApiError> {
    let mut piece = vec![0_u8; CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let n = match content.read(&mut piece) {
            Ok(0) => return Ok(total),
            Ok(n) => n,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                return Err(ApiError::InvalidRequest(
                    "the content could not be read to its end".to_owned(),
                ));
            }
        };
        total += n as u64;
        if total > announced {
            return Err(ApiError::ContentMismatch);
        }
        sink(&piece[..n])?;
    }
}
