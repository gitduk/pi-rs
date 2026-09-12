//! Bounded capture for the streams a tool pulls in — a child's stdout, a
//! response body. Memory stays capped whatever the producer does: what
//! overflows the window streams to disk, where `read` can get it back.

use std::io::Write as _;
use std::path::Path;

use brain::slice::{head_bytes, tail_bytes};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::spill::{self, SpillRef};
use crate::{Ctx, ToolError};

/// How much of a spilled stream the view keeps of each end — the same two
/// halves [`spill::prune`] shows of a body it holds whole.
const VIEW: usize = spill::MAX_OUTPUT / 2;

/// The one-call form of [`Capture`]: read `reader` dry under the cap, return
/// the bounded view.
pub async fn take<R: AsyncRead + Unpin>(mut reader: R, ctx: &Ctx) -> Result<Captured, ToolError> {
    let mut cap = Capture::new();
    cap.drain(&mut reader, ctx).await?;
    Ok(cap.finish())
}

/// What a finished capture leaves: a transcript-sized view of the stream, how
/// many bytes it was, and where the whole of it lives once it spilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub total: usize,
    /// The whole stream when it fits the window; head, an omitted marker and
    /// tail when it does not. Bounded either way — safe to interpolate.
    pub text: String,
    pub spill: Option<SpillRef>,
}

/// A sink that absorbs a stream without ever holding more than the window's
/// worth of it. Feed it with [`Capture::drain`], read it back with
/// [`Capture::finish`].
#[derive(Default)]
pub struct Capture {
    /// Every byte up to the threshold, kept whole so a stream that ends under
    /// it reaches the transcript in full; past the crossing, the view's head.
    front: Vec<u8>,
    /// The last `VIEW` bytes of a stream that crossed the threshold.
    tail: Vec<u8>,
    /// The spill handle and the locator it answers to, set together or not.
    file: Option<(std::fs::File, String)>,
    total: usize,
}

impl Capture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pull `reader` dry. Crossing the threshold opens the spill file, dumps
    /// what is held into it and streams the rest straight there.
    pub async fn drain<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        ctx: &Ctx,
    ) -> Result<(), ToolError> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = reader.read(&mut buf).await?;
            if n == 0 {
                return Ok(());
            }
            self.absorb(&buf[..n], ctx)?;
        }
    }

    fn absorb(&mut self, chunk: &[u8], ctx: &Ctx) -> Result<(), ToolError> {
        self.total += chunk.len();
        if let Some((file, _)) = self.file.as_mut() {
            file.write_all(chunk)?;
            self.keep_tail(chunk);
            return Ok(());
        }
        if self.front.len() + chunk.len() <= spill::MAX_OUTPUT {
            self.front.extend_from_slice(chunk);
            return Ok(());
        }
        // The crossing: the bytes held so far open the file, and everything
        // from here on — the rest of this chunk included — lands there.
        let room = spill::MAX_OUTPUT - self.front.len();
        self.front.extend_from_slice(&chunk[..room]);
        let rest = &chunk[room..];
        let (path, locator) = spill::allocate(ctx)?;
        let mut file = open_spill(&path)?;
        file.write_all(&self.front)?;
        file.write_all(rest)?;
        self.file = Some((file, locator));
        self.keep_tail(rest);
        Ok(())
    }

    fn keep_tail(&mut self, chunk: &[u8]) {
        self.tail.extend_from_slice(chunk);
        let over = self.tail.len().saturating_sub(VIEW);
        if over > 0 {
            self.tail.drain(..over);
        }
    }

    /// Bound the view and name the spill; the file is already fully written.
    pub fn finish(self) -> Captured {
        let Self {
            front,
            tail,
            file,
            total,
        } = self;
        let Some((_, locator)) = file else {
            return Captured {
                total,
                text: String::from_utf8_lossy(&front).into_owned(),
                spill: None,
            };
        };
        let front = String::from_utf8_lossy(&front);
        let tail = String::from_utf8_lossy(&tail);
        let head = head_bytes(&front, VIEW);
        let tail = tail_bytes(&tail, VIEW);
        Captured {
            total,
            text: spill::elided(head, total, tail),
            spill: Some(SpillRef {
                bytes: total,
                locator,
            }),
        }
    }
}

// The file grows as the stream arrives, so the tmp-and-rename dance of
// `state::write_private` does not apply: a killed run leaves no locator behind.
fn open_spill(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

#[cfg(test)]
mod tests {
    use crate::{Ctx, Workspace};

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx::new(Workspace::new(dir).unwrap()).with_spill_root(dir.join("spill"))
    }

    #[tokio::test]
    async fn a_small_stream_stays_whole_and_unspilled() {
        let dir = tempfile::tempdir().unwrap();
        let got = super::take(&b"hi there"[..], &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(got.text, "hi there");
        assert_eq!(got.total, 8);
        assert!(got.spill.is_none());
    }

    #[tokio::test]
    async fn a_stream_at_the_threshold_is_whole() {
        let dir = tempfile::tempdir().unwrap();
        let body = "x".repeat(crate::spill::MAX_OUTPUT);
        let got = super::take(body.as_bytes(), &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(got.text, body);
        assert!(got.spill.is_none());
    }

    #[tokio::test]
    async fn a_stream_past_the_threshold_shows_both_ends_and_keeps_the_whole() {
        let dir = tempfile::tempdir().unwrap();
        let c = ctx(dir.path());
        let body = "x".repeat(crate::spill::MAX_OUTPUT + 5_000);
        let got = super::take(body.as_bytes(), &c).await.unwrap();

        assert_eq!(got.total, body.len());
        let spill = got.spill.as_ref().expect("past the threshold must spill");
        assert_eq!(spill.bytes, body.len());
        assert!(got.text.starts_with("xxx"), "{got:?}");
        assert!(got.text.contains("bytes omitted"), "{got:?}");
        assert!(got.text.ends_with("xxx"), "{got:?}");

        // The file holds every byte, not just the view.
        let back = std::fs::read(c.spill_path(&spill.locator).unwrap()).unwrap();
        assert_eq!(back.len(), body.len());
    }

    #[tokio::test]
    async fn bytes_that_are_not_utf8_reach_the_view_as_replacements() {
        let dir = tempfile::tempdir().unwrap();
        let got = super::take(&[b'a', 0xff, b'b'][..], &ctx(dir.path()))
            .await
            .unwrap();
        assert_eq!(got.text, "a\u{fffd}b");
    }
}
