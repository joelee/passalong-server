//! One suite for every [`ItemShelf`]: what the rules rely on, whichever
//! shelf is under them (PLAN-00002, REQ-02 and REQ-03).

use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};

use passalong_server_core::error::ApiError;
use passalong_server_core::ids::{ItemId, KeyId, UploadId};
use passalong_server_core::random::SeededRandom;
use passalong_server_core::shelf::{Envelope, FsShelf, ItemShelf, MemoryShelf};

fn id(text: &str) -> ItemId {
    ItemId::parse(text).unwrap()
}

fn envelope() -> Envelope {
    Envelope {
        meta: br#"{"schema":1}"#.to_vec(),
        under: Some(KeyId::parse("aa").unwrap()),
        received_at: 1_003,
    }
}

fn stage<S: ItemShelf>(shelf: &S, rng: &mut SeededRandom, content: &[u8]) -> UploadId {
    let upload = UploadId::generate(rng);
    shelf.stage_create(&upload).unwrap();
    let written = shelf
        .stage_write(&upload, &mut &content[..], content.len() as u64)
        .unwrap();
    assert_eq!(written, content.len() as u64);
    upload
}

fn content_of<S: ItemShelf>(shelf: &S, generation: u64, id: &ItemId) -> Vec<u8> {
    let mut bytes = Vec::new();
    shelf
        .open_content(generation, id)
        .unwrap()
        .expect("the item has content")
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

/// A reader that notes the largest buffer it was ever asked to fill.
struct Counting<'a> {
    left: usize,
    largest: &'a AtomicUsize,
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.largest.fetch_max(buf.len(), Ordering::SeqCst);
        let n = buf.len().min(self.left);
        buf[..n].fill(7);
        self.left -= n;
        Ok(n)
    }
}

fn a_staged_upload_is_published_once_and_is_then_an_item<S: ItemShelf>(shelf: S) {
    let mut rng = SeededRandom::new(1);
    let upload = UploadId::generate(&mut rng);
    shelf.stage_create(&upload).unwrap();
    shelf.stage_create(&upload).unwrap();
    assert_eq!(shelf.stage_size(&upload).unwrap(), Some(0));
    shelf.stage_write(&upload, &mut &b"hello"[..], 5).unwrap();
    // Sent again, the content replaces what was sent before.
    shelf.stage_write(&upload, &mut &b"hi"[..], 5).unwrap();
    assert_eq!(shelf.stage_size(&upload).unwrap(), Some(2));
    assert_eq!(shelf.staged().unwrap(), vec![upload.clone()]);

    let a = id("00000001-aaaaaaaaaaaa");
    assert!(shelf.publish(&upload, 0, &a, envelope()).unwrap());
    assert!(shelf.staged().unwrap().is_empty());
    let item = shelf.get(0, &a).unwrap().unwrap();
    assert_eq!(item.size, 2);
    assert_eq!(item.envelope, envelope());
    assert_eq!(content_of(&shelf, 0, &a), b"hi");
    assert_eq!(shelf.bytes(0).unwrap(), 2);
    assert_eq!(shelf.ids(0).unwrap(), vec![a]);
}

fn publishing_never_replaces_an_item<S: ItemShelf>(shelf: S) {
    let mut rng = SeededRandom::new(2);
    let a = id("00000001-aaaaaaaaaaaa");
    let first = stage(&shelf, &mut rng, b"one");
    let second = stage(&shelf, &mut rng, b"two");
    assert!(shelf.publish(&first, 0, &a, envelope()).unwrap());
    assert!(!shelf.publish(&second, 0, &a, envelope()).unwrap());
    assert_eq!(content_of(&shelf, 0, &a), b"one");
    // The loser's staging is still there, for its owner to remove.
    assert_eq!(shelf.staged().unwrap(), vec![second.clone()]);
    shelf.stage_remove(&second).unwrap();
    shelf.stage_remove(&second).unwrap();
    assert!(shelf.staged().unwrap().is_empty());
}

fn what_was_never_staged_cannot_be_written_or_published<S: ItemShelf>(shelf: S) {
    let upload = UploadId::generate(&mut SeededRandom::new(3));
    let a = id("00000001-aaaaaaaaaaaa");
    assert_eq!(
        shelf.stage_write(&upload, &mut &b"x"[..], 1).unwrap_err(),
        ApiError::NotFound
    );
    assert_eq!(
        shelf.publish(&upload, 0, &a, envelope()).unwrap_err(),
        ApiError::NotFound
    );
    assert_eq!(shelf.stage_size(&upload).unwrap(), None);
    assert!(shelf.get(0, &a).unwrap().is_none());
    assert!(shelf.open_content(0, &a).unwrap().is_none());
}

fn generations_are_separate_listed_newest_first_and_dropped_whole<S: ItemShelf>(shelf: S) {
    let mut rng = SeededRandom::new(4);
    for (generation, text) in [
        (0, "00000001-aaaaaaaaaaaa"),
        (0, "00000002-bbbbbbbbbbbb"),
        (3, "00000003-cccccccccccc"),
    ] {
        let upload = stage(&shelf, &mut rng, b"abc");
        assert!(
            shelf
                .publish(&upload, generation, &id(text), envelope())
                .unwrap()
        );
    }
    assert_eq!(
        shelf.ids(0).unwrap(),
        vec![id("00000002-bbbbbbbbbbbb"), id("00000001-aaaaaaaaaaaa")]
    );
    assert_eq!(shelf.generations().unwrap(), vec![0, 3]);
    let removed = shelf
        .remove(0, &id("00000001-aaaaaaaaaaaa"))
        .unwrap()
        .unwrap();
    assert_eq!(removed.size, 3);
    assert!(
        shelf
            .remove(0, &id("00000001-aaaaaaaaaaaa"))
            .unwrap()
            .is_none()
    );
    assert_eq!(shelf.bytes(0).unwrap(), 3);
    shelf.drop_generation(0).unwrap();
    shelf.drop_generation(0).unwrap();
    assert_eq!(shelf.generations().unwrap(), vec![3]);
    assert_eq!(shelf.bytes(0).unwrap(), 0);
    assert!(shelf.ids(7).unwrap().is_empty());
}

fn content_longer_than_announced_is_refused_and_leaves_nothing<S: ItemShelf>(shelf: S) {
    let upload = UploadId::generate(&mut SeededRandom::new(5));
    shelf.stage_create(&upload).unwrap();
    let err = shelf
        .stage_write(&upload, &mut &b"123456"[..], 5)
        .unwrap_err();
    assert_eq!(err, ApiError::ContentMismatch);
    assert_eq!(shelf.stage_size(&upload).unwrap(), Some(0));
    // Shorter than announced is not the shelf's business: the commit checks.
    assert_eq!(shelf.stage_write(&upload, &mut &b"123"[..], 5).unwrap(), 3);
    assert_eq!(shelf.stage_size(&upload).unwrap(), Some(3));
}

fn content_streams_in_and_out_in_pieces<S: ItemShelf>(shelf: S) {
    const SIZE: usize = 1024 * 1024 + 17;
    let largest = AtomicUsize::new(0);
    let upload = UploadId::generate(&mut SeededRandom::new(6));
    shelf.stage_create(&upload).unwrap();
    let mut source = Counting {
        left: SIZE,
        largest: &largest,
    };
    assert_eq!(
        shelf
            .stage_write(&upload, &mut source, SIZE as u64)
            .unwrap(),
        SIZE as u64
    );
    let asked = largest.load(Ordering::SeqCst);
    assert!(
        asked > 0 && asked <= 64 * 1024,
        "asked for {asked} bytes at once"
    );

    let a = id("00000001-aaaaaaaaaaaa");
    assert!(shelf.publish(&upload, 0, &a, envelope()).unwrap());
    let mut reader = shelf.open_content(0, &a).unwrap().unwrap();
    let (mut total, mut piece) = (0, [0_u8; 4096]);
    loop {
        let n = reader.read(&mut piece).unwrap();
        if n == 0 {
            break;
        }
        assert!(piece[..n].iter().all(|byte| *byte == 7));
        total += n;
    }
    assert_eq!(total, SIZE);
    assert_eq!(shelf.bytes(0).unwrap(), SIZE as u64);
}

/// Instantiates the suite for one shelf.
macro_rules! shelf_suite {
    ($module:ident, $make:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn a_staged_upload_is_published_once_and_is_then_an_item() {
                let (shelf, _guard) = $make;
                super::a_staged_upload_is_published_once_and_is_then_an_item(shelf);
            }
            #[test]
            fn publishing_never_replaces_an_item() {
                let (shelf, _guard) = $make;
                super::publishing_never_replaces_an_item(shelf);
            }
            #[test]
            fn what_was_never_staged_cannot_be_written_or_published() {
                let (shelf, _guard) = $make;
                super::what_was_never_staged_cannot_be_written_or_published(shelf);
            }
            #[test]
            fn generations_are_separate_listed_newest_first_and_dropped_whole() {
                let (shelf, _guard) = $make;
                super::generations_are_separate_listed_newest_first_and_dropped_whole(shelf);
            }
            #[test]
            fn content_longer_than_announced_is_refused_and_leaves_nothing() {
                let (shelf, _guard) = $make;
                super::content_longer_than_announced_is_refused_and_leaves_nothing(shelf);
            }
            #[test]
            fn content_streams_in_and_out_in_pieces() {
                let (shelf, _guard) = $make;
                super::content_streams_in_and_out_in_pieces(shelf);
            }
        }
    };
}

fn fs_shelf() -> (FsShelf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (FsShelf::open(dir.path().join("ws")).unwrap(), dir)
}

shelf_suite!(memory, (MemoryShelf::default(), ()));
shelf_suite!(fs, fs_shelf());

// ---------- what only a filesystem can get wrong ----------

#[cfg(unix)]
#[test]
fn files_are_the_owners_alone_and_lie_where_the_layout_says() {
    use std::os::unix::fs::PermissionsExt;
    let (shelf, dir) = fs_shelf();
    let root = dir.path().join("ws");
    let upload = stage(&shelf, &mut SeededRandom::new(1), b"secret");
    let staged = root.join("staging").join(upload.as_str()).join("content");
    assert_eq!(std::fs::read(&staged).unwrap(), b"secret");

    let a = id("00000001-aaaaaaaaaaaa");
    assert!(shelf.publish(&upload, 4, &a, envelope()).unwrap());
    let item = root.join("gen-4").join("items").join(a.as_str());
    // Byte for byte what the client sent, so an export is a copy.
    assert_eq!(std::fs::read(item.join("content")).unwrap(), b"secret");
    assert_eq!(
        std::fs::read(item.join("meta.json")).unwrap(),
        envelope().meta
    );
    let server = std::fs::read_to_string(item.join("server.json")).unwrap();
    assert!(
        server.contains("\"receivedAt\":1003") && server.contains("\"under\":\"aa\""),
        "{server}"
    );

    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    for file in ["content", "meta.json", "server.json"] {
        assert_eq!(mode(&item.join(file)), 0o600, "{file}");
    }
    for folder in [root.clone(), root.join("staging"), root.join("gen-4"), item] {
        assert_eq!(mode(&folder), 0o700, "{}", folder.display());
    }
}

#[test]
fn of_two_publishes_racing_for_one_id_exactly_one_wins() {
    for round in 0..20 {
        let (shelf, _dir) = fs_shelf();
        let shelf = std::sync::Arc::new(shelf);
        let a = id("00000001-aaaaaaaaaaaa");
        let mut rng = SeededRandom::new(round);
        let uploads = [
            stage(&*shelf, &mut rng, b"first"),
            stage(&*shelf, &mut rng, b"second"),
        ];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let racers: Vec<_> = uploads
            .iter()
            .cloned()
            .map(|upload| {
                let (shelf, barrier, a) = (shelf.clone(), barrier.clone(), a.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    shelf.publish(&upload, 0, &a, envelope()).unwrap()
                })
            })
            .collect();
        let won: Vec<bool> = racers
            .into_iter()
            .map(|racer| racer.join().unwrap())
            .collect();
        assert_eq!(
            won.iter().filter(|won| **won).count(),
            1,
            "round {round}: {won:?}"
        );
        // The winner's content is whole, and the loser's staging survives.
        let content = content_of(&*shelf, 0, &a);
        assert!(content == b"first" || content == b"second");
        assert_eq!(shelf.staged().unwrap().len(), 1);
        assert_eq!(shelf.ids(0).unwrap(), vec![a]);
    }
}

#[test]
fn an_empty_folder_where_an_item_goes_is_not_an_item() {
    // `rename` replaces an empty directory. No code path makes one under
    // `items/`, but if something else does, it must neither block a publish
    // nor be listed.
    let (shelf, dir) = fs_shelf();
    let a = id("00000001-aaaaaaaaaaaa");
    let planted = dir.path().join("ws/gen-0/items").join(a.as_str());
    std::fs::create_dir_all(&planted).unwrap();
    assert!(shelf.ids(0).unwrap().is_empty());
    assert!(shelf.get(0, &a).unwrap().is_none());
    let upload = stage(&shelf, &mut SeededRandom::new(2), b"real");
    assert!(shelf.publish(&upload, 0, &a, envelope()).unwrap());
    assert_eq!(content_of(&shelf, 0, &a), b"real");
}

#[test]
fn what_is_not_an_id_is_not_listed_and_a_reopened_shelf_finds_everything() {
    let (shelf, dir) = fs_shelf();
    let a = id("00000001-aaaaaaaaaaaa");
    let upload = stage(&shelf, &mut SeededRandom::new(3), b"kept");
    shelf.publish(&upload, 2, &a, envelope()).unwrap();
    let pending = stage(&shelf, &mut SeededRandom::new(4), b"pending");
    std::fs::create_dir_all(dir.path().join("ws/gen-2/items/not-an-id")).unwrap();
    std::fs::create_dir_all(dir.path().join("ws/staging/.DS_Store")).unwrap();
    std::fs::create_dir_all(dir.path().join("ws/gen-x")).unwrap();
    drop(shelf);

    let shelf = FsShelf::open(dir.path().join("ws")).unwrap();
    assert_eq!(shelf.ids(2).unwrap(), vec![a.clone()]);
    assert_eq!(shelf.generations().unwrap(), vec![2]);
    assert_eq!(shelf.staged().unwrap(), vec![pending]);
    assert_eq!(shelf.get(2, &a).unwrap().unwrap().envelope, envelope());
}

#[test]
fn an_item_disappears_in_one_step_and_the_rubbish_goes_at_the_next_opening() {
    let (shelf, dir) = fs_shelf();
    let a = id("00000001-aaaaaaaaaaaa");
    let upload = stage(&shelf, &mut SeededRandom::new(5), b"gone");
    shelf.publish(&upload, 0, &a, envelope()).unwrap();
    // As if a removal had been cut short after its rename.
    let trash = dir.path().join("ws/trash");
    std::fs::create_dir_all(trash.join("left-behind")).unwrap();
    std::fs::write(trash.join("left-behind/content"), b"x").unwrap();
    drop(shelf);
    let shelf = FsShelf::open(dir.path().join("ws")).unwrap();
    assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 0);
    assert_eq!(shelf.remove(0, &a).unwrap().unwrap().size, 4);
    assert!(shelf.ids(0).unwrap().is_empty());
    assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 0);
}
