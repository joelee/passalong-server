//! Between asynchronous bodies and the synchronous core.
//!
//! The core reads content from a `std::io::Read` and hands it out as one,
//! on the blocking pool. HTTP bodies arrive and leave as asynchronous
//! frames. In both directions the two are joined by a bounded channel of
//! pieces of at most [`PIECE`] bytes, so that what is in flight is at most
//! [`DEPTH`] pieces, whatever the item's size, and a slow client slows only
//! its own transfer: the channel fills, and the side that is ahead waits.

use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use tokio::sync::mpsc;

/// The largest piece that crosses the bridge.
pub const PIECE: usize = 64 * 1024;
/// How many pieces may wait in the channel.
pub const DEPTH: usize = 8;

/// What the bridge holds, and held at most. The test of 64 MiB asserts the
/// peak, which is the bound this module promises.
#[derive(Debug, Default)]
pub struct BridgeStats {
    held: AtomicUsize,
    peak: AtomicUsize,
}

impl BridgeStats {
    fn took(&self, bytes: usize) {
        let held = self.held.fetch_add(bytes, Ordering::SeqCst) + bytes;
        self.peak.fetch_max(held, Ordering::SeqCst);
    }

    fn gave(&self, bytes: usize) {
        self.held.fetch_sub(bytes, Ordering::SeqCst);
    }

    /// The most bytes that were ever in flight at once, over all transfers.
    pub fn peak_bytes(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

type Piece = Result<Bytes, std::io::Error>;

/// A request's body, as the shelf reads it. Reading blocks, so it belongs
/// on the blocking pool.
pub struct BodyReader {
    pieces: mpsc::Receiver<Piece>,
    current: Bytes,
    stats: Arc<BridgeStats>,
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.current.is_empty() {
            match self.pieces.blocking_recv() {
                Some(Ok(piece)) => self.current = piece,
                // The client went away, or sent what is not HTTP: the shelf
                // empties the staging place, and the upload can be repeated.
                Some(Err(err)) => return Err(err),
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.current.len());
        buf[..n].copy_from_slice(&self.current.split_to(n));
        self.stats.gave(n);
        Ok(n)
    }
}

/// Starts pumping `body` into the reader it returns.
pub fn read_body(mut body: Body, stats: Arc<BridgeStats>) -> BodyReader {
    let (sender, pieces) = mpsc::channel::<Piece>(DEPTH);
    let counted = stats.clone();
    tokio::spawn(async move {
        loop {
            let frame = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await;
            let mut data = match frame {
                None => return,
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => data,
                    Err(_trailers) => continue,
                },
                Some(Err(err)) => {
                    let _ = sender
                        .send(Err(std::io::Error::other(err.to_string())))
                        .await;
                    return;
                }
            };
            while !data.is_empty() {
                let piece = data.split_to(data.len().min(PIECE));
                counted.took(piece.len());
                if sender.send(Ok(piece)).await.is_err() {
                    // The reader is gone: the shelf refused the content.
                    return;
                }
            }
        }
    });
    BodyReader {
        pieces,
        current: Bytes::new(),
        stats,
    }
}

/// A response body fed from a reader on the blocking pool.
pub struct ReaderBody {
    pieces: mpsc::Receiver<Piece>,
    remaining: u64,
    stats: Arc<BridgeStats>,
}

impl HttpBody for ReaderBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.pieces.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(err))),
            Poll::Ready(Some(Ok(piece))) => {
                self.stats.gave(piece.len());
                self.remaining = self.remaining.saturating_sub(piece.len() as u64);
                Poll::Ready(Some(Ok(Frame::data(piece))))
            }
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

/// Streams exactly `length` bytes of `content` as a body.
pub fn stream_out(mut content: Box<dyn Read + Send>, length: u64, stats: Arc<BridgeStats>) -> Body {
    let (sender, pieces) = mpsc::channel::<Piece>(DEPTH);
    let counted = stats.clone();
    tokio::task::spawn_blocking(move || {
        let mut left = length;
        let mut piece = vec![0_u8; PIECE];
        while left > 0 {
            let want = usize::try_from(left).unwrap_or(PIECE).min(PIECE);
            let sent = match content.read(&mut piece[..want]) {
                // Shorter than its size said: the item changed under us.
                Ok(0) => {
                    sender.blocking_send(Err(std::io::Error::other("the content ended early")))
                }
                Ok(n) => {
                    left -= n as u64;
                    counted.took(n);
                    sender.blocking_send(Ok(Bytes::copy_from_slice(&piece[..n])))
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => sender.blocking_send(Err(err)),
            };
            if sent.is_err() {
                // The client went away; nothing is owed to it.
                return;
            }
        }
    });
    Body::new(ReaderBody {
        pieces,
        remaining: length,
        stats,
    })
}

/// One range of `bytes=…`, as inclusive offsets into content of `size`
/// bytes; `None` for the whole content, which is also the answer to several
/// ranges and to other units. `Err` when the range lies beyond the content.
pub fn one_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.and_then(|value| value.trim().strip_prefix("bytes=")) else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Ok(None);
    };
    let range = match (first.trim().parse::<u64>(), last.trim().parse::<u64>()) {
        (Ok(first), Ok(last)) if first <= last => (first, last.min(size.saturating_sub(1))),
        (Ok(first), Err(_)) if last.trim().is_empty() => (first, size.saturating_sub(1)),
        // The last `n` bytes.
        (Err(_), Ok(n)) if first.trim().is_empty() && n > 0 => {
            (size.saturating_sub(n), size.saturating_sub(1))
        }
        _ => return Ok(None),
    };
    if size == 0 || range.0 >= size {
        return Err(());
    }
    Ok(Some(range))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_range_is_understood_and_anything_else_means_everything() {
        assert_eq!(one_range(None, 10), Ok(None));
        assert_eq!(one_range(Some("bytes=0-3"), 10), Ok(Some((0, 3))));
        assert_eq!(one_range(Some("bytes=7-"), 10), Ok(Some((7, 9))));
        assert_eq!(one_range(Some("bytes=-2"), 10), Ok(Some((8, 9))));
        assert_eq!(one_range(Some("bytes=-99"), 10), Ok(Some((0, 9))));
        assert_eq!(one_range(Some("bytes=8-99"), 10), Ok(Some((8, 9))));
        assert_eq!(one_range(Some(" bytes=2-2 "), 10), Ok(Some((2, 2))));
        for whole in [
            "bytes=0-1,4-5",
            "lines=1-2",
            "bytes=x-y",
            "bytes=5-2",
            "bytes=-0",
            "bytes=",
            "bytes=-",
        ] {
            assert_eq!(one_range(Some(whole), 10), Ok(None), "{whole}");
        }
        assert_eq!(one_range(Some("bytes=10-"), 10), Err(()));
        assert_eq!(one_range(Some("bytes=0-"), 0), Err(()));
    }

    #[test]
    fn the_stats_remember_the_most_that_was_held() {
        let stats = BridgeStats::default();
        stats.took(10);
        stats.took(5);
        stats.gave(12);
        stats.took(1);
        assert_eq!(stats.peak_bytes(), 15);
    }
}
