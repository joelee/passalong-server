//! One workspace: its encryption state, its generations of items, and the
//! changes to its encryption that re-encrypt nothing.
//!
//! Every operation of an [`Engine`] takes `&self` and runs as one ledger
//! transaction, which is the workspace's lock: exclusive across threads and,
//! with SQLite, across processes. So an engine is shared as it is. Every method is one API operation of
//! `docs/api/README.md`, named in its documentation.

use std::io::Read;
use std::sync::{Arc, Mutex, PoisonError};

use crate::clock::Clock;
use crate::error::ApiError;
use crate::ids::{ApiKeyId, ItemId, KeyId, UploadId};
use crate::ledger::{Ledger, MemoryLedger, WorkspaceId, WorkspaceRecord};
use crate::random::RandomSource;
use crate::rewrite::RewriteRequest;
use crate::shelf::{Content, ItemShelf, StoredItem};
use crate::upload::{Begun, PutOutcome, UploadRequest};

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
    /// Whether, in a plaintext workspace, a commit checks the content's
    /// SHA-256 and size against `meta` and the id. The server does; tests of
    /// other rules, which use made-up ids, switch it off.
    pub check_plaintext_content: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            quota_bytes: 20 * 1024 * 1024 * 1024,
            max_item_bytes: None,
            staging_secs: 24 * 60 * 60,
            lease_secs: 10 * 60,
            check_plaintext_content: true,
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

/// What `resolveItem` answers: what a user typed, turned into one item, by
/// the passalong client's rules. It works on ids alone, so as well in an
/// encrypted workspace as in a plaintext one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one item matches.
    Id(ItemId),
    /// Several do; newest first.
    Ambiguous(Vec<ItemId>),
    /// None does.
    NotFound,
    /// Fewer than 4 characters, or something other than hex digits and at
    /// most one dash.
    InvalidPrefix,
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

/// The rules of one workspace, at work on its record. Every method is one
/// API operation of `docs/api/README.md`, or a part of one, and [`Engine`]
/// runs each inside a [`Ledger`] transaction.
pub struct Rules<'a, S> {
    pub(crate) rec: &'a mut WorkspaceRecord,
    pub(crate) shelf: &'a S,
    pub(crate) clock: &'a dyn Clock,
    /// Locked for the instant an id is made, and never across a rule: a rule
    /// may spend minutes writing content to the shelf.
    pub(crate) rng: &'a Mutex<Box<dyn RandomSource>>,
    pub(crate) limits: &'a Limits,
    /// What to remove from the shelf once the record is safely stored.
    pub(crate) after: Vec<Cleanup>,
}

/// A destructive shelf step. The rules never take one while the record still
/// points at what it destroys: they ask for it, and [`Engine`] performs it
/// after the transaction has committed. If the process dies first, the
/// janitor finds the same rubbish later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cleanup {
    DropGeneration(u64),
    RemoveStaging(UploadId),
}

impl<S: ItemShelf> Rules<'_, S> {
    /// A new upload id from the engine's random source.
    pub(crate) fn new_upload_id(&self) -> UploadId {
        let mut rng = self.rng.lock().unwrap_or_else(PoisonError::into_inner);
        UploadId::generate(rng.as_mut())
    }

    /// The seal readers and ordinary writers go by: during a rewrite, the
    /// one before it.
    pub(crate) fn seal(&self) -> Option<&Seal> {
        match &self.rec.state {
            State::Settled(seal) => seal.as_ref(),
            State::Rewriting(session) => session.prior.as_ref(),
        }
    }

    /// `getWorkspace`: the encryption part.
    pub fn encryption(&self) -> Result<EncryptionView, ApiError> {
        let rewrite = self.session_view()?;
        let seal = self.seal();
        Ok(EncryptionView {
            state: match (&self.rec.state, seal) {
                (State::Rewriting(_), _) => EncryptionState::Rewriting,
                (_, Some(_)) => EncryptionState::Sealed,
                (_, None) => EncryptionState::Plaintext,
            },
            key_id: seal.map(|seal| seal.key_id.clone()),
            header: seal.map(|seal| seal.header.clone()),
            rewrite,
        })
    }

    /// Refuses ordinary writes during a rewrite, and writes made under a
    /// key id other than the workspace's. This one check, made under the
    /// workspace lock, replaces the client's reading of the header before
    /// and after every `put`.
    pub(crate) fn check_writable(&self, expected: Option<&KeyId>) -> Result<(), ApiError> {
        match &self.rec.state {
            State::Rewriting(_) => Err(ApiError::RewriteInProgress),
            State::Settled(seal) if seal.as_ref().map(|seal| &seal.key_id) == expected => Ok(()),
            State::Settled(_) => Err(ApiError::KeyIdMismatch),
        }
    }

    fn generation_of(&self, partition: Partition) -> Option<u64> {
        match partition {
            Partition::Current => Some(self.rec.generation),
            Partition::Plain => self.rec.plain,
        }
    }

    /// `listItemIds`: newest first.
    pub fn item_ids(&self, partition: Partition) -> Result<Vec<ItemId>, ApiError> {
        match self.generation_of(partition) {
            Some(generation) => self.shelf.ids(generation),
            None => Ok(Vec::new()),
        }
    }

    /// `listItemIds` with `after`: the ids newer than `after`, newest first.
    /// Ids begin with their creation time, so comparing them is enough.
    pub fn item_ids_after(
        &self,
        partition: Partition,
        after: Option<&ItemId>,
    ) -> Result<Vec<ItemId>, ApiError> {
        let mut ids = self.item_ids(partition)?;
        if let Some(after) = after {
            ids.retain(|id| id > after);
        }
        Ok(ids)
    }

    /// `listItems`: envelopes and sizes, newest first, in one answer.
    pub fn items_after(
        &self,
        partition: Partition,
        after: Option<&ItemId>,
    ) -> Result<Vec<(ItemId, StoredItem)>, ApiError> {
        let mut items = Vec::new();
        for id in self.item_ids_after(partition, after)? {
            // An item deleted between the listing and the look is left out.
            if let Some(item) = self.item(partition, &id)? {
                items.push((id, item));
            }
        }
        Ok(items)
    }

    /// `resolveItem`. Input without a dash is a prefix of the content key,
    /// such as `2cf2`; with a dash, of the whole id. Case and surrounding
    /// spaces are ignored; at least 4 characters are required.
    pub fn resolve_item(&self, input: &str) -> Result<Resolved, ApiError> {
        let typed = input.trim().to_ascii_lowercase();
        let well_formed = typed.len() >= 4
            && typed.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            && typed.bytes().filter(|b| *b == b'-').count() <= 1;
        if !well_formed {
            return Ok(Resolved::InvalidPrefix);
        }
        let whole_id = typed.contains('-');
        let mut matches: Vec<ItemId> = self
            .item_ids(Partition::Current)?
            .into_iter()
            .filter(|id| {
                if whole_id {
                    id.as_str().starts_with(&typed)
                } else {
                    id.content_key().starts_with(&typed)
                }
            })
            .collect();
        Ok(match matches.len() {
            0 => Resolved::NotFound,
            1 => Resolved::Id(matches.remove(0)),
            _ => Resolved::Ambiguous(matches),
        })
    }

    /// `getItem`: the envelope and the size.
    pub fn item(&self, partition: Partition, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        match self.generation_of(partition) {
            Some(generation) => self.shelf.get(generation, id),
            None => Ok(None),
        }
    }

    /// `getItemContent`: the bytes, as a stream the caller reads after the
    /// workspace's lock is released.
    pub fn item_content(
        &self,
        partition: Partition,
        id: &ItemId,
    ) -> Result<Option<Content>, ApiError> {
        self.item_content_from(partition, id, 0)
    }

    /// `getItemContent` with `Range`: the bytes from `offset` on.
    pub fn item_content_from(
        &self,
        partition: Partition,
        id: &ItemId,
        offset: u64,
    ) -> Result<Option<Content>, ApiError> {
        match self.generation_of(partition) {
            Some(generation) => self.shelf.open_content_from(generation, id, offset),
            None => Ok(None),
        }
    }

    /// `findByContentKey`: the oldest item with that content key.
    pub fn find_by_content_key(&self, content_key: &str) -> Result<Option<ItemId>, ApiError> {
        find_in(self.shelf, self.rec.generation, content_key)
    }

    /// Bytes published in every generation the workspace still points to.
    pub fn used_bytes(&self) -> u64 {
        self.live_generations()
            .iter()
            .map(|generation| self.rec.used.get(generation).copied().unwrap_or(0))
            .sum()
    }

    /// The generations the workspace points to: its items, the plain
    /// partition, and a rewrite's staged generation. Any other generation
    /// on the shelf is rubbish an interrupted change left behind.
    pub fn live_generations(&self) -> Vec<u64> {
        let mut live = vec![self.rec.generation];
        live.extend(self.rec.plain);
        if let State::Rewriting(session) = &self.rec.state {
            live.push(session.staged_generation);
        }
        live
    }

    pub(crate) fn add_used(&mut self, generation: u64, bytes: u64) {
        *self.rec.used.entry(generation).or_default() += bytes;
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
            .remove(generation, id)?
            .ok_or(ApiError::NotFound)?;
        let used = self.rec.used.entry(generation).or_default();
        *used = used.saturating_sub(item.size);
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
            return self.encryption();
        }
        if !self.shelf.ids(self.rec.generation)?.is_empty() {
            return Err(ApiError::InvalidRequest(
                "the workspace holds items; use a fresh start or migrate".to_owned(),
            ));
        }
        self.rec.state = State::Settled(Some(Seal { key_id, header }));
        self.encryption()
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
            return self.encryption();
        }
        if !self.shelf.ids(self.rec.generation)?.is_empty() {
            self.rec.plain = Some(self.rec.generation);
            self.rec.generation = self.rec.next_generation;
            self.rec.next_generation += 1;
        }
        self.rec.state = State::Settled(Some(Seal { key_id, header }));
        self.encryption()
    }

    /// What `enableEncryption` and `freshStart` share: `Ok(true)` when the
    /// workspace is already sealed under `key_id` (a replay), `Ok(false)`
    /// when it is plaintext and may be sealed.
    fn settle(&self, caller: &Caller, key_id: &KeyId) -> Result<bool, ApiError> {
        caller.must_write()?;
        match &self.rec.state {
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
        match &mut self.rec.state {
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
        self.encryption()
    }
}

/// One workspace: its shelf, its record in the ledger, and the rules between
/// them. Each method is one API operation, run as one ledger transaction,
/// which is also the workspace's lock; [`Rules`] documents what each does
/// and how it is refused. Operations that only read work on a copy of the
/// record and store nothing.
///
/// Every method can also fail with [`ApiError::ServiceUnavailable`], when
/// the ledger cannot be used: the server fails closed.
pub struct Engine<S, L = MemoryLedger> {
    pub(crate) shelf: S,
    pub(crate) ledger: L,
    pub(crate) workspace: WorkspaceId,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) rng: Mutex<Box<dyn RandomSource>>,
    pub(crate) limits: Limits,
}

impl<S: ItemShelf, L: Ledger> Engine<S, L> {
    /// Opens `workspace` over `shelf` and `ledger`, creating its record when
    /// it is new, and repairing what an unclean stop left.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`].
    pub fn open(
        shelf: S,
        ledger: L,
        workspace: WorkspaceId,
        clock: Arc<dyn Clock>,
        rng: Box<dyn RandomSource>,
        limits: Limits,
    ) -> Result<Self, ApiError> {
        ledger.create(&workspace)?;
        let engine = Self {
            shelf,
            ledger,
            workspace,
            clock,
            rng: Mutex::new(rng),
            limits,
        };
        // Whatever the last process left half done is put right before the
        // first request: see `Rules::reconcile`.
        engine.write(|rules| rules.reconcile())?;
        Ok(engine)
    }

    /// Opens a workspace and repairs nothing. Test support: the kill harness
    /// uses it to show that, without the repair of [`Engine::open`], a kill
    /// does leave the books and the shelf apart.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`].
    #[cfg(feature = "fault-injection")]
    pub fn open_without_repair(
        shelf: S,
        ledger: L,
        workspace: WorkspaceId,
        clock: Arc<dyn Clock>,
        rng: Box<dyn RandomSource>,
        limits: Limits,
    ) -> Result<Self, ApiError> {
        ledger.create(&workspace)?;
        Ok(Self {
            shelf,
            ledger,
            workspace,
            clock,
            rng: Mutex::new(rng),
            limits,
        })
    }

    /// The workspace's limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The shelf, for inspection.
    pub fn shelf(&self) -> &S {
        &self.shelf
    }

    /// The workspace's record as the ledger holds it, for inspection.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`].
    pub fn record(&self) -> Result<WorkspaceRecord, ApiError> {
        self.ledger.load(&self.workspace)
    }

    /// Runs `rule` in a transaction; its changes to the record are stored
    /// only when it succeeds.
    fn write<T>(
        &self,
        rule: impl FnOnce(&mut Rules<'_, S>) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        let mut rule = Some(rule);
        let mut after = Vec::new();
        let answer = self.ledger.transact(&self.workspace, &mut |rec| {
            let rule = rule.take().expect("a ledger runs its rule once");
            let mut rules = Rules {
                rec,
                shelf: &self.shelf,
                clock: self.clock.as_ref(),
                rng: &self.rng,
                limits: &self.limits,
                after: Vec::new(),
            };
            let answer = rule(&mut rules)?;
            after = std::mem::take(&mut rules.after);
            Ok(answer)
        })?;
        crate::fault::point("engine: after the commit");
        // The record is stored. What follows only removes what nothing
        // points to any more, so a failure here is the janitor's to finish.
        for cleanup in after {
            let done = match &cleanup {
                Cleanup::DropGeneration(generation) => self.shelf.drop_generation(*generation),
                Cleanup::RemoveStaging(upload) => self.shelf.stage_remove(upload),
            };
            crate::fault::point("engine: between clean-ups");
            if let Err(err) = done {
                tracing::warn!(workspace = %self.workspace.as_str(), ?cleanup, %err, "clean-up left for the janitor");
            }
        }
        Ok(answer)
    }

    /// Runs `rule` on a copy of the record and stores nothing.
    fn read<T>(&self, rule: impl FnOnce(&mut Rules<'_, S>) -> T) -> Result<T, ApiError> {
        let mut rec = self.ledger.load(&self.workspace)?;
        Ok(rule(&mut Rules {
            rec: &mut rec,
            shelf: &self.shelf,
            clock: self.clock.as_ref(),
            rng: &self.rng,
            limits: &self.limits,
            after: Vec::new(),
        }))
    }

    /// `getWorkspace`: the encryption part; [`Rules::encryption`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn encryption(&self) -> Result<EncryptionView, ApiError> {
        self.read(|rules| rules.encryption())?
    }

    /// `listItemIds`; [`Rules::item_ids`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn item_ids(&self, partition: Partition) -> Result<Vec<ItemId>, ApiError> {
        self.read(|rules| rules.item_ids(partition))?
    }

    /// `getItem` and `getItemContent`; [`Rules::item`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn item(&self, partition: Partition, id: &ItemId) -> Result<Option<StoredItem>, ApiError> {
        self.read(|rules| rules.item(partition, id))?
    }

    /// `listItemIds` with `after`; [`Rules::item_ids_after`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn item_ids_after(
        &self,
        partition: Partition,
        after: Option<&ItemId>,
    ) -> Result<Vec<ItemId>, ApiError> {
        self.read(|rules| rules.item_ids_after(partition, after))?
    }

    /// `listItems`; [`Rules::items_after`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn items_after(
        &self,
        partition: Partition,
        after: Option<&ItemId>,
    ) -> Result<Vec<(ItemId, StoredItem)>, ApiError> {
        self.read(|rules| rules.items_after(partition, after))?
    }

    /// `resolveItem`; [`Rules::resolve_item`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn resolve_item(&self, input: &str) -> Result<Resolved, ApiError> {
        self.read(|rules| rules.resolve_item(input))?
    }

    /// `getItemContent` from an offset, for `Range`; [`Rules::item_content_from`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn item_content_from(
        &self,
        partition: Partition,
        id: &ItemId,
        offset: u64,
    ) -> Result<Option<Content>, ApiError> {
        self.read(|rules| rules.item_content_from(partition, id, offset))?
    }

    /// `getItemContent`; [`Rules::item_content`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn item_content(
        &self,
        partition: Partition,
        id: &ItemId,
    ) -> Result<Option<Content>, ApiError> {
        self.read(|rules| rules.item_content(partition, id))?
    }

    /// `getItemContent` with `partition=staged`; [`Rules::staged_item_content`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn staged_item_content(&self, caller: &Caller, id: &ItemId) -> Result<Content, ApiError> {
        self.read(|rules| rules.staged_item_content(caller, id))?
    }

    /// `findByContentKey`; [`Rules::find_by_content_key`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn find_by_content_key(&self, content_key: &str) -> Result<Option<ItemId>, ApiError> {
        self.read(|rules| rules.find_by_content_key(content_key))?
    }

    /// [`Rules::used_bytes`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn used_bytes(&self) -> Result<u64, ApiError> {
        self.read(|rules| rules.used_bytes())
    }

    /// [`Rules::live_generations`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn live_generations(&self) -> Result<Vec<u64>, ApiError> {
        self.read(|rules| rules.live_generations())
    }

    /// [`Rules::reserved_bytes`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn reserved_bytes(&self) -> Result<u64, ApiError> {
        self.read(|rules| rules.reserved_bytes())
    }

    /// [`Rules::pending_uploads`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn pending_uploads(&self) -> Result<usize, ApiError> {
        self.read(|rules| rules.pending_uploads())
    }

    /// `getRewrite`; [`Rules::session_view`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn session_view(&self) -> Result<Option<SessionView>, ApiError> {
        self.read(|rules| rules.session_view())?
    }

    /// `listItemIds` with `partition=staged`; [`Rules::staged_item_ids`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn staged_item_ids(&self, caller: &Caller) -> Result<Vec<ItemId>, ApiError> {
        self.read(|rules| rules.staged_item_ids(caller))?
    }

    /// `getItem` with `partition=staged`; [`Rules::staged_item`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn staged_item(&self, caller: &Caller, id: &ItemId) -> Result<StoredItem, ApiError> {
        self.read(|rules| rules.staged_item(caller, id))?
    }

    /// `putUploadContent`; [`Rules::put_upload_content`]. It changes the shelf
    /// and not the record, so it runs outside any transaction: content may take
    /// long to arrive, and the workspace must not wait for it.
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn put_upload_content(
        &self,
        caller: &Caller,
        upload: &UploadId,
        content: &mut dyn Read,
    ) -> Result<(), ApiError> {
        self.read(|rules| rules.put_upload_content(caller, upload, content))?
    }

    /// `deleteItem`; [`Rules::delete_item`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn delete_item(
        &self,
        caller: &Caller,
        partition: Partition,
        id: &ItemId,
        expected: Option<&KeyId>,
    ) -> Result<StoredItem, ApiError> {
        self.write(|rules| rules.delete_item(caller, partition, id, expected))
    }

    /// `enableEncryption`; [`Rules::enable_encryption`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn enable_encryption(
        &self,
        caller: &Caller,
        key_id: KeyId,
        header: Vec<u8>,
    ) -> Result<EncryptionView, ApiError> {
        self.write(|rules| rules.enable_encryption(caller, key_id, header))
    }

    /// `freshStart`; [`Rules::fresh_start`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn fresh_start(
        &self,
        caller: &Caller,
        key_id: KeyId,
        header: Vec<u8>,
    ) -> Result<EncryptionView, ApiError> {
        self.write(|rules| rules.fresh_start(caller, key_id, header))
    }

    /// `replaceHeader`; [`Rules::replace_header`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn replace_header(
        &self,
        caller: &Caller,
        expected: &KeyId,
        header: Vec<u8>,
    ) -> Result<EncryptionView, ApiError> {
        self.write(|rules| rules.replace_header(caller, expected, header))
    }

    /// `beginUpload`; [`Rules::begin_upload`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn begin_upload(&self, caller: &Caller, request: UploadRequest) -> Result<Begun, ApiError> {
        self.write(|rules| rules.begin_upload(caller, request))
    }

    /// `commitUpload`; [`Rules::commit_upload`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn commit_upload(
        &self,
        caller: &Caller,
        upload: &UploadId,
    ) -> Result<PutOutcome, ApiError> {
        self.write(|rules| rules.commit_upload(caller, upload))
    }

    /// `abortUpload`; [`Rules::abort_upload`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn abort_upload(&self, caller: &Caller, upload: &UploadId) -> Result<(), ApiError> {
        self.write(|rules| rules.abort_upload(caller, upload))
    }

    /// `beginRewrite`; [`Rules::begin_rewrite`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn begin_rewrite(
        &self,
        caller: &Caller,
        request: RewriteRequest,
    ) -> Result<SessionView, ApiError> {
        self.write(|rules| rules.begin_rewrite(caller, request))
    }

    /// `heartbeatRewrite`; [`Rules::heartbeat_rewrite`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn heartbeat_rewrite(&self, caller: &Caller) -> Result<SessionView, ApiError> {
        self.write(|rules| rules.heartbeat_rewrite(caller))
    }

    /// `takeOverRewrite`; [`Rules::take_over_rewrite`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn take_over_rewrite(&self, caller: &Caller) -> Result<SessionView, ApiError> {
        self.write(|rules| rules.take_over_rewrite(caller))
    }

    /// `commitRewrite`; [`Rules::commit_rewrite`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn commit_rewrite(
        &self,
        caller: &Caller,
        new_key_id: &KeyId,
    ) -> Result<EncryptionView, ApiError> {
        self.write(|rules| rules.commit_rewrite(caller, new_key_id))
    }

    /// `abortRewrite`; [`Rules::abort_rewrite`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn abort_rewrite(
        &self,
        caller: &Caller,
        new_key_id: &KeyId,
    ) -> Result<EncryptionView, ApiError> {
        self.write(|rules| rules.abort_rewrite(caller, new_key_id))
    }

    /// `cleanStaging`, which the janitor also runs unasked;
    /// [`Rules::clean_staging`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn clean_staging(&self) -> Result<usize, ApiError> {
        self.write(|rules| rules.clean_staging())
    }

    /// [`Rules::upload_state`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn upload_state(
        &self,
        caller: &Caller,
        upload: &UploadId,
    ) -> Result<crate::upload::UploadState, ApiError> {
        self.read(|rules| rules.upload_state(caller, upload))?
    }

    /// [`Rules::staged_item_content_from`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn staged_item_content_from(
        &self,
        caller: &Caller,
        id: &ItemId,
        offset: u64,
    ) -> Result<Content, ApiError> {
        self.read(|rules| rules.staged_item_content_from(caller, id, offset))?
    }

    /// `probeWrite`; [`Rules::probe_write`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn probe_write(&self, caller: &Caller) -> Result<(), ApiError> {
        self.read(|rules| rules.probe_write(caller))?
    }

    /// `passalong-server rewrite abort`; [`Rules::operator_abort_rewrite`].
    ///
    /// # Errors
    ///
    /// As the rule, and [`ApiError::ServiceUnavailable`].
    pub fn operator_abort_rewrite(&self, force: bool) -> Result<EncryptionView, ApiError> {
        self.write(|rules| rules.operator_abort_rewrite(force))
    }

    #[cfg(test)]
    pub(crate) fn seed_for_tests(&self, id: &ItemId, content: &[u8], under: Option<KeyId>) {
        self.write(|rules| {
            let upload = rules.new_upload_id();
            rules.shelf.stage_create(&upload)?;
            let mut bytes = content;
            rules
                .shelf
                .stage_write(&upload, &mut bytes, content.len() as u64)?;
            let envelope = crate::shelf::Envelope {
                meta: b"{}".to_vec(),
                under,
                received_at: rules.clock.now(),
            };
            let generation = rules.rec.generation;
            assert!(rules.shelf.publish(&upload, generation, id, envelope)?);
            rules.add_used(generation, content.len() as u64);
            Ok(())
        })
        .unwrap();
    }
}
/// The oldest item of `generation` whose content key is `content_key`.
pub(crate) fn find_in<S: ItemShelf>(
    shelf: &S,
    generation: u64,
    content_key: &str,
) -> Result<Option<ItemId>, ApiError> {
    Ok(shelf
        .ids(generation)?
        .into_iter()
        .rev()
        .find(|id| id.content_key() == content_key))
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
        let engine = Engine::open(
            MemoryShelf::default(),
            MemoryLedger::default(),
            WorkspaceId::generate(&mut SeededRandom::new(1)),
            Arc::new(clock.clone()),
            Box::new(SeededRandom::new(42)),
            Limits {
                // These tests are about other rules and use made-up ids; the
                // check has tests of its own in `upload.rs`.
                check_plaintext_content: false,
                ..Limits::default()
            },
        )
        .unwrap();
        (engine, clock)
    }

    /// An item's content, read to its end.
    pub(crate) fn content(
        engine: &Engine<MemoryShelf>,
        partition: Partition,
        id: &ItemId,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut stream = engine.item_content(partition, id).unwrap().unwrap();
        std::io::Read::read_to_end(&mut stream, &mut bytes).unwrap();
        bytes
    }

    /// Publishes an item straight onto the shelf, as a finished upload would.
    pub(crate) fn seed(engine: &mut Engine<MemoryShelf>, id: &str, content: &[u8]) {
        let id = ItemId::parse(id).unwrap();
        let under = engine.encryption().unwrap().key_id;
        engine.seed_for_tests(&id, content, under);
    }

    #[test]
    fn a_new_workspace_is_plaintext_and_empty() {
        let (engine, _) = engine();
        let view = engine.encryption().unwrap();
        assert_eq!(view.state, EncryptionState::Plaintext);
        assert!(view.key_id.is_none() && view.header.is_none() && view.rewrite.is_none());
        assert!(engine.item_ids(Partition::Current).unwrap().is_empty());
        assert_eq!(engine.used_bytes().unwrap(), 0);
    }

    #[test]
    fn enabling_encryption_needs_an_empty_plaintext_workspace_and_a_writer() {
        let (engine, _) = engine();
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
        assert_eq!(engine.encryption().unwrap(), view);
    }

    #[test]
    fn enabling_encryption_is_refused_while_the_workspace_holds_items() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"text");
        let err = engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap_err();
        assert_eq!(err.code(), "INVALID_REQUEST");
        assert_eq!(
            engine.encryption().unwrap().state,
            EncryptionState::Plaintext
        );
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
        assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 1);
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
        assert!(engine.item_ids(Partition::Current).unwrap().is_empty());
        assert_eq!(engine.item_ids(Partition::Plain).unwrap().len(), 1);
        let kept = engine
            .item(
                Partition::Plain,
                &ItemId::parse("00000001-aaaaaaaaaaaa").unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(kept.size, 10);
        let id = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();
        assert_eq!(content(&engine, Partition::Plain, &id), b"plain text");
        assert_eq!(engine.used_bytes().unwrap(), 10);

        // Sent again it answers the same state and sets nothing else aside.
        seed(&mut engine, "00000002-bbbbbbbbbbbb", b"sealed");
        assert_eq!(
            engine
                .fresh_start(&rw("a"), key("aa"), b"h".to_vec())
                .unwrap(),
            view
        );
        assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 1);
        assert_eq!(
            engine
                .fresh_start(&rw("a"), key("bb"), b"h".to_vec())
                .unwrap_err(),
            ApiError::KeyIdMismatch
        );
    }

    #[test]
    fn a_fresh_start_of_an_empty_workspace_leaves_no_plain_partition() {
        let (engine, _) = engine();
        engine
            .fresh_start(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        assert!(engine.item_ids(Partition::Plain).unwrap().is_empty());
        assert_eq!(engine.live_generations().unwrap().len(), 1);
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
        assert_eq!(gone.size, 5);
        assert_eq!(engine.used_bytes().unwrap(), 0);
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
    fn what_a_user_typed_resolves_to_one_item_by_the_clients_rules() {
        let (mut engine, _) = engine();
        seed(&mut engine, "6aa52107-2cf24dba5fb0", b"a");
        seed(&mut engine, "6aa52108-2cf2ffffffff", b"b");
        seed(&mut engine, "6aa52109-9999aaaabbbb", b"c");
        let id = |text: &str| ItemId::parse(text).unwrap();
        let resolve = |input: &str| engine.resolve_item(input).unwrap();

        // Without a dash: a prefix of the content key.
        assert_eq!(resolve("9999"), Resolved::Id(id("6aa52109-9999aaaabbbb")));
        assert_eq!(resolve("2cf24"), Resolved::Id(id("6aa52107-2cf24dba5fb0")));
        assert_eq!(
            resolve("2cf2"),
            Resolved::Ambiguous(vec![
                id("6aa52108-2cf2ffffffff"),
                id("6aa52107-2cf24dba5fb0")
            ])
        );
        // With a dash: a prefix of the whole id.
        assert_eq!(
            resolve("6aa52108-"),
            Resolved::Id(id("6aa52108-2cf2ffffffff"))
        );
        assert_eq!(
            resolve("6aa5210"),
            Resolved::NotFound,
            "no dash, so a content key, and none starts so"
        );
        // Case and surrounding spaces are ignored.
        assert_eq!(
            resolve("  9999AAAA "),
            Resolved::Id(id("6aa52109-9999aaaabbbb"))
        );
        assert_eq!(resolve("ffff"), Resolved::NotFound);
        for bad in ["", "999", "zzzz", "99 99", "9999-9999-", "../9999"] {
            assert_eq!(resolve(bad), Resolved::InvalidPrefix, "{bad:?}");
        }
    }

    #[test]
    fn items_newer_than_one_are_listed_newest_first() {
        let (mut engine, _) = engine();
        for text in [
            "00000001-aaaaaaaaaaaa",
            "00000002-bbbbbbbbbbbb",
            "00000003-cccccccccccc",
        ] {
            seed(&mut engine, text, b"x");
        }
        let after = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();
        let newer: Vec<String> = engine
            .item_ids_after(Partition::Current, Some(&after))
            .unwrap()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(newer, ["00000003-cccccccccccc", "00000002-bbbbbbbbbbbb"]);
        assert_eq!(
            engine
                .item_ids_after(Partition::Current, None)
                .unwrap()
                .len(),
            3
        );
        let newest = ItemId::parse("00000003-cccccccccccc").unwrap();
        assert!(
            engine
                .item_ids_after(Partition::Current, Some(&newest))
                .unwrap()
                .is_empty()
        );
        let items = engine
            .items_after(Partition::Current, Some(&after))
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0.as_str(), "00000003-cccccccccccc");
        assert_eq!(items[0].1.size, 1);
    }

    #[test]
    fn the_content_key_finds_the_oldest_item_with_that_content() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000002-aaaaaaaaaaaa", b"x");
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"x");
        seed(&mut engine, "00000003-bbbbbbbbbbbb", b"y");
        let found = engine.find_by_content_key("aaaaaaaaaaaa").unwrap().unwrap();
        assert_eq!(found.as_str(), "00000001-aaaaaaaaaaaa");
        assert!(
            engine
                .find_by_content_key("cccccccccccc")
                .unwrap()
                .is_none()
        );
    }
}
