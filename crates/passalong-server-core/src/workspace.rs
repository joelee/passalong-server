//! One workspace: its encryption state, its generations of items, and the
//! changes to its encryption that re-encrypt nothing.
//!
//! An [`Engine`] is reached through `&mut self`, which stands for the
//! workspace lock: in the server, one mutex per workspace around a control
//! database transaction. Every method is one API operation of
//! `docs/api/README.md`, named in its documentation.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::clock::Clock;
use crate::error::ApiError;
use crate::ids::{ApiKeyId, ItemId, KeyId};
use crate::random::RandomSource;
use crate::shelf::{ItemShelf, StoredItem};

/// What an API key may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Read and write.
    ReadWrite,
    /// Read only.
    ReadOnly,
}

/// Who makes a request: the API key the authentication layer accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    /// The key's public id.
    pub key: ApiKeyId,
    /// The key's role.
    pub role: Role,
}

impl Caller {
    /// A caller.
    pub fn new(key: ApiKeyId, role: Role) -> Self {
        Self { key, role }
    }

    pub(crate) fn must_write(&self) -> Result<(), ApiError> {
        match self.role {
            Role::ReadWrite => Ok(()),
            Role::ReadOnly => Err(ApiError::ForbiddenRole),
        }
    }
}

/// The limits of one workspace, from the configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `limits.workspace_quota_bytes`.
    pub quota_bytes: u64,
    /// `limits.max_item_bytes`; `None` is "unlimited".
    pub max_item_bytes: Option<u64>,
    /// How long an upload ticket lives, and how long a committed upload's
    /// outcome is remembered for replays (`staging.max_age_hours`).
    pub staging_secs: u64,
    /// How long a rewrite session's lease lasts without a heartbeat.
    pub lease_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            quota_bytes: 20 * 1024 * 1024 * 1024,
            max_item_bytes: None,
            staging_secs: 24 * 60 * 60,
            lease_secs: 10 * 60,
        }
    }
}

/// Which items a request means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    /// The workspace's items.
    Current,
    /// The plaintext items a fresh start set aside.
    Plain,
}

/// The encryption state, as `getWorkspace` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionState {
    /// Items are stored as the client sent them, readable by the operator.
    Plaintext,
    /// Items are sealed by the clients under the data key `key_id` names.
    Sealed,
    /// A rewrite session is open; ordinary writes are refused.
    Rewriting,
}

/// A rewrite session, as `getRewrite` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    /// `migrate` or `rotate`.
    pub kind: crate::rewrite::RewriteKind,
    /// The key that holds the lease.
    pub holder: ApiKeyId,
    /// When the lease ends unless renewed.
    pub lease_expires_at: u64,
    /// The key id the workspace gets on commit.
    pub new_key_id: KeyId,
    /// The ids already staged, newest first, so a resumed run skips them.
    pub staged_ids: Vec<ItemId>,
    /// How many items the source generation holds.
    pub source_items: usize,
}

/// `getWorkspace`'s `encryption`. While a rewrite is open, `key_id` and
/// `header` are still those readers need: the ones before the rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionView {
    /// The state.
    pub state: EncryptionState,
    /// The current data key's id; `None` for plaintext.
    pub key_id: Option<KeyId>,
    /// The wrapped data key, opaque to the server.
    pub header: Option<Vec<u8>>,
    /// The open session, exactly when `state` is `Rewriting`.
    pub rewrite: Option<SessionView>,
}

/// A workspace's key id and header while it is sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seal {
    pub(crate) key_id: KeyId,
    pub(crate) header: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    /// `None` is plaintext.
    Settled(Option<Seal>),
    Rewriting(crate::rewrite::Session),
}

/// One workspace and the rules of its API operations.
pub struct Engine<S> {
    pub(crate) shelf: S,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) rng: Box<dyn RandomSource>,
    pub(crate) limits: Limits,
    pub(crate) state: State,
    /// The generation that holds the workspace's items.
    pub(crate) generation: u64,
    /// The generation a fresh start set aside, if it held anything.
    pub(crate) plain: Option<u64>,
    pub(crate) next_generation: u64,
    /// Bytes published per live generation: what the control database
    /// believes, which the model test compares with the shelf.
    pub(crate) used: BTreeMap<u64, u64>,
    pub(crate) uploads: crate::upload::Uploads,
    /// The new key ids of aborted rewrites, kept for good: a handful of
    /// short strings per workspace, against a duplicate `beginRewrite` that
    /// may arrive at any time.
    pub(crate) ended_rewrites: std::collections::BTreeSet<KeyId>,
}

impl<S: ItemShelf> Engine<S> {
    /// A new, empty, plaintext workspace over `shelf`.
    pub fn new(
        shelf: S,
        clock: Arc<dyn Clock>,
        rng: Box<dyn RandomSource>,
        limits: Limits,
    ) -> Self {
        Self {
            shelf,
            clock,
            rng,
            limits,
            state: State::Settled(None),
            generation: 0,
            plain: None,
            next_generation: 1,
            used: BTreeMap::new(),
            uploads: crate::upload::Uploads::default(),
            ended_rewrites: std::collections::BTreeSet::new(),
        }
    }

    /// The shelf, for inspection.
    pub fn shelf(&self) -> &S {
        &self.shelf
    }

    /// The seal readers and ordinary writers go by: during a rewrite, the
    /// one before it.
    pub(crate) fn seal(&self) -> Option<&Seal> {
        match &self.state {
            State::Settled(seal) => seal.as_ref(),
            State::Rewriting(session) => session.prior.as_ref(),
        }
    }

    /// `getWorkspace`: the encryption part.
    pub fn encryption(&self) -> EncryptionView {
        let seal = self.seal();
        EncryptionView {
            state: match (&self.state, seal) {
                (State::Rewriting(_), _) => EncryptionState::Rewriting,
                (_, Some(_)) => EncryptionState::Sealed,
                (_, None) => EncryptionState::Plaintext,
            },
            key_id: seal.map(|seal| seal.key_id.clone()),
            header: seal.map(|seal| seal.header.clone()),
            rewrite: self.session_view(),
        }
    }

    /// Refuses ordinary writes during a rewrite, and writes made under a
    /// key id other than the workspace's. This one check, made under the
    /// workspace lock, replaces the client's reading of the header before
    /// and after every `put`.
    pub(crate) fn check_writable(&self, expected: Option<&KeyId>) -> Result<(), ApiError> {
        match &self.state {
            State::Rewriting(_) => Err(ApiError::RewriteInProgress),
            State::Settled(seal) if seal.as_ref().map(|seal| &seal.key_id) == expected => Ok(()),
            State::Settled(_) => Err(ApiError::KeyIdMismatch),
        }
    }

    fn generation_of(&self, partition: Partition) -> Option<u64> {
        match partition {
            Partition::Current => Some(self.generation),
            Partition::Plain => self.plain,
        }
    }

    /// `listItemIds`: newest first.
    pub fn item_ids(&self, partition: Partition) -> Vec<ItemId> {
        self.generation_of(partition)
            .map(|generation| self.shelf.ids(generation))
            .unwrap_or_default()
    }

    /// `getItem` and `getItemContent`.
    pub fn item(&self, partition: Partition, id: &ItemId) -> Option<StoredItem> {
        self.shelf.get(self.generation_of(partition)?, id)
    }

    /// `findByContentKey`: the oldest item with that content key.
    pub fn find_by_content_key(&self, content_key: &str) -> Option<ItemId> {
        find_in(&self.shelf, self.generation, content_key)
    }

    /// Bytes published in every generation the workspace still points to.
    pub fn used_bytes(&self) -> u64 {
        self.live_generations()
            .iter()
            .map(|generation| self.used.get(generation).copied().unwrap_or(0))
            .sum()
    }

    /// The generations the workspace points to: its items, the plain
    /// partition, and a rewrite's staged generation. Any other generation
    /// on the shelf is rubbish an interrupted change left behind.
    pub fn live_generations(&self) -> Vec<u64> {
        let mut live = vec![self.generation];
        live.extend(self.plain);
        if let State::Rewriting(session) = &self.state {
            live.push(session.staged_generation);
        }
        live
    }

    pub(crate) fn add_used(&mut self, generation: u64, bytes: u64) {
        *self.used.entry(generation).or_default() += bytes;
    }

    /// `deleteItem`. In an encrypted workspace the caller proves with
    /// `expected` that it holds the current key.
    ///
    /// # Errors
    ///
    /// [`ApiError::ForbiddenRole`], [`ApiError::RewriteInProgress`],
    /// [`ApiError::KeyIdMismatch`], and [`ApiError::NotFound`], which a
    /// client that repeats a delete treats as done.
    pub fn delete_item(
        &mut self,
        caller: &Caller,
        partition: Partition,
        id: &ItemId,
        expected: Option<&KeyId>,
    ) -> Result<StoredItem, ApiError> {
        caller.must_write()?;
        self.check_writable(expected)?;
        let generation = self.generation_of(partition).ok_or(ApiError::NotFound)?;
        let item = self
            .shelf
            .remove(generation, id)
            .ok_or(ApiError::NotFound)?;
        let used = self.used.entry(generation).or_default();
        *used = used.saturating_sub(item.content.len() as u64);
        Ok(item)
    }

    /// `enableEncryption`: seals an empty plaintext workspace. Sent again
    /// with the same key id, it answers the state it already produced.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidRequest`] while the workspace holds items, and
    /// the refusals `freshStart` shares: [`ApiError::ForbiddenRole`],
    /// [`ApiError::RewriteInProgress`], and [`ApiError::KeyIdMismatch`] for
    /// a workspace sealed under another key.
    pub fn enable_encryption(
        &mut self,
        caller: &Caller,
        key_id: KeyId,
        header: Vec<u8>,
    ) -> Result<EncryptionView, ApiError> {
        if self.settle(caller, &key_id)? {
            return Ok(self.encryption());
        }
        if !self.shelf.ids(self.generation).is_empty() {
            return Err(ApiError::InvalidRequest(
                "the workspace holds items; use a fresh start or migrate".to_owned(),
            ));
        }
        self.state = State::Settled(Some(Seal { key_id, header }));
        Ok(self.encryption())
    }

    /// `freshStart`: seals a plaintext workspace without re-encrypting
    /// anything. Its items become the plain partition by moving one
    /// pointer, so the change is a single step however many items there
    /// are. Sent again with the same key id, it answers the same state.
    ///
    /// # Errors
    ///
    /// As [`Engine::enable_encryption`], except that items are welcome.
    pub fn fresh_start(
        &mut self,
        caller: &Caller,
        key_id: KeyId,
        header: Vec<u8>,
    ) -> Result<EncryptionView, ApiError> {
        if self.settle(caller, &key_id)? {
            return Ok(self.encryption());
        }
        if !self.shelf.ids(self.generation).is_empty() {
            self.plain = Some(self.generation);
            self.generation = self.next_generation;
            self.next_generation += 1;
        }
        self.state = State::Settled(Some(Seal { key_id, header }));
        Ok(self.encryption())
    }

    /// What `enableEncryption` and `freshStart` share: `Ok(true)` when the
    /// workspace is already sealed under `key_id` (a replay), `Ok(false)`
    /// when it is plaintext and may be sealed.
    fn settle(&self, caller: &Caller, key_id: &KeyId) -> Result<bool, ApiError> {
        caller.must_write()?;
        match &self.state {
            State::Rewriting(_) => Err(ApiError::RewriteInProgress),
            State::Settled(Some(seal)) if &seal.key_id == key_id => Ok(true),
            State::Settled(Some(_)) => Err(ApiError::KeyIdMismatch),
            State::Settled(None) => Ok(false),
        }
    }

    /// `replaceHeader`: the change of words. The data key stays, so no item
    /// is touched and every device that joined keeps working. Repeating it
    /// writes the same header again.
    ///
    /// # Errors
    ///
    /// [`ApiError::InvalidRequest`] for a plaintext workspace, and the usual
    /// refusals.
    pub fn replace_header(
        &mut self,
        caller: &Caller,
        expected: &KeyId,
        header: Vec<u8>,
    ) -> Result<EncryptionView, ApiError> {
        caller.must_write()?;
        match &mut self.state {
            State::Rewriting(_) => return Err(ApiError::RewriteInProgress),
            State::Settled(None) => {
                return Err(ApiError::InvalidRequest(
                    "the workspace is not encrypted".to_owned(),
                ));
            }
            State::Settled(Some(seal)) if &seal.key_id != expected => {
                return Err(ApiError::KeyIdMismatch);
            }
            State::Settled(Some(seal)) => seal.header = header,
        }
        Ok(self.encryption())
    }

    #[cfg(test)]
    pub(crate) fn seed_for_tests(&mut self, id: &ItemId, content: &[u8], under: Option<KeyId>) {
        let upload = crate::ids::UploadId::generate(self.rng.as_mut());
        self.shelf.stage_create(&upload);
        self.shelf.stage_write(&upload, content.to_vec()).unwrap();
        let envelope = crate::shelf::Envelope {
            meta: b"{}".to_vec(),
            under,
            received_at: self.clock.now(),
        };
        assert!(
            self.shelf
                .publish(&upload, self.generation, id, envelope)
                .unwrap()
        );
        self.add_used(self.generation, content.len() as u64);
    }
}

/// The oldest item of `generation` whose content key is `content_key`.
pub(crate) fn find_in<S: ItemShelf>(
    shelf: &S,
    generation: u64,
    content_key: &str,
) -> Option<ItemId> {
    shelf
        .ids(generation)
        .into_iter()
        .rev()
        .find(|id| id.content_key() == content_key)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use crate::random::SeededRandom;
    use crate::shelf::MemoryShelf;

    pub(crate) fn key(text: &str) -> KeyId {
        KeyId::parse(text).unwrap()
    }

    pub(crate) fn rw(name: &str) -> Caller {
        Caller::new(ApiKeyId::new(name), Role::ReadWrite)
    }

    pub(crate) fn ro(name: &str) -> Caller {
        Caller::new(ApiKeyId::new(name), Role::ReadOnly)
    }

    pub(crate) fn engine() -> (Engine<MemoryShelf>, ManualClock) {
        let clock = ManualClock::at(1_000);
        let engine = Engine::new(
            MemoryShelf::default(),
            Arc::new(clock.clone()),
            Box::new(SeededRandom::new(42)),
            Limits::default(),
        );
        (engine, clock)
    }

    /// Publishes an item straight onto the shelf, as a finished upload would.
    pub(crate) fn seed(engine: &mut Engine<MemoryShelf>, id: &str, content: &[u8]) {
        let id = ItemId::parse(id).unwrap();
        let under = engine.encryption().key_id;
        engine.seed_for_tests(&id, content, under);
    }

    #[test]
    fn a_new_workspace_is_plaintext_and_empty() {
        let (engine, _) = engine();
        let view = engine.encryption();
        assert_eq!(view.state, EncryptionState::Plaintext);
        assert!(view.key_id.is_none() && view.header.is_none() && view.rewrite.is_none());
        assert!(engine.item_ids(Partition::Current).is_empty());
        assert_eq!(engine.used_bytes(), 0);
    }

    #[test]
    fn enabling_encryption_needs_an_empty_plaintext_workspace_and_a_writer() {
        let (mut engine, _) = engine();
        let err = engine
            .enable_encryption(&ro("kiosk"), key("aa"), b"h".to_vec())
            .unwrap_err();
        assert_eq!(err, ApiError::ForbiddenRole);

        let view = engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        assert_eq!(view.state, EncryptionState::Sealed);
        assert_eq!(view.key_id, Some(key("aa")));
        assert_eq!(view.header.as_deref(), Some(&b"h"[..]));

        // Sent again, by anyone, it answers the state it already produced.
        let again = engine
            .enable_encryption(&rw("b"), key("aa"), b"h".to_vec())
            .unwrap();
        assert_eq!(again, view);
        // Under another key it is a refusal, and changes nothing.
        let err = engine
            .enable_encryption(&rw("b"), key("bb"), b"x".to_vec())
            .unwrap_err();
        assert_eq!(err, ApiError::KeyIdMismatch);
        assert_eq!(engine.encryption(), view);
    }

    #[test]
    fn enabling_encryption_is_refused_while_the_workspace_holds_items() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"text");
        let err = engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_REQUEST");
        assert_eq!(engine.encryption().state, EncryptionState::Plaintext);
    }

    #[test]
    fn replacing_the_header_needs_the_current_key_id_and_touches_no_item() {
        let (mut engine, _) = engine();
        let err = engine
            .replace_header(&rw("a"), &key("aa"), b"h2".to_vec())
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_REQUEST");

        engine
            .enable_encryption(&rw("a"), key("aa"), b"h1".to_vec())
            .unwrap();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"sealed");
        let err = engine
            .replace_header(&ro("k"), &key("aa"), b"h2".to_vec())
            .unwrap_err();
        assert_eq!(err, ApiError::ForbiddenRole);
        let err = engine
            .replace_header(&rw("a"), &key("bb"), b"h2".to_vec())
            .unwrap_err();
        assert_eq!(err, ApiError::KeyIdMismatch);

        let view = engine
            .replace_header(&rw("a"), &key("aa"), b"h2".to_vec())
            .unwrap();
        assert_eq!(view.header.as_deref(), Some(&b"h2"[..]));
        assert_eq!(view.key_id, Some(key("aa")));
        assert_eq!(
            engine
                .replace_header(&rw("a"), &key("aa"), b"h2".to_vec())
                .unwrap(),
            view
        );
        assert_eq!(engine.item_ids(Partition::Current).len(), 1);
    }

    #[test]
    fn a_fresh_start_sets_the_items_aside_in_one_step() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"plain text");
        assert_eq!(
            engine
                .fresh_start(&ro("k"), key("aa"), b"h".to_vec())
                .unwrap_err(),
            ApiError::ForbiddenRole
        );

        let view = engine
            .fresh_start(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        assert_eq!(view.state, EncryptionState::Sealed);
        assert!(engine.item_ids(Partition::Current).is_empty());
        assert_eq!(engine.item_ids(Partition::Plain).len(), 1);
        let kept = engine
            .item(
                Partition::Plain,
                &ItemId::parse("00000001-aaaaaaaaaaaa").unwrap(),
            )
            .unwrap();
        assert_eq!(kept.content, b"plain text");
        assert_eq!(engine.used_bytes(), 10);

        // Sent again it answers the same state and sets nothing else aside.
        seed(&mut engine, "00000002-bbbbbbbbbbbb", b"sealed");
        assert_eq!(
            engine
                .fresh_start(&rw("a"), key("aa"), b"h".to_vec())
                .unwrap(),
            view
        );
        assert_eq!(engine.item_ids(Partition::Current).len(), 1);
        assert_eq!(
            engine
                .fresh_start(&rw("a"), key("bb"), b"h".to_vec())
                .unwrap_err(),
            ApiError::KeyIdMismatch
        );
    }

    #[test]
    fn a_fresh_start_of_an_empty_workspace_leaves_no_plain_partition() {
        let (mut engine, _) = engine();
        engine
            .fresh_start(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        assert!(engine.item_ids(Partition::Plain).is_empty());
        assert_eq!(engine.live_generations().len(), 1);
    }

    #[test]
    fn items_are_deleted_under_the_current_key_only() {
        let (mut engine, _) = engine();
        engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"12345");
        let id = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();
        assert_eq!(
            engine
                .delete_item(&ro("k"), Partition::Current, &id, Some(&key("aa")))
                .unwrap_err(),
            ApiError::ForbiddenRole
        );
        assert_eq!(
            engine
                .delete_item(&rw("a"), Partition::Current, &id, None)
                .unwrap_err(),
            ApiError::KeyIdMismatch
        );
        assert_eq!(
            engine
                .delete_item(&rw("a"), Partition::Current, &id, Some(&key("bb")))
                .unwrap_err(),
            ApiError::KeyIdMismatch
        );
        let gone = engine
            .delete_item(&rw("a"), Partition::Current, &id, Some(&key("aa")))
            .unwrap();
        assert_eq!(gone.content, b"12345");
        assert_eq!(engine.used_bytes(), 0);
        assert_eq!(
            engine
                .delete_item(&rw("a"), Partition::Current, &id, Some(&key("aa")))
                .unwrap_err(),
            ApiError::NotFound
        );
        assert_eq!(
            engine
                .delete_item(&rw("a"), Partition::Plain, &id, Some(&key("aa")))
                .unwrap_err(),
            ApiError::NotFound
        );
    }

    #[test]
    fn the_content_key_finds_the_oldest_item_with_that_content() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000002-aaaaaaaaaaaa", b"x");
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"x");
        seed(&mut engine, "00000003-bbbbbbbbbbbb", b"y");
        let found = engine.find_by_content_key("aaaaaaaaaaaa").unwrap();
        assert_eq!(found.as_str(), "00000001-aaaaaaaaaaaa");
        assert!(engine.find_by_content_key("cccccccccccc").is_none());
    }
}
