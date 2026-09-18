//! The rewrite session: re-encrypting every item, for `encrypt` migration
//! and `encrypt --rotate`.

use crate::error::ApiError;
use crate::ids::{ApiKeyId, ItemId, KeyId};
use crate::shelf::{Content, ItemShelf, StoredItem};
use crate::workspace::{Caller, Cleanup, EncryptionView, Rules, Seal, SessionView, State};

/// What a rewrite does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteKind {
    /// A plaintext workspace's items are encrypted.
    Migrate,
    /// An encrypted workspace's items move to a new data key.
    Rotate,
}

/// An open rewrite session, as the control database records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) kind: RewriteKind,
    /// The seal before the rewrite, which readers still go by and which an
    /// abort restores; `None` for a migration.
    pub(crate) prior: Option<Seal>,
    pub(crate) next: Seal,
    pub(crate) holder: ApiKeyId,
    pub(crate) lease_expires_at: u64,
    pub(crate) staged_generation: u64,
}

/// `beginRewrite`'s request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteRequest {
    /// Migration or rotation.
    pub kind: RewriteKind,
    /// The current data key's id; `None` for the plaintext workspace a
    /// migration starts from.
    pub expected_key_id: Option<KeyId>,
    /// The id of the key the items are re-sealed under.
    pub new_key_id: KeyId,
    /// That key, wrapped: the header the workspace gets on commit.
    pub new_header: Vec<u8>,
}

impl<S: ItemShelf> Rules<'_, S> {
    /// The open session, for its holder. While a lease has ended but nobody
    /// took over, the holder is still the holder: taking over is the only
    /// thing that ends its hold, so there are never two.
    fn held_session(&self, caller: &Caller) -> Result<&Session, ApiError> {
        match &self.rec.state {
            State::Settled(_) => Err(ApiError::NotFound),
            State::Rewriting(session) if session.holder != caller.key => Err(ApiError::LeaseHeld),
            State::Rewriting(session) => Ok(session),
        }
    }

    fn renew_lease(&mut self, holder: &ApiKeyId) {
        let lease_expires_at = self.clock.now() + self.limits.lease_secs;
        if let State::Rewriting(session) = &mut self.rec.state {
            session.holder = holder.clone();
            session.lease_expires_at = lease_expires_at;
        }
    }

    /// The generation an upload of the open rewrite goes to.
    pub(crate) fn rewrite_target(
        &self,
        caller: &Caller,
        expected: Option<&KeyId>,
    ) -> Result<u64, ApiError> {
        let session = self.held_session(caller)?;
        if expected != Some(&session.next.key_id) {
            return Err(ApiError::KeyIdMismatch);
        }
        Ok(session.staged_generation)
    }

    /// `listItemIds` with `partition=staged`: the open rewrite's next
    /// generation, newest first, for its holder alone.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] without a session, [`ApiError::LeaseHeld`] for
    /// anyone but the holder.
    pub fn staged_item_ids(&self, caller: &Caller) -> Result<Vec<ItemId>, ApiError> {
        let session = self.held_session(caller)?;
        self.shelf.ids(session.staged_generation)
    }

    /// `getItem` and `getItemContent` with `partition=staged`. The client
    /// reads every re-encrypted item back and compares its SHA-256 and size
    /// before it commits, which catches a store that acknowledged a write
    /// and lost or damaged it. Only the holder reads here: the generation
    /// may yet be dropped, and nobody else can open what is in it.
    ///
    /// # Errors
    ///
    /// As [`Engine::staged_item_ids`], and [`ApiError::NotFound`] for an id
    /// that is not staged.
    pub fn staged_item(&self, caller: &Caller, id: &ItemId) -> Result<StoredItem, ApiError> {
        let session = self.held_session(caller)?;
        self.shelf
            .get(session.staged_generation, id)?
            .ok_or(ApiError::NotFound)
    }

    /// `getItemContent` with `partition=staged`: what the holder reads back
    /// and compares before it commits.
    ///
    /// # Errors
    ///
    /// As [`Rules::staged_item`].
    pub fn staged_item_content(&self, caller: &Caller, id: &ItemId) -> Result<Content, ApiError> {
        let session = self.held_session(caller)?;
        self.shelf
            .open_content(session.staged_generation, id)?
            .ok_or(ApiError::NotFound)
    }

    /// `getRewrite`.
    pub fn session_view(&self) -> Result<Option<SessionView>, ApiError> {
        let State::Rewriting(session) = &self.rec.state else {
            return Ok(None);
        };
        Ok(Some(SessionView {
            kind: session.kind,
            holder: session.holder.clone(),
            lease_expires_at: session.lease_expires_at,
            new_key_id: session.next.key_id.clone(),
            staged_ids: self.shelf.ids(session.staged_generation)?,
            source_items: self.shelf.ids(self.rec.generation)?.len(),
        }))
    }

    /// `beginRewrite`: takes the lease and opens an empty next generation.
    /// From here ordinary writers get [`ApiError::RewriteInProgress`] and
    /// readers keep reading the current generation. Sent again by the
    /// holder with the same kind and new key id, it answers the live
    /// session and renews the lease.
    ///
    /// # Errors
    ///
    /// [`ApiError::ForbiddenRole`]; [`ApiError::RewriteInProgress`] while
    /// another session is open; [`ApiError::InvalidRequest`] for a kind
    /// that does not fit the workspace's state, or a new key id equal to
    /// the current one; [`ApiError::KeyIdMismatch`]; and
    /// [`ApiError::RewriteEnded`] for the new key id of a rewrite that was
    /// aborted, which marks the request as a duplicate of an old one. A new
    /// attempt comes with a new data key, as the client makes one each time.
    pub fn begin_rewrite(
        &mut self,
        caller: &Caller,
        request: RewriteRequest,
    ) -> Result<SessionView, ApiError> {
        caller.must_write()?;
        let prior = match &self.rec.state {
            State::Rewriting(session) => {
                let replay = session.holder == caller.key
                    && session.kind == request.kind
                    && session.next.key_id == request.new_key_id;
                if !replay {
                    return Err(ApiError::RewriteInProgress);
                }
                self.renew_lease(&caller.key);
                return self.session_view()?.ok_or(ApiError::NotFound);
            }
            State::Settled(seal) => seal.clone(),
        };
        match (request.kind, &prior) {
            (RewriteKind::Migrate, None) | (RewriteKind::Rotate, Some(_)) => {}
            (RewriteKind::Migrate, Some(_)) => {
                return Err(ApiError::InvalidRequest(
                    "the workspace is encrypted already; rotate its key instead".to_owned(),
                ));
            }
            (RewriteKind::Rotate, None) => {
                return Err(ApiError::InvalidRequest(
                    "the workspace is not encrypted; migrate it instead".to_owned(),
                ));
            }
        }
        if self.rec.ended_rewrites.contains(&request.new_key_id) {
            return Err(ApiError::RewriteEnded);
        }
        let current = prior.as_ref().map(|seal| &seal.key_id);
        if current != request.expected_key_id.as_ref() {
            return Err(ApiError::KeyIdMismatch);
        }
        if current == Some(&request.new_key_id) {
            return Err(ApiError::InvalidRequest(
                "the new key is the current one".to_owned(),
            ));
        }
        let staged_generation = self.rec.next_generation;
        self.rec.next_generation += 1;
        self.rec.state = State::Rewriting(Session {
            kind: request.kind,
            prior,
            next: Seal {
                key_id: request.new_key_id,
                header: request.new_header,
            },
            holder: caller.key.clone(),
            lease_expires_at: self.clock.now() + self.limits.lease_secs,
            staged_generation,
        });
        self.session_view()?.ok_or(ApiError::NotFound)
    }

    /// `heartbeatRewrite`: the holder renews its lease.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] without a session, [`ApiError::LeaseHeld`]
    /// for anyone but the holder.
    pub fn heartbeat_rewrite(&mut self, caller: &Caller) -> Result<SessionView, ApiError> {
        caller.must_write()?;
        self.held_session(caller)?;
        self.renew_lease(&caller.key);
        self.session_view()?.ok_or(ApiError::NotFound)
    }

    /// `takeOverRewrite`: once the lease has ended, another key becomes the
    /// holder, to resume the rewrite or abort it; this is what
    /// `passalong encrypt --recover` does from a second device. The former
    /// holder's requests are refused from then on. For the holder itself
    /// it is a heartbeat, so it may be repeated.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] without a session, [`ApiError::LeaseHeld`]
    /// while the lease runs.
    pub fn take_over_rewrite(&mut self, caller: &Caller) -> Result<SessionView, ApiError> {
        caller.must_write()?;
        let State::Rewriting(session) = &self.rec.state else {
            return Err(ApiError::NotFound);
        };
        if session.holder != caller.key && session.lease_expires_at > self.clock.now() {
            return Err(ApiError::LeaseHeld);
        }
        self.renew_lease(&caller.key);
        self.session_view()?.ok_or(ApiError::NotFound)
    }

    /// `commitRewrite`: the workspace's generation, header, and key id
    /// change together, in what the server does as one control database
    /// transaction. The old generation is rubbish from then on; removing
    /// it may be interrupted, and the janitor finishes the job.
    ///
    /// Sent again after it succeeded, by anyone who names the new key id,
    /// it answers the state it produced. This is the replay that matters
    /// most: a client that lost the answer learns that the commit
    /// happened, instead of concluding that it must start again.
    ///
    /// # Errors
    ///
    /// [`ApiError::RewriteIncomplete`] unless the staged generation holds
    /// as many items as the source; [`ApiError::LeaseHeld`];
    /// [`ApiError::KeyIdMismatch`]; [`ApiError::NotFound`] when there is no
    /// session and the workspace is not under `new_key_id`.
    pub fn commit_rewrite(
        &mut self,
        caller: &Caller,
        new_key_id: &KeyId,
    ) -> Result<EncryptionView, ApiError> {
        caller.must_write()?;
        if let State::Settled(seal) = &self.rec.state {
            let done = seal.as_ref().is_some_and(|seal| &seal.key_id == new_key_id);
            return if done {
                self.encryption()
            } else {
                Err(ApiError::NotFound)
            };
        }
        let session = self.held_session(caller)?.clone();
        if &session.next.key_id != new_key_id {
            return Err(ApiError::KeyIdMismatch);
        }
        let staged = self.shelf.ids(session.staged_generation)?.len();
        let source = self.shelf.ids(self.rec.generation)?.len();
        if staged != source {
            return Err(ApiError::RewriteIncomplete { staged, source });
        }
        let old = self.rec.generation;
        self.rec.generation = session.staged_generation;
        self.rec.state = State::Settled(Some(session.next));
        self.rec.used.remove(&old);
        // Not before the record points elsewhere: see `Cleanup`.
        self.after.push(Cleanup::DropGeneration(old));
        self.drop_rewrite_uploads();
        self.encryption()
    }

    /// `abortRewrite`: the workspace is again what it was before
    /// `beginRewrite`, and the staged generation is rubbish. With no
    /// session open it answers the current state, so it may be repeated,
    /// and a client whose rewrite was committed by someone else sees so.
    ///
    /// # Errors
    ///
    /// [`ApiError::LeaseHeld`]; [`ApiError::KeyIdMismatch`] when the open
    /// session is another one than the caller means.
    pub fn abort_rewrite(
        &mut self,
        caller: &Caller,
        new_key_id: &KeyId,
    ) -> Result<EncryptionView, ApiError> {
        caller.must_write()?;
        if matches!(self.rec.state, State::Settled(_)) {
            return self.encryption();
        }
        let session = self.held_session(caller)?.clone();
        if &session.next.key_id != new_key_id {
            return Err(ApiError::KeyIdMismatch);
        }
        self.rec.ended_rewrites.insert(session.next.key_id.clone());
        self.rec.state = State::Settled(session.prior);
        self.rec.used.remove(&session.staged_generation);
        self.after
            .push(Cleanup::DropGeneration(session.staged_generation));
        self.drop_rewrite_uploads();
        self.encryption()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ApiError;
    use crate::ids::ItemId;
    use crate::shelf::MemoryShelf;
    use crate::upload::tests::request;
    use crate::upload::{Begun, PutOutcome};
    use crate::workspace::Engine;
    use crate::workspace::tests::{engine, key, ro, rw, seed};
    use crate::workspace::{EncryptionState, Limits, Partition};

    fn staged_content(engine: &Engine<MemoryShelf>, who: &str, id: &ItemId) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut stream = engine.staged_item_content(&rw(who), id).unwrap();
        std::io::Read::read_to_end(&mut stream, &mut bytes).unwrap();
        bytes
    }

    fn migrate(new: &str) -> RewriteRequest {
        RewriteRequest {
            kind: RewriteKind::Migrate,
            expected_key_id: None,
            new_key_id: key(new),
            new_header: format!("header-{new}").into_bytes(),
        }
    }

    fn rotate(old: &str, new: &str) -> RewriteRequest {
        RewriteRequest {
            kind: RewriteKind::Rotate,
            expected_key_id: Some(key(old)),
            ..migrate(new)
        }
    }

    /// Stages one item of the open rewrite, as its holder.
    fn stage(
        engine: &mut Engine<MemoryShelf>,
        who: &str,
        id: &str,
        new: &str,
        content: &[u8],
    ) -> PutOutcome {
        let mut req = request(id, content.len() as u64);
        req.expected_key_id = Some(key(new));
        req.in_rewrite = true;
        let upload = match engine.begin_upload(&rw(who), req).unwrap() {
            Begun::Ticket(ticket) => ticket.upload_id,
            Begun::Stored(outcome) => return outcome,
        };
        engine
            .put_upload_content(
                &rw(who),
                &upload,
                &mut std::io::Cursor::new(content.to_vec()),
            )
            .unwrap();
        engine.commit_upload(&rw(who), &upload).unwrap()
    }

    #[test]
    fn a_migration_begins_only_on_plaintext_and_a_rotation_only_under_the_current_key() {
        let (mut engine, _) = engine();
        assert_eq!(
            engine.begin_rewrite(&ro("k"), migrate("bb")).unwrap_err(),
            ApiError::ForbiddenRole
        );
        assert_eq!(
            engine
                .begin_rewrite(&rw("a"), rotate("aa", "bb"))
                .unwrap_err()
                .code(),
            "INVALID_REQUEST"
        );
        let mut wrong = migrate("bb");
        wrong.expected_key_id = Some(key("aa"));
        assert_eq!(
            engine.begin_rewrite(&rw("a"), wrong).unwrap_err(),
            ApiError::KeyIdMismatch
        );

        engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        assert_eq!(
            engine
                .begin_rewrite(&rw("a"), migrate("bb"))
                .unwrap_err()
                .code(),
            "INVALID_REQUEST"
        );
        assert_eq!(
            engine
                .begin_rewrite(&rw("a"), rotate("cc", "bb"))
                .unwrap_err(),
            ApiError::KeyIdMismatch
        );
        assert_eq!(
            engine
                .begin_rewrite(&rw("a"), rotate("aa", "aa"))
                .unwrap_err()
                .code(),
            "INVALID_REQUEST"
        );

        let view = engine.begin_rewrite(&rw("a"), rotate("aa", "bb")).unwrap();
        assert_eq!(view.kind, RewriteKind::Rotate);
        assert_eq!(view.holder.as_str(), "a");
        assert_eq!(view.lease_expires_at, 1_000 + Limits::default().lease_secs);
        assert_eq!(view.new_key_id, key("bb"));
        assert!(view.staged_ids.is_empty());
    }

    #[test]
    fn while_a_rewrite_is_open_readers_read_and_writers_wait() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"plain");
        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();

        let view = engine.encryption().unwrap();
        assert_eq!(view.state, EncryptionState::Rewriting);
        assert_eq!(view.key_id, None);
        assert_eq!(view.rewrite.unwrap().source_items, 1);
        assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 1);

        let id = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();
        let busy = ApiError::RewriteInProgress;
        assert_eq!(
            engine
                .begin_upload(&rw("b"), request("00000002-bbbbbbbbbbbb", 1))
                .unwrap_err(),
            busy
        );
        assert_eq!(
            engine
                .delete_item(&rw("b"), Partition::Current, &id, None)
                .unwrap_err(),
            busy
        );
        assert_eq!(
            engine
                .enable_encryption(&rw("b"), key("cc"), vec![])
                .unwrap_err(),
            busy
        );
        assert_eq!(
            engine.fresh_start(&rw("b"), key("cc"), vec![]).unwrap_err(),
            busy
        );
        assert_eq!(
            engine
                .replace_header(&rw("b"), &key("cc"), vec![])
                .unwrap_err(),
            busy
        );
        assert_eq!(
            engine.begin_rewrite(&rw("b"), migrate("bb")).unwrap_err(),
            busy
        );
        assert_eq!(
            engine.begin_rewrite(&rw("a"), migrate("cc")).unwrap_err(),
            busy
        );
    }

    #[test]
    fn beginning_again_answers_the_live_session() {
        let (mut engine, clock) = engine();
        let first = engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        clock.advance(10);
        let again = engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        assert_eq!(again.lease_expires_at, first.lease_expires_at + 10);
        assert_eq!(engine.live_generations().unwrap().len(), 2);
    }

    #[test]
    fn only_the_holder_stages_and_only_under_the_new_key() {
        let (mut engine, _) = engine();
        let mut req = request("00000001-cccccccccccc", 1);
        req.in_rewrite = true;
        req.expected_key_id = Some(key("bb"));
        assert_eq!(
            engine.begin_upload(&rw("a"), req.clone()).unwrap_err(),
            ApiError::NotFound
        );

        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        assert_eq!(
            engine.begin_upload(&rw("b"), req.clone()).unwrap_err(),
            ApiError::LeaseHeld
        );
        let mut old = req.clone();
        old.expected_key_id = None;
        assert_eq!(
            engine.begin_upload(&rw("a"), old).unwrap_err(),
            ApiError::KeyIdMismatch
        );
        assert!(matches!(
            engine.begin_upload(&rw("a"), req).unwrap(),
            Begun::Ticket(_)
        ));
    }

    #[test]
    fn a_rewrite_may_stage_as_much_as_the_quota_and_no_more() {
        // Staged items do not count against the workspace's own use, or a
        // workspace more than half full could never rotate its key. They
        // have an allowance of their own, the quota again, so that a
        // session cannot be used to fill the disk.
        let (mut engine, _) = engine();
        engine.limits.quota_bytes = 10;
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"12345678");
        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        stage(&mut engine, "a", "00000001-cccccccccccc", "bb", b"12345678");
        assert_eq!(engine.used_bytes().unwrap(), 16);

        let mut req = request("00000002-dddddddddddd", 2);
        req.expected_key_id = Some(key("bb"));
        req.in_rewrite = true;
        let mut too_much = req.clone();
        too_much.id = ItemId::parse("00000003-eeeeeeeeeeee").unwrap();
        too_much.size = 3;
        assert_eq!(
            engine.begin_upload(&rw("a"), too_much).unwrap_err(),
            ApiError::QuotaExceeded
        );
        assert!(matches!(
            engine.begin_upload(&rw("a"), req).unwrap(),
            Begun::Ticket(_)
        ));
        // The reservation counts: nothing more fits beside it.
        let mut more = request("00000004-ffffffffffff", 1);
        more.expected_key_id = Some(key("bb"));
        more.in_rewrite = true;
        assert_eq!(
            engine.begin_upload(&rw("a"), more).unwrap_err(),
            ApiError::QuotaExceeded
        );
    }

    #[test]
    fn a_refused_operation_leaves_the_record_as_it_was() {
        let (mut engine, _) = engine();
        engine.limits.quota_bytes = 10;
        engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"12345678");
        let mut sealed = request("00000002-bbbbbbbbbbbb", 1);
        sealed.expected_key_id = Some(key("aa"));
        engine.begin_upload(&rw("a"), sealed.clone()).unwrap();
        let before = engine.record().unwrap();
        let id = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();
        let unknown = crate::ids::UploadId::generate(&mut crate::random::SeededRandom::new(3));

        let mut too_big = sealed.clone();
        too_big.id = ItemId::parse("00000003-cccccccccccc").unwrap();
        too_big.size = 5;
        assert!(engine.begin_upload(&rw("a"), too_big).is_err());
        assert!(engine.begin_upload(&ro("k"), sealed).is_err());
        assert!(engine.commit_upload(&rw("a"), &unknown).is_err());
        assert!(
            engine
                .delete_item(&rw("a"), Partition::Current, &id, None)
                .is_err()
        );
        assert!(
            engine
                .enable_encryption(&rw("a"), key("bb"), vec![])
                .is_err()
        );
        assert!(engine.fresh_start(&rw("a"), key("bb"), vec![]).is_err());
        assert!(engine.replace_header(&rw("a"), &key("bb"), vec![]).is_err());
        assert!(engine.begin_rewrite(&rw("a"), migrate("bb")).is_err());
        assert!(engine.begin_rewrite(&rw("a"), rotate("cc", "bb")).is_err());
        assert!(engine.heartbeat_rewrite(&rw("a")).is_err());
        assert!(engine.take_over_rewrite(&rw("a")).is_err());
        assert!(engine.commit_rewrite(&rw("a"), &key("bb")).is_err());
        assert_eq!(engine.record().unwrap(), before);

        // Within a session, too.
        engine.begin_rewrite(&rw("a"), rotate("aa", "bb")).unwrap();
        let before = engine.record().unwrap();
        assert!(engine.commit_rewrite(&rw("a"), &key("bb")).is_err());
        assert!(engine.commit_rewrite(&rw("b"), &key("bb")).is_err());
        assert!(engine.abort_rewrite(&rw("a"), &key("cc")).is_err());
        assert!(engine.take_over_rewrite(&rw("b")).is_err());
        assert_eq!(engine.record().unwrap(), before);
    }

    #[test]
    fn only_the_holder_reads_staged_items_back() {
        // The client verifies every re-encrypted item before it commits, by
        // reading it back and comparing SHA-256 and size. Nobody else has
        // any business in a generation that may yet be dropped.
        let (mut engine, clock) = engine();
        assert_eq!(
            engine.staged_item_ids(&rw("a")).unwrap_err(),
            ApiError::NotFound
        );
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"plain");
        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        let staged = stage(&mut engine, "a", "00000001-cccccccccccc", "bb", b"sealed").id;

        assert_eq!(
            engine.staged_item_ids(&rw("a")).unwrap(),
            vec![staged.clone()]
        );
        let item = engine.staged_item(&rw("a"), &staged).unwrap();
        assert_eq!(item.size, 6);
        assert_eq!(staged_content(&engine, "a", &staged), b"sealed");
        assert_eq!(item.envelope.under, Some(key("bb")));
        // Staged items are not the workspace's items yet.
        assert!(engine.item(Partition::Current, &staged).unwrap().is_none());

        let source = ItemId::parse("00000001-aaaaaaaaaaaa").unwrap();
        assert_eq!(
            engine.staged_item(&rw("a"), &source).unwrap_err(),
            ApiError::NotFound
        );
        assert_eq!(
            engine.staged_item(&rw("b"), &staged).unwrap_err(),
            ApiError::LeaseHeld
        );
        assert_eq!(
            engine.staged_item_ids(&ro("k")).unwrap_err(),
            ApiError::LeaseHeld
        );

        // After a take-over the new holder reads, and the former one does not.
        clock.advance(Limits::default().lease_secs + 1);
        engine.take_over_rewrite(&rw("b")).unwrap();
        assert_eq!(staged_content(&engine, "b", &staged), b"sealed");
        assert_eq!(
            engine.staged_item(&rw("a"), &staged).unwrap_err(),
            ApiError::LeaseHeld
        );

        engine.abort_rewrite(&rw("b"), &key("bb")).unwrap();
        assert_eq!(
            engine.staged_item(&rw("b"), &staged).unwrap_err(),
            ApiError::NotFound
        );
    }

    #[test]
    fn a_migration_commits_once_every_item_is_staged() {
        let (mut engine, _) = engine();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"one");
        seed(&mut engine, "00000002-bbbbbbbbbbbb", b"two!");
        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();

        assert!(
            stage(
                &mut engine,
                "a",
                "00000001-cccccccccccc",
                "bb",
                b"sealed-one"
            )
            .created
        );
        // Staging the same item again, as a resumed run might, adds nothing.
        assert!(
            !stage(
                &mut engine,
                "a",
                "00000001-cccccccccccc",
                "bb",
                b"sealed-one"
            )
            .created
        );
        assert_eq!(engine.session_view().unwrap().unwrap().staged_ids.len(), 1);
        assert_eq!(
            engine.commit_rewrite(&rw("a"), &key("bb")).unwrap_err(),
            ApiError::RewriteIncomplete {
                staged: 1,
                source: 2
            }
        );
        stage(
            &mut engine,
            "a",
            "00000002-dddddddddddd",
            "bb",
            b"sealed-two!",
        );
        assert_eq!(
            engine.commit_rewrite(&rw("b"), &key("bb")).unwrap_err(),
            ApiError::LeaseHeld
        );

        let view = engine.commit_rewrite(&rw("a"), &key("bb")).unwrap();
        assert_eq!(view.state, EncryptionState::Sealed);
        assert_eq!(view.key_id, Some(key("bb")));
        assert_eq!(view.header.as_deref(), Some(&b"header-bb"[..]));
        assert!(view.rewrite.is_none());
        let ids = engine.item_ids(Partition::Current).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| {
            engine
                .item(Partition::Current, id)
                .unwrap()
                .unwrap()
                .envelope
                .under
                == Some(key("bb"))
        }));
        assert_eq!(engine.used_bytes().unwrap(), 21);
        assert_eq!(engine.shelf().generations().unwrap().len(), 1);

        // A commit whose answer was lost is asked again, by anyone who
        // knows the new key id, and learns that it happened.
        assert_eq!(engine.commit_rewrite(&rw("a"), &key("bb")).unwrap(), view);
        assert_eq!(
            engine.commit_rewrite(&rw("a"), &key("cc")).unwrap_err(),
            ApiError::NotFound
        );
        // Writers under the new key are welcome again.
        let mut req = request("00000003-eeeeeeeeeeee", 1);
        req.expected_key_id = Some(key("bb"));
        assert!(matches!(
            engine.begin_upload(&rw("b"), req).unwrap(),
            Begun::Ticket(_)
        ));
    }

    #[test]
    fn an_abort_restores_what_was_there_and_may_be_repeated() {
        let (mut engine, _) = engine();
        engine
            .enable_encryption(&rw("a"), key("aa"), b"h".to_vec())
            .unwrap();
        seed(&mut engine, "00000001-aaaaaaaaaaaa", b"sealed-aa");
        engine.begin_rewrite(&rw("a"), rotate("aa", "bb")).unwrap();
        stage(
            &mut engine,
            "a",
            "00000001-cccccccccccc",
            "bb",
            b"sealed-bb",
        );
        // An upload of the rewrite that never finished goes with it.
        let mut req = request("00000002-dddddddddddd", 3);
        req.expected_key_id = Some(key("bb"));
        req.in_rewrite = true;
        engine.begin_upload(&rw("a"), req).unwrap();

        assert_eq!(
            engine.abort_rewrite(&rw("b"), &key("bb")).unwrap_err(),
            ApiError::LeaseHeld
        );
        assert_eq!(
            engine.abort_rewrite(&rw("a"), &key("cc")).unwrap_err(),
            ApiError::KeyIdMismatch
        );
        let view = engine.abort_rewrite(&rw("a"), &key("bb")).unwrap();
        assert_eq!(view.state, EncryptionState::Sealed);
        assert_eq!(view.key_id, Some(key("aa")));
        assert_eq!(engine.abort_rewrite(&rw("a"), &key("bb")).unwrap(), view);

        assert_eq!(engine.item_ids(Partition::Current).unwrap().len(), 1);
        assert_eq!(engine.used_bytes().unwrap(), 9);
        assert_eq!(engine.shelf().generations().unwrap().len(), 1);
        assert!(engine.shelf().staged().unwrap().is_empty());
    }

    #[test]
    fn a_begin_that_names_an_aborted_rewrites_key_is_refused_for_good() {
        // A duplicate of `beginRewrite` that arrives after its rewrite was
        // aborted looks like a new request. Were it accepted, a session
        // nobody holds would shut every writer out until someone recovered
        // it. The client makes a fresh data key for every attempt, so the
        // new key id tells the duplicate from a new attempt.
        let (mut engine, clock) = engine();
        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        engine.abort_rewrite(&rw("a"), &key("bb")).unwrap();
        assert_eq!(
            engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap_err(),
            ApiError::RewriteEnded
        );
        assert_eq!(
            engine.begin_rewrite(&rw("b"), migrate("bb")).unwrap_err(),
            ApiError::RewriteEnded
        );
        clock.advance(10 * Limits::default().staging_secs);
        engine.clean_staging().unwrap();
        assert_eq!(
            engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap_err(),
            ApiError::RewriteEnded
        );
        assert_eq!(
            engine.encryption().unwrap().state,
            EncryptionState::Plaintext
        );
        // A new attempt, under a new key, is welcome.
        engine.begin_rewrite(&rw("a"), migrate("cc")).unwrap();
    }

    #[test]
    fn the_lease_is_renewed_by_the_holder_and_taken_over_only_once_it_ended() {
        let (mut engine, clock) = engine();
        let lease = Limits::default().lease_secs;
        assert_eq!(
            engine.heartbeat_rewrite(&rw("a")).unwrap_err(),
            ApiError::NotFound
        );
        assert_eq!(
            engine.take_over_rewrite(&rw("b")).unwrap_err(),
            ApiError::NotFound
        );

        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        clock.advance(lease - 1);
        assert_eq!(
            engine.heartbeat_rewrite(&rw("b")).unwrap_err(),
            ApiError::LeaseHeld
        );
        assert_eq!(
            engine.take_over_rewrite(&rw("b")).unwrap_err(),
            ApiError::LeaseHeld
        );
        assert_eq!(
            engine.take_over_rewrite(&ro("k")).unwrap_err(),
            ApiError::ForbiddenRole
        );
        let view = engine.heartbeat_rewrite(&rw("a")).unwrap();
        assert_eq!(view.lease_expires_at, 1_000 + lease - 1 + lease);

        clock.advance(lease);
        // The holder of an ended lease carries on unless someone took over.
        engine.heartbeat_rewrite(&rw("a")).unwrap();
        clock.advance(lease);
        let view = engine.take_over_rewrite(&rw("b")).unwrap();
        assert_eq!(view.holder.as_str(), "b");
        assert_eq!(
            engine.take_over_rewrite(&rw("b")).unwrap().holder.as_str(),
            "b"
        );

        // The former holder is now a stranger to the session.
        assert_eq!(
            engine.heartbeat_rewrite(&rw("a")).unwrap_err(),
            ApiError::LeaseHeld
        );
        assert_eq!(
            engine.commit_rewrite(&rw("a"), &key("bb")).unwrap_err(),
            ApiError::LeaseHeld
        );
        assert_eq!(
            engine.abort_rewrite(&rw("a"), &key("bb")).unwrap_err(),
            ApiError::LeaseHeld
        );
        engine.abort_rewrite(&rw("b"), &key("bb")).unwrap();
    }

    #[test]
    fn an_upload_staged_by_a_former_holder_cannot_commit() {
        let (mut engine, clock) = engine();
        engine.begin_rewrite(&rw("a"), migrate("bb")).unwrap();
        let mut req = request("00000001-cccccccccccc", 1);
        req.expected_key_id = Some(key("bb"));
        req.in_rewrite = true;
        let Begun::Ticket(ticket) = engine.begin_upload(&rw("a"), req).unwrap() else {
            panic!("no ticket");
        };
        engine
            .put_upload_content(
                &rw("a"),
                &ticket.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        clock.advance(Limits::default().lease_secs + 1);
        engine.take_over_rewrite(&rw("b")).unwrap();
        assert_eq!(
            engine
                .commit_upload(&rw("a"), &ticket.upload_id)
                .unwrap_err(),
            ApiError::LeaseHeld
        );
    }
}
