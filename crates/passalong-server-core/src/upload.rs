//! The upload lifecycle: `beginUpload`, `putUploadContent`, `commitUpload`,
//! `abortUpload`.

//!
//! The upload id is the idempotency key. Every request may be sent again:
//! a client that lost an answer repeats the request and gets the answer it
//! missed. `docs/api/README.md`, "Replays", has the table these rules
//! implement.

use std::collections::BTreeMap;
use std::io::Read;

use crate::error::ApiError;
use crate::ids::{ApiKeyId, ItemId, KeyId, UploadId};
use crate::shelf::{Envelope, ItemShelf};
use crate::workspace::{Caller, Cleanup, Rules, find_in};

/// `beginUpload`'s request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadRequest {
    /// The id the client proposes. It must fix the id before it uploads,
    /// because the id is associated data of its sealed metadata.
    pub id: ItemId,
    /// The client's `meta.json`, opaque.
    pub meta: Vec<u8>,
    /// The content's size in bytes, as it will be stored.
    pub size: u64,
    /// The data key the item is made under; `None` for plaintext.
    pub expected_key_id: Option<KeyId>,
    /// Whether this stages an item of the open rewrite's next generation.
    pub in_rewrite: bool,
}

/// An upload the server is ready to receive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTicket {
    /// The upload's id.
    pub upload_id: UploadId,
    /// When the server forgets an unfinished upload.
    pub expires_at: u64,
}

/// `Store::put`'s result: the stored item, and whether this upload made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// The stored item: the proposed one, or the older one with the same
    /// content.
    pub id: ItemId,
    /// `false` when identical content was already stored.
    pub created: bool,
}

/// `beginUpload`'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begun {
    /// Send the content, then commit.
    Ticket(UploadTicket),
    /// The content is stored already; there is nothing to send.
    Stored(PutOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ticket {
    pub(crate) owner: ApiKeyId,
    pub(crate) request: UploadRequest,
    pub(crate) expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tombstone {
    pub(crate) owner: ApiKeyId,
    pub(crate) outcome: PutOutcome,
    pub(crate) expires_at: u64,
}

/// The uploads in progress and the outcomes remembered for replays. In the
/// server both live in the control database, so they survive a restart.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Uploads {
    pub(crate) tickets: BTreeMap<UploadId, Ticket>,
    pub(crate) tombstones: BTreeMap<UploadId, Tombstone>,
}

impl<S: ItemShelf> Rules<'_, S> {
    /// Bytes promised to ordinary uploads that have begun and not finished.
    /// Uploads of a rewrite are counted apart, against an allowance of the
    /// same size as the quota: a rotation needs room for a second copy of
    /// every item, and refusing it for quota would leave a full workspace
    /// unable ever to change its key. So a workspace can hold twice its
    /// quota, and only while a rewrite is open.
    pub fn reserved_bytes(&self) -> u64 {
        self.reserved_by(self.clock.now(), false)
    }

    /// Bytes promised to live uploads: those of a rewrite, or the others.
    fn reserved_by(&self, now: u64, in_rewrite: bool) -> u64 {
        self.rec
            .uploads
            .tickets
            .values()
            .filter(|ticket| ticket.expires_at > now && ticket.request.in_rewrite == in_rewrite)
            .map(|ticket| ticket.request.size)
            .sum()
    }

    /// Bytes published in the open rewrite's next generation.
    fn staged_bytes(&self) -> u64 {
        match &self.rec.state {
            crate::workspace::State::Rewriting(session) => self
                .rec
                .used
                .get(&session.staged_generation)
                .copied()
                .unwrap_or(0),
            crate::workspace::State::Settled(_) => 0,
        }
    }

    /// How many uploads have begun and are neither finished nor forgotten,
    /// expired ones included until the janitor passes. Every staging place
    /// on the shelf belongs to one of them.
    pub fn pending_uploads(&self) -> usize {
        self.rec.uploads.tickets.len()
    }

    /// Where an upload goes, if it may go anywhere: the generation and the
    /// key id it must be made under. Checked when the upload begins and
    /// again, under the same lock as the publish, when it commits.
    fn upload_target(&self, caller: &Caller, request: &UploadRequest) -> Result<u64, ApiError> {
        if request.in_rewrite {
            return self.rewrite_target(caller, request.expected_key_id.as_ref());
        }
        self.check_writable(request.expected_key_id.as_ref())?;
        Ok(self.rec.generation)
    }

    /// `beginUpload`. Sent again by the same key with the same request, it
    /// answers the live ticket and reserves nothing more.
    ///
    /// # Errors
    ///
    /// [`ApiError::ForbiddenRole`], [`ApiError::RewriteInProgress`],
    /// [`ApiError::KeyIdMismatch`], [`ApiError::ItemTooLarge`], and
    /// [`ApiError::QuotaExceeded`].
    pub fn begin_upload(
        &mut self,
        caller: &Caller,
        request: UploadRequest,
    ) -> Result<Begun, ApiError> {
        caller.must_write()?;
        let generation = self.upload_target(caller, &request)?;
        if self
            .limits
            .max_item_bytes
            .is_some_and(|max| request.size > max)
        {
            return Err(ApiError::ItemTooLarge);
        }
        if let Some(id) = find_in(self.shelf, generation, request.id.content_key())? {
            // Someone stored this content first. A ticket the caller still
            // holds for the same request is of no use any more, and would
            // hold quota until it expired.
            let given_up: Vec<UploadId> = self
                .rec
                .uploads
                .tickets
                .iter()
                .filter(|(_, ticket)| ticket.owner == caller.key && ticket.request == request)
                .map(|(upload, _)| upload.clone())
                .collect();
            for upload in given_up {
                self.rec.uploads.tickets.remove(&upload);
                self.after.push(Cleanup::RemoveStaging(upload));
            }
            return Ok(Begun::Stored(PutOutcome { id, created: false }));
        }
        let now = self.clock.now();
        let live = self.rec.uploads.tickets.iter().find(|(_, ticket)| {
            ticket.expires_at > now && ticket.owner == caller.key && ticket.request == request
        });
        if let Some((upload_id, ticket)) = live {
            // Harmless when it exists; and if it went missing, the ticket is
            // usable again.
            self.shelf.stage_create(upload_id)?;
            return Ok(Begun::Ticket(UploadTicket {
                upload_id: upload_id.clone(),
                expires_at: ticket.expires_at,
            }));
        }
        let held = if request.in_rewrite {
            // The next generation has the quota as an allowance of its own.
            let staged = self.rec.used.get(&generation).copied().unwrap_or(0);
            staged.saturating_add(self.reserved_by(now, true))
        } else {
            let current = self.used_bytes() - self.staged_bytes();
            current.saturating_add(self.reserved_by(now, false))
        };
        if held.saturating_add(request.size) > self.limits.quota_bytes {
            return Err(ApiError::QuotaExceeded);
        }
        // An id the record already knows, as an upload in flight or as a
        // remembered outcome, is never handed out again: the id is what a
        // replay is recognised by. With a real random source the loop runs
        // once; the kill harness, whose source repeats itself, found that it
        // must exist.
        let upload_id = loop {
            let candidate = UploadId::generate(&mut *self.rng);
            let known = self.rec.uploads.tickets.contains_key(&candidate)
                || self.rec.uploads.tombstones.contains_key(&candidate);
            if !known {
                break candidate;
            }
        };
        let expires_at = now + self.limits.staging_secs;
        self.shelf.stage_create(&upload_id)?;
        self.rec.uploads.tickets.insert(
            upload_id.clone(),
            Ticket {
                owner: caller.key.clone(),
                request,
                expires_at,
            },
        );
        Ok(Begun::Ticket(UploadTicket {
            upload_id,
            expires_at,
        }))
    }

    fn live_ticket(&self, caller: &Caller, upload: &UploadId) -> Option<&Ticket> {
        self.rec
            .uploads
            .tickets
            .get(upload)
            .filter(|ticket| ticket.owner == caller.key && ticket.expires_at > self.clock.now())
    }

    fn tombstone(&self, caller: &Caller, upload: &UploadId) -> Option<&Tombstone> {
        self.rec
            .uploads
            .tombstones
            .get(upload)
            .filter(|stone| stone.owner == caller.key && stone.expires_at > self.clock.now())
    }

    /// `putUploadContent`. Before the commit it replaces what was sent
    /// before; after it, it is acknowledged and the content discarded.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] for an upload that is not the caller's, has
    /// expired, or never was.
    pub fn put_upload_content(
        &mut self,
        caller: &Caller,
        upload: &UploadId,
        content: &mut dyn Read,
    ) -> Result<(), ApiError> {
        if self.tombstone(caller, upload).is_some() {
            return Ok(());
        }
        let announced = self
            .live_ticket(caller, upload)
            .ok_or(ApiError::NotFound)?
            .request
            .size;
        self.shelf
            .stage_write(upload, content, announced)
            .map(|_| ())
    }

    /// `commitUpload`: checks, deduplicates, and publishes, all under the
    /// workspace lock, so two uploads of the same content yield one item
    /// whichever commits first. Sent again, it answers the same outcome,
    /// with the original `created`, for as long as the outcome is kept.
    ///
    /// # Errors
    ///
    /// [`ApiError::NotFound`] once the outcome is forgotten, which the
    /// client settles with `getItem`; [`ApiError::ContentMismatch`], after
    /// which the content may be sent again; and the refusals of
    /// [`Engine::begin_upload`], checked again because the workspace may
    /// have changed since.
    pub fn commit_upload(
        &mut self,
        caller: &Caller,
        upload: &UploadId,
    ) -> Result<PutOutcome, ApiError> {
        if let Some(stone) = self.tombstone(caller, upload) {
            return Ok(stone.outcome.clone());
        }
        let ticket = self
            .live_ticket(caller, upload)
            .ok_or(ApiError::NotFound)?
            .clone();
        let generation = self.upload_target(caller, &ticket.request)?;
        let staged = self.shelf.stage_size(upload)?;
        if staged.is_none() {
            // The staging place is gone though the upload never finished:
            // an earlier commit published the item and died before it could
            // record that. Only this upload could move its own staging
            // place, so the item under its id is its own. Finish the record,
            // and answer what that commit would have answered.
            let published = self.shelf.get(generation, &ticket.request.id)?;
            if published.is_some_and(|item| item.size == ticket.request.size) {
                let bytes = self.shelf.bytes(generation)?;
                self.rec.used.insert(generation, bytes);
                tracing::warn!(target: "passalong_server::upload", item = %ticket.request.id, upload = %upload.as_str(), "finished a publish that was never recorded");
                return Ok(self.finish_upload(upload, ticket.owner, ticket.request.id, true));
            }
        }
        if staged != Some(ticket.request.size) {
            return Err(ApiError::ContentMismatch);
        }
        let existing = find_in(self.shelf, generation, ticket.request.id.content_key())?;
        let outcome = match existing {
            Some(id) => PutOutcome { id, created: false },
            None => {
                let envelope = Envelope {
                    meta: ticket.request.meta.clone(),
                    under: ticket.request.expected_key_id.clone(),
                    received_at: self.clock.now(),
                };
                self.shelf
                    .publish(upload, generation, &ticket.request.id, envelope)?;
                crate::fault::point("upload: after the publish");
                self.add_used(generation, ticket.request.size);
                PutOutcome {
                    id: ticket.request.id.clone(),
                    created: true,
                }
            }
        };
        Ok(self.finish_upload(upload, ticket.owner, outcome.id, outcome.created))
    }

    /// Records an upload's outcome: the ticket goes, the outcome stays for
    /// replays, and what is left of the staging place is removed once the
    /// record is stored. After a publish that is nothing, the staging place
    /// having become the item; after a duplicate it is the rejected copy.
    fn finish_upload(
        &mut self,
        upload: &UploadId,
        owner: ApiKeyId,
        id: ItemId,
        created: bool,
    ) -> PutOutcome {
        let outcome = PutOutcome { id, created };
        self.after.push(Cleanup::RemoveStaging(upload.clone()));
        self.rec.uploads.tickets.remove(upload);
        self.rec.uploads.tombstones.insert(
            upload.clone(),
            Tombstone {
                owner,
                outcome: outcome.clone(),
                expires_at: self.clock.now() + self.limits.staging_secs,
            },
        );
        outcome
    }

    /// `abortUpload`. Succeeds whether or not there was anything to abort;
    /// another key's upload is left alone.
    ///
    /// # Errors
    ///
    /// None today; the result leaves room for the filesystem's.
    pub fn abort_upload(&mut self, caller: &Caller, upload: &UploadId) -> Result<(), ApiError> {
        let own = self
            .rec
            .uploads
            .tickets
            .get(upload)
            .is_some_and(|ticket| ticket.owner == caller.key);
        if own {
            self.rec.uploads.tickets.remove(upload);
            self.after.push(Cleanup::RemoveStaging(upload.clone()));
        }
        Ok(())
    }

    /// Drops the uploads of a rewrite that ended.
    pub(crate) fn drop_rewrite_uploads(&mut self) {
        let ended: Vec<UploadId> = self
            .rec
            .uploads
            .tickets
            .iter()
            .filter(|(_, ticket)| ticket.request.in_rewrite)
            .map(|(upload, _)| upload.clone())
            .collect();
        for upload in ended {
            self.rec.uploads.tickets.remove(&upload);
            self.after.push(Cleanup::RemoveStaging(upload));
        }
    }

    /// What a workspace needs when it is opened, whatever the process that
    /// had it before was doing when it stopped: the bytes the quota counts
    /// are taken from the shelf, and what nothing points to is removed.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`].
    pub fn reconcile(&mut self) -> Result<(), ApiError> {
        let mut corrected = 0;
        for generation in self.live_generations() {
            let bytes = self.shelf.bytes(generation)?;
            let booked = self.rec.used.insert(generation, bytes).unwrap_or(0);
            corrected += u64::from(booked != bytes);
        }
        let removed = self.clean_staging()?;
        if corrected > 0 || removed > 0 {
            tracing::warn!(target: "passalong_server::reconcile", generations_corrected = corrected, staging_places_removed = removed, "the workspace was not closed cleanly; repaired");
        }
        Ok(())
    }

    /// `cleanStaging`, which the janitor also runs unasked: forgets expired
    /// uploads and outcomes, and removes what interrupted changes left on
    /// the shelf: staging places without a ticket, and generations the
    /// workspace no longer points to. Returns how many staging places went.
    ///
    /// # Errors
    ///
    /// [`ApiError::ServiceUnavailable`].
    pub fn clean_staging(&mut self) -> Result<usize, ApiError> {
        let now = self.clock.now();
        self.rec
            .uploads
            .tickets
            .retain(|_, ticket| ticket.expires_at > now);
        self.rec
            .uploads
            .tombstones
            .retain(|_, stone| stone.expires_at > now);
        let mut removed = 0;
        for upload in self.shelf.staged()? {
            if !self.rec.uploads.tickets.contains_key(&upload) {
                // Asked for, not done: see `Cleanup`.
                self.after.push(Cleanup::RemoveStaging(upload));
                removed += 1;
            }
        }
        let live = self.live_generations();
        for generation in self.shelf.generations()? {
            if !live.contains(&generation) {
                self.after.push(Cleanup::DropGeneration(generation));
                self.rec.used.remove(&generation);
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::error::ApiError;
    use crate::ids::ItemId;
    use crate::shelf::ItemShelf;
    use crate::workspace::tests::{content, engine, key, ro, rw};
    use crate::workspace::{Limits, Partition};

    pub(crate) fn request(id: &str, size: u64) -> UploadRequest {
        UploadRequest {
            id: ItemId::parse(id).unwrap(),
            meta: br#"{"schema":1}"#.to_vec(),
            size,
            expected_key_id: None,
            in_rewrite: false,
        }
    }

    fn ticket(begun: Begun) -> UploadTicket {
        match begun {
            Begun::Ticket(ticket) => ticket,
            Begun::Stored(outcome) => panic!("already stored: {outcome:?}"),
        }
    }

    const A: &str = "00000001-aaaaaaaaaaaa";

    #[test]
    fn an_upload_is_begun_filled_and_committed() {
        let (mut engine, clock) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 5)).unwrap());
        assert_eq!(t.expires_at, 1_000 + Limits::default().staging_secs);
        assert_eq!(engine.reserved_bytes().unwrap(), 5);
        assert!(engine.item_ids(Partition::Current).unwrap().is_empty());

        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"hello".to_vec()),
            )
            .unwrap();
        clock.advance(3);
        let outcome = engine.commit_upload(&rw("a"), &t.upload_id).unwrap();
        assert_eq!(
            outcome,
            PutOutcome {
                id: ItemId::parse(A).unwrap(),
                created: true
            }
        );

        let item = engine
            .item(Partition::Current, &outcome.id)
            .unwrap()
            .unwrap();
        assert_eq!(item.size, 5);
        assert_eq!(content(&engine, Partition::Current, &outcome.id), b"hello");
        assert_eq!(item.envelope.meta, br#"{"schema":1}"#);
        assert_eq!(item.envelope.received_at, 1_003);
        assert_eq!(item.envelope.under, None);
        assert_eq!(
            (
                engine.used_bytes().unwrap(),
                engine.reserved_bytes().unwrap()
            ),
            (5, 0)
        );
        assert!(engine.shelf().staged().unwrap().is_empty());
    }

    #[test]
    fn pending_uploads_are_counted_until_they_finish() {
        let (mut engine, _) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 1)).unwrap());
        assert_eq!(engine.pending_uploads().unwrap(), 1);
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        engine.commit_upload(&rw("a"), &t.upload_id).unwrap();
        assert_eq!(engine.pending_uploads().unwrap(), 0);
    }

    #[test]
    fn beginning_again_returns_the_same_ticket_and_reserves_nothing_more() {
        let (mut engine, _) = engine();
        let first = ticket(engine.begin_upload(&rw("a"), request(A, 5)).unwrap());
        let again = ticket(engine.begin_upload(&rw("a"), request(A, 5)).unwrap());
        assert_eq!(first, again);
        assert_eq!(engine.reserved_bytes().unwrap(), 5);
        // Another key, or another size, is another upload.
        let other = ticket(engine.begin_upload(&rw("b"), request(A, 5)).unwrap());
        assert_ne!(first.upload_id, other.upload_id);
        assert_eq!(engine.reserved_bytes().unwrap(), 10);
    }

    #[test]
    fn content_can_be_sent_again_until_the_commit_and_is_ignored_after() {
        let (mut engine, _) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 2)).unwrap());
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"no".to_vec()),
            )
            .unwrap();
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"ok".to_vec()),
            )
            .unwrap();
        engine.commit_upload(&rw("a"), &t.upload_id).unwrap();
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"late".to_vec()),
            )
            .unwrap();
        let id = ItemId::parse(A).unwrap();
        assert_eq!(content(&engine, Partition::Current, &id), b"ok");
        assert!(engine.shelf().staged().unwrap().is_empty());
    }

    #[test]
    fn committing_again_returns_the_same_outcome_until_it_is_forgotten() {
        let (mut engine, clock) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 1)).unwrap());
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        let first = engine.commit_upload(&rw("a"), &t.upload_id).unwrap();
        assert!(first.created);
        assert_eq!(engine.commit_upload(&rw("a"), &t.upload_id).unwrap(), first);
        assert_eq!(engine.used_bytes().unwrap(), 1);

        clock.advance(Limits::default().staging_secs + 1);
        engine.clean_staging().unwrap();
        assert_eq!(
            engine.commit_upload(&rw("a"), &t.upload_id).unwrap_err(),
            ApiError::NotFound
        );
        // The client settles it by asking for the item it proposed.
        assert!(
            engine
                .item(Partition::Current, &first.id)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn the_same_content_is_stored_once_whichever_upload_commits_first() {
        let (mut engine, _) = engine();
        let one = ticket(
            engine
                .begin_upload(&rw("a"), request("00000001-aaaaaaaaaaaa", 1))
                .unwrap(),
        );
        let two = ticket(
            engine
                .begin_upload(&rw("b"), request("00000002-aaaaaaaaaaaa", 1))
                .unwrap(),
        );
        engine
            .put_upload_content(
                &rw("a"),
                &one.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        engine
            .put_upload_content(
                &rw("b"),
                &two.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        let second = engine.commit_upload(&rw("b"), &two.upload_id).unwrap();
        let first = engine.commit_upload(&rw("a"), &one.upload_id).unwrap();
        assert!(second.created);
        assert_eq!(
            first,
            PutOutcome {
                id: second.id.clone(),
                created: false
            }
        );
        assert_eq!(
            engine.item_ids(Partition::Current).unwrap(),
            vec![second.id.clone()]
        );
        assert_eq!(
            (
                engine.used_bytes().unwrap(),
                engine.reserved_bytes().unwrap()
            ),
            (1, 0)
        );
        assert!(engine.shelf().staged().unwrap().is_empty());

        // Once stored, beginning the same content answers at once.
        match engine
            .begin_upload(&rw("c"), request("00000003-aaaaaaaaaaaa", 1))
            .unwrap()
        {
            Begun::Stored(outcome) => assert_eq!(
                outcome,
                PutOutcome {
                    id: second.id,
                    created: false
                }
            ),
            Begun::Ticket(_) => panic!("stored content was given a ticket"),
        }
    }

    #[test]
    fn a_ticket_is_given_up_when_its_content_turns_out_to_be_stored() {
        // Found by the two-process test: a client whose commit was refused
        // while a rewrite was open began again, was told that someone else
        // had stored the content meanwhile, and left its first ticket
        // behind, holding quota until it expired.
        let (mut engine, _) = engine();
        let mine = ticket(
            engine
                .begin_upload(&rw("a"), request("00000001-aaaaaaaaaaaa", 1))
                .unwrap(),
        );
        engine
            .put_upload_content(
                &rw("a"),
                &mine.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        let theirs = ticket(
            engine
                .begin_upload(&rw("b"), request("00000002-aaaaaaaaaaaa", 1))
                .unwrap(),
        );
        engine
            .put_upload_content(
                &rw("b"),
                &theirs.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        assert!(
            engine
                .commit_upload(&rw("b"), &theirs.upload_id)
                .unwrap()
                .created
        );

        assert!(matches!(
            engine
                .begin_upload(&rw("a"), request("00000001-aaaaaaaaaaaa", 1))
                .unwrap(),
            Begun::Stored(_)
        ));
        assert_eq!(engine.pending_uploads().unwrap(), 0);
        assert_eq!(engine.reserved_bytes().unwrap(), 0);
        assert!(engine.shelf().staged().unwrap().is_empty());
        // Its commit, should it still arrive, finds nothing, and the client
        // settles that with `getItem`, as after any forgotten upload.
        assert_eq!(
            engine.commit_upload(&rw("a"), &mine.upload_id).unwrap_err(),
            ApiError::NotFound
        );
    }

    #[test]
    fn a_commit_checks_the_announced_size() {
        let (mut engine, _) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 5)).unwrap());
        assert_eq!(
            engine.commit_upload(&rw("a"), &t.upload_id).unwrap_err(),
            ApiError::ContentMismatch
        );
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"four".to_vec()),
            )
            .unwrap();
        assert_eq!(
            engine.commit_upload(&rw("a"), &t.upload_id).unwrap_err(),
            ApiError::ContentMismatch
        );
        // The ticket survives a refusal, so the content can be sent again.
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"fiveb".to_vec()),
            )
            .unwrap();
        assert!(
            engine
                .commit_upload(&rw("a"), &t.upload_id)
                .unwrap()
                .created
        );
    }

    #[test]
    fn quota_and_item_size_are_enforced_when_the_upload_begins() {
        let (mut engine, _) = engine();
        engine.limits.quota_bytes = 10;
        engine.limits.max_item_bytes = Some(8);
        assert_eq!(
            engine.begin_upload(&rw("a"), request(A, 9)).unwrap_err(),
            ApiError::ItemTooLarge
        );
        ticket(engine.begin_upload(&rw("a"), request(A, 8)).unwrap());
        let err = engine
            .begin_upload(&rw("a"), request("00000002-bbbbbbbbbbbb", 3))
            .unwrap_err();
        assert_eq!(err, ApiError::QuotaExceeded);
        ticket(
            engine
                .begin_upload(&rw("a"), request("00000002-bbbbbbbbbbbb", 2))
                .unwrap(),
        );
    }

    #[test]
    fn only_a_writer_under_the_current_key_may_upload() {
        let (mut engine, _) = engine();
        assert_eq!(
            engine.begin_upload(&ro("k"), request(A, 1)).unwrap_err(),
            ApiError::ForbiddenRole
        );
        let mut sealed = request(A, 1);
        sealed.expected_key_id = Some(key("aa"));
        assert_eq!(
            engine.begin_upload(&rw("a"), sealed.clone()).unwrap_err(),
            ApiError::KeyIdMismatch
        );

        // A plaintext upload that the workspace's sealing overtakes is
        // refused at the commit, under the same lock as the sealing.
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 1)).unwrap());
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"x".to_vec()),
            )
            .unwrap();
        engine
            .enable_encryption(&rw("b"), key("aa"), b"h".to_vec())
            .unwrap();
        assert_eq!(
            engine.commit_upload(&rw("a"), &t.upload_id).unwrap_err(),
            ApiError::KeyIdMismatch
        );
        assert!(engine.item_ids(Partition::Current).unwrap().is_empty());

        let t = ticket(engine.begin_upload(&rw("a"), sealed).unwrap());
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"s".to_vec()),
            )
            .unwrap();
        let outcome = engine.commit_upload(&rw("a"), &t.upload_id).unwrap();
        let item = engine
            .item(Partition::Current, &outcome.id)
            .unwrap()
            .unwrap();
        assert_eq!(item.envelope.under, Some(key("aa")));
    }

    #[test]
    fn an_upload_belongs_to_the_key_that_began_it() {
        let (mut engine, _) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 1)).unwrap());
        assert_eq!(
            engine
                .put_upload_content(&rw("b"), &t.upload_id, &mut std::io::Cursor::new(vec![1]))
                .unwrap_err(),
            ApiError::NotFound
        );
        assert_eq!(
            engine.commit_upload(&rw("b"), &t.upload_id).unwrap_err(),
            ApiError::NotFound
        );
        engine.abort_upload(&rw("b"), &t.upload_id).unwrap();
        assert_eq!(engine.reserved_bytes().unwrap(), 1);
    }

    #[test]
    fn aborting_frees_the_reservation_and_may_be_repeated() {
        let (mut engine, _) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 4)).unwrap());
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"data".to_vec()),
            )
            .unwrap();
        engine.abort_upload(&rw("a"), &t.upload_id).unwrap();
        engine.abort_upload(&rw("a"), &t.upload_id).unwrap();
        assert_eq!(engine.reserved_bytes().unwrap(), 0);
        assert!(engine.shelf().staged().unwrap().is_empty());
        assert_eq!(
            engine.commit_upload(&rw("a"), &t.upload_id).unwrap_err(),
            ApiError::NotFound
        );
    }

    #[test]
    fn the_janitor_removes_uploads_nobody_finished() {
        let (mut engine, clock) = engine();
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 4)).unwrap());
        engine
            .put_upload_content(
                &rw("a"),
                &t.upload_id,
                &mut std::io::Cursor::new(b"data".to_vec()),
            )
            .unwrap();
        assert_eq!(engine.clean_staging().unwrap(), 0);
        clock.advance(Limits::default().staging_secs + 1);
        assert_eq!(engine.clean_staging().unwrap(), 1);
        assert_eq!(engine.reserved_bytes().unwrap(), 0);
        assert!(engine.shelf().staged().unwrap().is_empty());
        // An expired ticket is gone even before the janitor passes.
        let t = ticket(engine.begin_upload(&rw("a"), request(A, 4)).unwrap());
        clock.advance(Limits::default().staging_secs + 1);
        assert_eq!(
            engine.commit_upload(&rw("a"), &t.upload_id).unwrap_err(),
            ApiError::NotFound
        );
    }

    #[test]
    fn the_janitor_drops_generations_nothing_points_to() {
        let (mut engine, _) = engine();
        let orphan = crate::ids::UploadId::generate(&mut crate::random::SeededRandom::new(9));
        engine.shelf.stage_create(&orphan).unwrap();
        engine
            .shelf
            .publish(
                &orphan,
                77,
                &ItemId::parse(A).unwrap(),
                crate::shelf::Envelope {
                    meta: vec![],
                    under: None,
                    received_at: 0,
                },
            )
            .unwrap();
        engine.clean_staging().unwrap();
        assert!(engine.shelf().generations().unwrap().is_empty());
    }
}
