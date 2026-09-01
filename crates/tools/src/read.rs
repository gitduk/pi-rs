use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use hashline::tag;

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput, spill};

const DEFAULT_LIMIT: usize = 2_000;
const MAX_LINE: usize = 2_000;
const BINARY_SNIFF: usize = 8_000;
const MAX_BYTES: u64 = 10 << 20;
const OUTLINE_OVER: usize = 300;

#[derive(Deserialize)]
struct Args {
    path: String,
    /// 1-based first line to return.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    /// Force the skeleton on, or off for a file long enough to trigger it.
    #[serde(default)]
    outline: Option<bool>,
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0)
}

/// A numbered view, held as rows until the transcript's budget is spent on it.
///
/// The rows stay apart from the text they will become for two reasons that are
/// really one: the budget is then spent in whole rows, so no row is cut through
/// the middle and left carrying a line number it no longer holds; and the line
/// naming what survived is read off the same decision that built the body.
/// Naming it from what the caller *meant* to send is how it came to name rows
/// the model was never shown.
struct View {
    /// The `[path#TAG]` line the model anchors a patch to.
    head: String,
    /// Each row as it prints — newline and all — beside the file line it holds.
    rows: Vec<(usize, String)>,
    /// What follows the rows: how much of the file is still unread.
    note: String,
    kind: Kind,
}

/// What the view is, which is what decides how it names itself. One field, not
/// a `whole` flag beside an `outline` option: a skeleton is never a run of
/// lines, and two flags can say it is.
enum Kind {
    /// A run of file lines; `whole` when they reached both ends of the file.
    Lines { whole: bool },
    /// A skeleton, and the length of the file it skips through.
    Outline { lines: usize },
}

/// The rows a view keeps at each end when all of them will not fit. `None` when
/// they do — the one value both the body and its name are read from.
type Cut = Option<(usize, usize)>;

impl View {
    /// The view as the model reads it: every row, or the ends of them with
    /// `cut`'s middle elided. One assembly, so the spill copy and the
    /// transcript copy cannot come to disagree about anything but the middle.
    fn text(&self, cut: Cut) -> String {
        let total: usize = self.rows.iter().map(|(_, r)| r.len()).sum();
        let mut out = String::with_capacity(self.head.len() + 1 + total + self.note.len());
        out.push_str(&self.head);
        out.push('\n');
        let kept: Vec<&[(usize, String)]> = match cut {
            None => vec![&self.rows],
            Some((head, tail)) => vec![&self.rows[..head], &self.rows[self.rows.len() - tail..]],
        };
        for (i, span) in kept.iter().enumerate() {
            if i > 0 {
                out.push_str(crate::rows::GAP);
            }
            for (_, row) in *span {
                out.push_str(row);
            }
        }
        out.push_str(&self.note);
        out
    }

    // Whole rows from each end until the budget is gone. The alternative —
    // cutting the assembled text at a byte offset — lands mid-row about as
    // often as not, and half a line under a line number reads as content.
    fn cut(&self) -> Cut {
        let spent = self.head.len() + self.note.len() + crate::rows::GAP.len();
        let room = spill::MAX_OUTPUT.saturating_sub(spent);
        let total: usize = self.rows.iter().map(|(_, r)| r.len()).sum();
        if total <= room {
            return None;
        }
        let size = |(_, r): &&(usize, String)| r.len();
        let head = crate::rows::fits(self.rows.iter(), size, room / 2);
        let tail = crate::rows::fits(self.rows.iter().rev(), size, room / 2);
        Some((head, tail.min(self.rows.len() - head)))
    }

    /// The one line a person sees, named from the rows the body actually holds.
    fn shown(&self, rel: &str, cut: Cut) -> String {
        if let Kind::Outline { lines } = self.kind {
            let dropped = cut.map_or(String::new(), |(h, t)| {
                format!(" · {} not shown", self.rows.len() - h - t)
            });
            return format!("[{rel}] {lines} lines · outline{dropped}");
        }
        if cut.is_none() && matches!(self.kind, Kind::Lines { whole: true }) {
            return format!("[{rel}]");
        }
        let at = |i: usize| self.rows[i].0;
        let last = self.rows.len() - 1;
        let spans = match cut {
            None => vec![(at(0), at(last))],
            Some((h, t)) => vec![(at(0), at(h - 1)), (at(self.rows.len() - t), at(last))],
        };
        // Through `hashline`, which is the crate that reads addresses back: a
        // single row is `N`, and `N-N` is a shape its own parser refuses.
        let named: Vec<String> = spans
            .iter()
            .map(|(a, b)| hashline::Target::Range { start: *a, end: *b }.to_string())
            .collect();
        format!("[{rel}:{}]", named.join(crate::rows::GAP.trim_end()))
    }
}

// Deliver the view: the model's copy, the whole of it spilled when it is too
// long for the transcript, and the one line a person sees.
//
// Both halves from here, because they used to be spelt at each return and a
// read has several: the tag belongs to one of them and kept turning up in the
// other. What the model reads carries it — a patch names it and has nowhere
// else to get it — and what a person reads never does.
fn deliver(ctx: &Ctx, rel: &str, view: View) -> Result<ToolOutput, ToolError> {
    // One length check decides both halves. Two — the transcript's budget and
    // the spill threshold, each read off a differently assembled string — can
    // disagree at the margin, and the margin is where rows get elided behind
    // `…` with no locator to recover them from.
    let full = view.text(None);
    let Some(spilled) = spill::write(ctx, &full)? else {
        return Ok(ToolOutput::text(full).with_preview(view.shown(rel, None)));
    };
    let cut = view.cut();
    let mut body = view.text(cut);
    body.push('\n');
    body.push_str(&spilled.note());
    Ok(ToolOutput::text(body).with_preview(view.shown(rel, cut)))
}

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file as numbered lines, or list a directory. Output is headed by \
         [path#TAG]; the TAG is required by later edits and goes stale when the \
         file changes. A long file comes back as a skeleton of its declarations \
         instead — read a range with offset and limit, or replace one whole \
         construct with edit's `PUT N*:`."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path, or an absolute path anywhere readable." },
                "offset": { "type": "integer", "description": "1-based first line. Default 1." },
                "limit": { "type": "integer", "description": "Max lines. Default 2000." },
                "outline": {
                    "type": "boolean",
                    "description": "Return the file's declarations instead of its lines. \
                                    Applied automatically to long files unless offset or \
                                    limit is given.",
                },
            },
            "required": ["path"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args(args)?;
        // A `spill:` path names a file in the session's spill directory. Only
        // locators our own writer mints resolve; anything else is refused
        // before the filesystem is touched.
        let is_spill = args.path.starts_with("spill:");
        let (path, rel) = match args.path.strip_prefix("spill:") {
            Some(_) => {
                let path = ctx.spill_path(&args.path)?;
                (path, args.path.clone())
            }
            None => {
                let p = ctx.workspace.resolve(&args.path, self.tier())?;
                let rel = ctx.workspace.display(&p);
                (p, rel)
            }
        };
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| ToolError::Invalid(format!("{rel}: {e}")))?;

        if meta.is_dir() {
            let mut entries = tokio::fs::read_dir(&path).await?;
            let mut names = Vec::new();
            while let Some(e) = entries.next_entry().await? {
                let suffix = if e.file_type().await.is_ok_and(|t| t.is_dir()) {
                    "/"
                } else {
                    ""
                };
                names.push(format!("{}{suffix}", e.file_name().to_string_lossy()));
            }
            names.sort();
            if names.is_empty() {
                return Ok(ToolOutput::useless(format!("{rel}/ is empty")));
            }
            return Ok(ToolOutput::text(format!("{rel}/\n{}", names.join("\n"))));
        }

        // Sniffing needs the whole file in memory, so the guard precedes the
        // read. A spill locator names a file this very session wrote — the
        // model asked for it by locator, and the retrieval hint promised read
        // would serve it — so the cap does not apply there.
        if meta.len() > MAX_BYTES && !is_spill {
            return Ok(ToolOutput::useless(format!(
                "{rel} is {} bytes, over the {MAX_BYTES}-byte read limit; use bash to slice it",
                meta.len()
            )));
        }
        let bytes = tokio::fs::read(&path).await?;
        if looks_binary(&bytes) {
            return Ok(ToolOutput::useless(format!(
                "{rel} is binary ({} bytes); read is for text",
                meta.len()
            )));
        }
        // Lossy decoding would hand back a wall of U+FFFD, and a model that
        // writes any of it back corrupts the file for real. grep can afford
        // lossy — a matching line is still a match — but read cannot.
        let content = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolOutput::useless(format!(
                    "{rel} is not valid UTF-8 (byte {} is invalid). If it is text in \
                     another encoding, convert it with bash first.",
                    e.valid_up_to()
                )));
            }
        };
        let tag = tag(content);
        // Not in the view any more, so recorded here: which version of a file
        // the model was looking at is the whole story when an edit built on
        // this read turns out to have addressed the wrong lines.
        tracing::info!(target: "pi::read", path = %rel, tag = %tag, "read");
        // The numbering about to be shown is the current one. A window read
        // clears the whole file rather than its own rows: the case worth
        // catching is an edit built with no read between it and the last one.
        ctx.forget_shift(&path);

        let all: Vec<&str> = content.lines().collect();

        // A range request is an explicit ask for lines; only an unqualified read
        // of a long file is worth answering with a skeleton.
        let ranged = args.offset.is_some() || args.limit.is_some();
        let wants_outline = args.outline.unwrap_or(!ranged && all.len() > OUTLINE_OVER);
        if wants_outline && let Some(lang) = syntax::Lang::of(&rel) {
            let items = syntax::outline(lang, content);
            if !items.is_empty() {
                // From the items already in hand: asking `rows::spans` here
                // would parse the file a second time for the same answer.
                let spans = crate::rows::of(&items);
                let rows = items
                    .iter()
                    .map(|item| {
                        // The span, not just the opening row: it is what an edit
                        // names, and a skeleton is the only view of a long file
                        // that shows where anything ends.
                        let mut row = crate::rows::addr(item.line, &spans);
                        for _ in 0..item.depth {
                            row.push_str("  ");
                        }
                        row.push_str(&item.text);
                        row.push('\n');
                        (item.line, row)
                    })
                    .collect();
                return deliver(
                    ctx,
                    &rel,
                    View {
                        head: format!(
                            "{} {} lines · outline",
                            hashline::header(&rel, &tag),
                            all.len()
                        ),
                        rows,
                        note: "… declarations only, each with the range that replaces it \
                               whole. Read a range with offset and limit.\n"
                            .into(),
                        kind: Kind::Outline { lines: all.len() },
                    },
                );
            }
        }

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let start = offset - 1;

        if start >= all.len() {
            return Ok(ToolOutput::useless(format!(
                "[{rel}] line {offset} is past the end ({} lines)",
                all.len()
            )));
        }

        let end = (start + limit).min(all.len());
        // A construct opening inside the window often closes outside it, and a
        // row that says where it ends is the difference between one read and
        // two.
        let spans = crate::rows::spans(&rel, content);
        let rows = all[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let n = start + i + 1;
                let mut row = String::new();
                if line.len() > MAX_LINE {
                    row.push_str(&crate::rows::addr(n, &spans));
                    row.push_str(brain::slice::head_bytes(line, MAX_LINE));
                    row.push_str("… (line truncated)\n");
                } else {
                    crate::rows::line(&mut row, n, &spans, line);
                }
                (n, row)
            })
            .collect();
        let left = all.len() - end;
        let note = if left > 0 {
            let unit = if left == 1 { "line" } else { "lines" };
            format!("… {left} more {unit}; re-read from {}\n", end + 1)
        } else {
            String::new()
        };
        deliver(
            ctx,
            &rel,
            View {
                head: hashline::header(&rel, &tag),
                rows,
                note,
                // Both ends of the file reached. Whether a range was asked for
                // does not come into it: what is named is what came back.
                kind: Kind::Lines {
                    whole: start == 0 && end == all.len(),
                },
            },
        )
    }
}
