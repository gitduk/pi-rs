//! Bounded capture for the streams a tool pulls in — a child's stdout, a
//! response body. What overflows the window streams to disk; `read` gets it
//! back. Sweeps that assemble output as items share [`Budget`]; what reaches
//! the transcript is what [`bound`] lets through.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use llm::message::ToolResultContent;
use llm::slice::{head_bytes, tail_bytes};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::fold::fold_repeats;
use crate::spill::{self, SpillRef};
use crate::{Ctx, Tool, ToolError, ToolOutput};

/// How much of a spilled stream the view keeps of each end — the same two
/// halves [`spill::prune`] shows of a body it holds whole.
const VIEW: usize = spill::MAX_OUTPUT / 2;

/// What a finished capture leaves: a transcript-sized view of the stream, how
/// many bytes it was, and where the whole of it lives once it spilled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub total: usize,
    /// The whole stream when it fits the window; head, an omitted marker and
    /// tail when it does not. Bounded either way — safe to interpolate. Runs
    /// printed again verbatim are folded to a line naming the first copy.
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
                text: fold_repeats(&String::from_utf8_lossy(&front)),
                spill: None,
            };
        };
        let front = String::from_utf8_lossy(&front);
        let tail = String::from_utf8_lossy(&tail);
        let head = head_bytes(&front, VIEW);
        let tail = tail_bytes(&tail, VIEW);
        Captured {
            total,
            text: fold_repeats(&spill::elided(head, total, tail)),
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

/// Kept bytes one sweep of output assembly may hold — a grep's matches, a
/// glob's paths, a directory's entries. The budget is what ends the sweep;
/// [`bound`] is what bounds the view.
pub const SWEEP_BUDGET: usize = 4 << 20;

/// Accounts kept bytes for one sweep. Cheap to clone, so one budget can
/// cover a parallel walk's threads.
#[derive(Clone)]
pub struct Budget(Arc<AtomicUsize>);

impl Budget {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Account `bytes` as kept; `false` means the budget is spent and the
    /// item should be dropped, counted by the caller.
    pub fn admits(&self, bytes: usize) -> bool {
        self.0.fetch_add(bytes, Ordering::Relaxed) < SWEEP_BUDGET
    }

    /// Whether the budget is spent.
    pub fn spent(&self) -> bool {
        self.0.load(Ordering::Relaxed) >= SWEEP_BUDGET
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self::new()
    }
}

/// The interface's own gate: whatever a tool returns, over-window text is
/// spilled whole and shown as head plus locator. A tool that bounds itself
/// tightly never notices it; a tool that forgets cannot flood the
/// transcript.
pub fn bound(mut out: ToolOutput, ctx: &Ctx) -> ToolOutput {
    for piece in &mut out.content {
        // Structured blocks are text to the model too; an oversized one is
        // bounded as the text it would be sent as.
        if let ToolResultContent::Json { value } = piece {
            let text = value.to_string();
            if text.len() <= spill::MAX_OUTPUT {
                continue;
            }
            *piece = ToolResultContent::Text(llm::message::Text { text });
        }
        if let ToolResultContent::Text(t) = piece {
            t.text = bound_text(std::mem::take(&mut t.text), ctx);
        }
    }
    out
}

fn bound_text(text: String, ctx: &Ctx) -> String {
    match spill::write(ctx, &text) {
        Ok(None) => text,
        Ok(Some(spilled)) => {
            let head = head_bytes(&text, VIEW);
            let left = text.len() - head.len();
            format!("{head}\n… {left} more bytes; {}\n", spilled.note())
        }
        // Spilling failed and the gate still must not flood: say plainly
        // that the tail is gone instead of a clean-looking prefix.
        Err(_) => {
            let head = head_bytes(&text, VIEW);
            format!(
                "{head}\n… {} more bytes truncated — spill failed\n",
                text.len() - head.len()
            )
        }
    }
}

/// Run `tool` behind the gate: its output bounded, and so is the prose of a
/// refusal, which reaches the model just the same.
pub async fn gated(tool: &dyn Tool, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
    match tool.execute(args, ctx).await {
        Ok(out) => Ok(bound(out, ctx)),
        Err(ToolError::Invalid(why)) => Err(ToolError::Invalid(bound_text(why, ctx))),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ctx, Workspace};

    fn ctx(dir: &std::path::Path) -> Ctx {
        Ctx::new(Workspace::new(dir).unwrap()).with_spill_root(dir.join("spill"))
    }

    async fn take(mut reader: &[u8], ctx: &Ctx) -> Result<super::Captured, crate::ToolError> {
        let mut cap = super::Capture::new();
        cap.drain(&mut reader, ctx).await?;
        Ok(cap.finish())
    }

    // Below the cap, kept whole; at the cap, still whole; past it, the view
    // shows both ends but the spill file holds every byte, not just the view.
    #[tokio::test]
    async fn the_spill_threshold_keeps_small_streams_whole_and_spills_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let got = take(&b"hi there"[..], &ctx(dir.path())).await.unwrap();
        assert_eq!(got.text, "hi there");
        assert!(got.spill.is_none());

        let body = "x".repeat(crate::spill::MAX_OUTPUT);
        let got = take(body.as_bytes(), &ctx(dir.path())).await.unwrap();
        assert_eq!(got.text, body);
        assert!(got.spill.is_none());

        let c = ctx(dir.path());
        let body = "x".repeat(crate::spill::MAX_OUTPUT + 5_000);
        let got = take(body.as_bytes(), &c).await.unwrap();
        assert_eq!(got.total, body.len());
        let spill = got.spill.as_ref().expect("past the threshold must spill");
        assert_eq!(spill.bytes, body.len());
        assert!(got.text.contains("bytes omitted"), "{got:?}");
        let back = std::fs::read(c.spill_path(&spill.locator).unwrap()).unwrap();
        assert_eq!(back.len(), body.len());
    }

    // What fits passes straight through, untouched.
    #[tokio::test]
    async fn bound_spills_what_overflows_and_leaves_the_locator() {
        let dir = tempfile::tempdir().unwrap();
        let huge = "x".repeat(super::spill::MAX_OUTPUT + 1_000);
        let got = super::bound(super::ToolOutput::text(huge), &ctx(dir.path()));
        let text = got.flatten();
        assert!(text.contains("full output:"), "{text}");
        assert!(text.len() < 40_000, "{}", text.len());

        let small = super::bound(super::ToolOutput::text("tiny"), &ctx(dir.path()));
        assert_eq!(small.flatten(), "tiny");
    }

    // A structured block and a refusal reach the model as text, so neither
    // may slip past the gate at any size.
    #[tokio::test]
    async fn the_gate_bounds_json_blocks_and_refusals_too() {
        struct Refuses;
        #[async_trait::async_trait]
        impl crate::Tool for Refuses {
            fn name(&self) -> &str {
                "refuses"
            }
            fn description(&self) -> &str {
                ""
            }
            fn schema(&self) -> Value {
                Value::Null
            }
            fn tier(&self) -> crate::Tier {
                crate::Tier::Read
            }
            async fn execute(&self, _: Value, _: &Ctx) -> Result<ToolOutput, ToolError> {
                Err(ToolError::Invalid("x".repeat(spill::MAX_OUTPUT + 1_000)))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());

        let big = Value::String("y".repeat(spill::MAX_OUTPUT + 1_000));
        let out = ToolOutput {
            content: vec![ToolResultContent::Json { value: big }],
            ..ToolOutput::text("")
        };
        let text = bound(out, &ctx).flatten();
        assert!(text.contains("full output:"), "{text}");
        assert!(text.len() < 40_000, "{}", text.len());

        let Err(ToolError::Invalid(why)) = gated(&Refuses, Value::Null, &ctx).await else {
            panic!("a refusal stays a refusal");
        };
        assert!(why.contains("full output:"), "{why}");
        assert!(why.len() < 40_000, "{}", why.len());
    }

    #[test]
    fn a_budget_admits_until_spent_then_says_drop() {
        let b = super::Budget::new();
        assert!(b.admits(super::SWEEP_BUDGET));
        assert!(b.spent());
        assert!(!b.admits(1));
    }
}
