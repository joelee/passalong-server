//! The shelf on a filesystem. One workspace, one directory:
//!
//! ```text
//! <root>/
//! ├── gen-<n>/items/<id>/
//! │   ├── content        the client's bytes, untouched
//! │   ├── meta.json      the client's bytes, untouched
//! │   └── server.json    the key id it came under, and when
//! ├── staging/<upload id>/content
//! ├── staging/<upload id>.sha256   the server's note for a plaintext upload
//! └── trash/             what is on its way out
//! ```
//!
//! Each method of [`ItemShelf`] is one step here. Publishing is a rename of
//! the staging directory onto the item's place. `rename` refuses a target
//! that is a non-empty directory, and a published item always holds files,
//! so of two publishes of one id the first wins and the second is told so.
//! An *empty* directory would be replaced, which is harmless: it is not an
//! item. Removing an item, or a generation, is a rename into `trash/`, so it
//! disappears whole; emptying the trash may be cut short and is finished
//! when the shelf is next opened.
//!
//! Path components come only from [`ItemId`], [`UploadId`], and numbers, all
//! of which are hex digits and at most one dash. Nothing a client sends
//! reaches a path any other way.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{Content, Envelope, ItemShelf, StoredItem, pump};
use crate::error::ApiError;
use crate::ids::{ItemId, KeyId, UploadId};

const CONTENT: &str = "content";
const META: &str = "meta.json";
const SERVER: &str = "server.json";

/// `server.json`: the envelope's fields that are the server's own.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerFile {
    under: Option<String>,
    received_at: u64,
}

/// An [`ItemShelf`] in a directory.
#[derive(Debug)]
pub struct FsShelf {
    root: PathBuf,
    /// Makes names in `trash/` distinct within this process; across
    /// processes the process id does.
    trashed: AtomicU64,
}

/// Logs what the filesystem said and reports only that the shelf cannot be
/// used. Paths here are made of ids, so they are safe to log; content and
/// `meta` never reach this function.
fn unavailable(action: &'static str, path: &Path, err: &std::io::Error) -> ApiError {
    tracing::error!(target: "passalong_server::shelf", action, path = %path.display(), %err, "the shelf cannot be used");
    ApiError::ServiceUnavailable
}

fn make_dir(path: &Path) -> Result<(), ApiError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|err| unavailable("create a directory", path, &err))
}

fn create_file(path: &Path) -> Result<File, ApiError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| unavailable("create a file", path, &err))
}

/// Writes a small file whole and flushes it to the disk.
fn write_file(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let mut file = create_file(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| unavailable("write a file", path, &err))
}

/// Flushes a directory, so that a rename into or out of it survives.
fn sync_dir(path: &Path) -> Result<(), ApiError> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| unavailable("flush a directory", path, &err))
}

/// The entries of a directory, or none when it does not exist.
fn entries(path: &Path) -> Result<Vec<(String, PathBuf)>, ApiError> {
    let reader = match fs::read_dir(path) {
        Ok(reader) => reader,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(unavailable("list a directory", path, &err)),
    };
    let mut found = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|err| unavailable("list a directory", path, &err))?;
        if let Ok(name) = entry.file_name().into_string() {
            found.push((name, entry.path()));
        }
    }
    Ok(found)
}

fn remove_tree(path: &Path) -> Result<(), ApiError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(unavailable("remove a directory", path, &err)),
    }
}

impl FsShelf {
    /// Opens the shelf in `root`, creating it when it is new, and empties
    /// the trash an earlier process may have left.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`] when the directory cannot be used.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, ApiError> {
        let shelf = Self {
            root: root.into(),
            trashed: AtomicU64::new(0),
        };
        make_dir(&shelf.root)?;
        make_dir(&shelf.staging())?;
        make_dir(&shelf.trash())?;
        shelf.empty_trash()?;
        shelf.sweep_digests()?;
        Ok(shelf)
    }

    fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }

    fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }

    /// Beside the staging directory, not in it: the directory becomes the
    /// item, and the digest is not part of an item.
    fn digest_file(&self, upload: &UploadId) -> PathBuf {
        self.staging().join(format!("{}.sha256", upload.as_str()))
    }

    fn staged_dir(&self, upload: &UploadId) -> PathBuf {
        self.staging().join(upload.as_str())
    }

    fn items(&self, generation: u64) -> PathBuf {
        self.root.join(format!("gen-{generation}")).join("items")
    }

    fn item_dir(&self, generation: u64, id: &ItemId) -> PathBuf {
        self.items(generation).join(id.as_str())
    }

    fn remove_digest(&self, upload: &UploadId) -> Result<(), ApiError> {
        let path = self.digest_file(upload);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(unavailable("remove a digest", &path, &err)),
        }
    }

    fn write(
        &self,
        upload: &UploadId,
        content: &mut dyn Read,
        announced: u64,
        hashed: bool,
    ) -> Result<u64, ApiError> {
        let dir = self.staged_dir(upload);
        if !dir.is_dir() {
            return Err(ApiError::NotFound);
        }
        // The digest of what was there is the digest of nothing now.
        self.remove_digest(upload)?;
        let path = dir.join(CONTENT);
        let mut file = create_file(&path)?;
        let mut hasher = hashed.then(sha2::Sha256::default);
        let written = pump(content, announced, hasher.as_mut(), |piece| {
            file.write_all(piece)
                .map_err(|err| unavailable("write content", &path, &err))
        });
        match written {
            Ok(total) => {
                // On the disk before anyone may publish it.
                file.sync_all()
                    .map_err(|err| unavailable("flush content", &path, &err))?;
                if let Some(hasher) = hasher {
                    let digest: [u8; 32] = sha2::Digest::finalize(hasher).into();
                    write_file(&self.digest_file(upload), &digest)?;
                }
                Ok(total)
            }
            Err(refusal) => {
                file.set_len(0)
                    .map_err(|err| unavailable("empty the staging place", &path, &err))?;
                Err(refusal)
            }
        }
    }

    /// Digests whose upload is gone: a publish or a removal was cut short
    /// between the directory and its note.
    fn sweep_digests(&self) -> Result<(), ApiError> {
        for (name, path) in entries(&self.staging())? {
            let orphan = name
                .strip_suffix(".sha256")
                .and_then(|upload| UploadId::parse(upload).ok())
                .is_some_and(|upload| !self.staged_dir(&upload).is_dir());
            if orphan {
                fs::remove_file(&path)
                    .map_err(|err| unavailable("remove a digest", &path, &err))?;
            }
        }
        Ok(())
    }

    fn empty_trash(&self) -> Result<(), ApiError> {
        for (_, path) in entries(&self.trash())? {
            remove_tree(&path)?;
        }
        Ok(())
    }

    /// Makes `path` disappear in one step, then removes it at leisure.
    /// `Ok(false)` when there was nothing at `path`.
    fn discard(&self, path: &Path) -> Result<bool, ApiError> {
        let name = format!(
            "{}-{}",
            std::process::id(),
            self.trashed.fetch_add(1, Ordering::SeqCst)
        );
        let bin = self.trash().join(name);
        match fs::rename(path, &bin) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(unavailable("move into the trash", path, &err)),
        }
        crate::fault::point("shelf: inside a removal");
        remove_tree(&bin)?;
        Ok(true)
    }

    /// An item's envelope and size, read from its directory. A directory
    /// without `content` is not an item.
    fn read_item(&self, dir: &Path) -> Result<Option<StoredItem>, ApiError> {
        let size = match fs::metadata(dir.join(CONTENT)) {
            Ok(meta) => meta.len(),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(unavailable("look at an item", dir, &err)),
        };
        let meta =
            fs::read(dir.join(META)).map_err(|err| unavailable("read meta.json", dir, &err))?;
        let server =
            fs::read(dir.join(SERVER)).map_err(|err| unavailable("read server.json", dir, &err))?;
        let server: ServerFile = serde_json::from_slice(&server).map_err(|err| {
            unavailable(
                "parse server.json",
                dir,
                &std::io::Error::other(err.to_string()),
            )
        })?;
        let under = match server.under {
            Some(text) => Some(KeyId::parse(&text).map_err(|_| {
                unavailable(
                    "parse server.json",
                    dir,
                    &std::io::Error::other("not a key id"),
                )
            })?),
            None => None,
        };
        Ok(Some(StoredItem {
            size,
            envelope: Envelope {
                meta,
                under,
                received_at: server.received_at,
            },
        }))
    }
}

impl ItemShelf for FsShelf {
    fn stage_create(&self, upload: &UploadId) -> Result<(), ApiError> {
        let dir = self.staged_dir(upload);
        make_dir(&dir)?;
        let content = dir.join(CONTENT);
        if !content.exists() {
            create_file(&content)?;
        }
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
        let path = self.digest_file(upload);
        match fs::read(&path) {
            Ok(bytes) => Ok(bytes.try_into().ok()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(unavailable("read a digest", &path, &err)),
        }
    }

    fn stage_size(&self, upload: &UploadId) -> Result<Option<u64>, ApiError> {
        let path = self.staged_dir(upload).join(CONTENT);
        match fs::metadata(&path) {
            Ok(meta) => Ok(Some(meta.len())),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
            Err(err) => Err(unavailable("look at staged content", &path, &err)),
        }
    }

    fn stage_remove(&self, upload: &UploadId) -> Result<(), ApiError> {
        self.discard(&self.staged_dir(upload))?;
        self.remove_digest(upload)
    }

    fn staged(&self) -> Result<Vec<UploadId>, ApiError> {
        let mut found: Vec<UploadId> = entries(&self.staging())?
            .into_iter()
            .filter_map(|(name, _)| UploadId::parse(&name).ok())
            .collect();
        found.sort();
        Ok(found)
    }

    fn publish(
        &self,
        upload: &UploadId,
        generation: u64,
        id: &ItemId,
        envelope: Envelope,
    ) -> Result<bool, ApiError> {
        let staged = self.staged_dir(upload);
        if !staged.is_dir() {
            return Err(ApiError::NotFound);
        }
        let server = ServerFile {
            under: envelope.under.as_ref().map(|key| key.as_str().to_owned()),
            received_at: envelope.received_at,
        };
        let server = serde_json::to_vec(&server).unwrap_or_default();
        write_file(&staged.join(META), &envelope.meta)?;
        write_file(&staged.join(SERVER), &server)?;
        sync_dir(&staged)?;

        let items = self.items(generation);
        make_dir(&items)?;
        let target = items.join(id.as_str());
        crate::fault::point("shelf: before the rename");
        match fs::rename(&staged, &target) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists
                ) =>
            {
                return Ok(false);
            }
            Err(err) => return Err(unavailable("publish an item", &target, &err)),
        }
        self.remove_digest(upload)?;
        sync_dir(&items)?;
        sync_dir(&self.staging())?;
        tracing::debug!(target: "passalong_server::shelf", generation, item = %id, "item published");
        Ok(true)
    }

    fn get(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        self.read_item(&self.item_dir(generation, id))
    }

    fn open_content_from(
        &self,
        generation: u64,
        id: &ItemId,
        offset: u64,
    ) -> Result<Option<Content>, ApiError> {
        let path = self.item_dir(generation, id).join(CONTENT);
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(unavailable("open content", &path, &err)),
        };
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|err| unavailable("seek in content", &path, &err))?;
        }
        Ok(Some(Box::new(file)))
    }

    fn ids(&self, generation: u64) -> Result<Vec<ItemId>, ApiError> {
        let mut found: Vec<ItemId> = entries(&self.items(generation))?
            .into_iter()
            .filter(|(_, path)| path.join(CONTENT).is_file())
            .filter_map(|(name, _)| ItemId::parse(&name).ok())
            .collect();
        found.sort();
        found.reverse();
        Ok(found)
    }

    fn remove(&self, generation: u64, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        let dir = self.item_dir(generation, id);
        let Some(item) = self.read_item(&dir)? else {
            return Ok(None);
        };
        if !self.discard(&dir)? {
            return Ok(None);
        }
        sync_dir(&self.items(generation))?;
        Ok(Some(item))
    }

    fn drop_generation(&self, generation: u64) -> Result<(), ApiError> {
        if self.discard(&self.root.join(format!("gen-{generation}")))? {
            sync_dir(&self.root)?;
        }
        Ok(())
    }

    fn generations(&self) -> Result<Vec<u64>, ApiError> {
        let mut found = Vec::new();
        for (name, _) in entries(&self.root)? {
            let Some(generation) = name
                .strip_prefix("gen-")
                .and_then(|number| number.parse::<u64>().ok())
            else {
                continue;
            };
            if !self.ids(generation)?.is_empty() {
                found.push(generation);
            }
        }
        found.sort_unstable();
        Ok(found)
    }

    fn bytes(&self, generation: u64) -> Result<u64, ApiError> {
        let mut total = 0;
        for id in self.ids(generation)? {
            let path = self.item_dir(generation, &id).join(CONTENT);
            total += fs::metadata(&path)
                .map_err(|err| unavailable("look at content", &path, &err))?
                .len();
        }
        Ok(total)
    }
}
