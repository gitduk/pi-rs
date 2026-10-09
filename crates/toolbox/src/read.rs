use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::rows::{header, view_hash};

use tool::limit::{MAX_BYTES, over_limit};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output, spill};

const DEFAULT_LIMIT: usize = 2_000;
pub(crate) const MAX_LINE: usize = 2_000;
const BINARY_SNIFF: usize = 8_000;
const OUTLINE_OVER: usize = 300;

#[derive(Deserialize)]
struct Args {
    path: String,
    // 1-based first line to return.
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    // Force the skeleton on, or off for a file long enough to trigger it.
    #[serde(default)]
    outline: Option<bool>,
}

fn image(rel: &str, media_type: &str, bytes: &[u8]) -> ToolOutput {
    use llm::message::{Text, ToolResultContent};
    let image = match llm::message::Image::from_bytes(bytes) {
        Ok(image) => image,
        Err(why) => {
            return ToolOutput::text(format!(
                "{rel} is {why}; shrink it with bash and read the smaller copy"
            ));
        }
    };
    ToolOutput {
        content: vec![
            ToolResultContent::Text(Text {
                text: format!("[{rel}] {media_type}, {} bytes", bytes.len()),
            }),
            ToolResultContent::Image(image),
        ],
        preview: None,
        spent: Default::default(),
    }
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF).any(|b| *b == 0)
}

// Held as rows, not joined text: the budget cuts whole rows, and the shown
// range is read off the same cut, so it never names rows the model wasn't sent.
struct View {
    // The `[path]` line naming the file this view came from.
    head: String,
    // Each row as it prints — newline and all — beside the file line it holds.
    rows: Vec<(usize, String)>,
    // What follows the rows: how much of the file is still unread.
    note: String,
    kind: Kind,
}

// What the view is, deciding how it names itself: one field, not a `whole`
// flag beside `outline` — two flags could disagree about which the view is.
enum Kind {
    // A run of file lines; `whole` when they reached both ends of the file.
    Lines { whole: bool },
    // A skeleton, and the length of the file it skips through.
    Outline { lines: usize },
}

// The rows a view keeps at each end when all of them will not fit. `None` when
// they do — the one value both the body and its name are read from.
type Cut = Option<(usize, usize)>;

impl View {
    // The view as the model reads it: every row, or the ends with `cut`'s
    // middle elided — one assembly, so spill and transcript can't disagree.
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

    // Whole rows from each end until the budget is gone: cutting assembled text
    // at a byte offset lands mid-row, and half a line under a number reads as content.
    fn cut(&self) -> Cut {
        let spent = self.head.len() + self.note.len() + crate::rows::GAP.len();
        let room = spill::MAX_OUTPUT.saturating_sub(spent);
        let total: usize = self.rows.iter().map(|(_, r)| r.len()).sum();
        if total <= room {
            return None;
        }
        let size = |(_, r): &&(usize, String)| r.len();
        let head = spill::fits(self.rows.iter(), size, room / 2);
        let tail = spill::fits(self.rows.iter().rev(), size, room / 2);
        Some((head, tail.min(self.rows.len() - head)))
    }

    // The one line a person sees, named from the rows the body actually holds.
    fn shown(&self, rel: &str, cut: Cut) -> String {
        if let Kind::Outline { lines } = self.kind {
            let dropped = cut.map_or(String::new(), |(h, t)| {
                format!(" · {} not shown", self.rows.len() - h - t)
            });
            return format!("{rel} {lines} lines · outline{dropped}");
        }
        if cut.is_none() && matches!(self.kind, Kind::Lines { whole: true }) {
            return rel.to_string();
        }
        let at = |i: usize| self.rows[i].0;
        let last = self.rows.len() - 1;
        let spans = match cut {
            None => vec![(at(0), at(last))],
            // fits() keeps 0 rows when one row alone exceeds the half-budget,
            // so either end of the cut can come back empty.
            Some((h, t)) => {
                let mut spans = Vec::new();
                if h > 0 {
                    spans.push((at(0), at(h - 1)));
                }
                if t > 0 {
                    spans.push((at(self.rows.len() - t), at(last)));
                }
                spans
            }
        };
        // Both ends of an elided window, named `N` or `N-M`.
        let named: Vec<String> = spans
            .iter()
            .map(|(a, b)| {
                if a == b {
                    format!("{a}")
                } else {
                    format!("{a}-{b}")
                }
            })
            .collect();
        format!("{rel}:{}", named.join(crate::rows::GAP.trim_end()))
    }
}

// The model's copy (spilled when too long) and the one line a person sees,
// built together so the tag lands only in the model's.
fn deliver(ctx: &Ctx, rel: &str, view: View) -> Result<ToolOutput, ToolError> {
    // One length check decides both halves: two checks off differently
    // assembled strings can disagree at the margin, eliding rows with no way back.
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

impl Read {
    pub const NAME: &'static str = "read";
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Read a file as numbered lines, list a directory, or look at an image \
         (PNG, JPEG, GIF, WebP). Output is headed by \
         [path]; later edits anchor on the content itself, so re-read after the \
         file changes. A long file comes back as a skeleton of its declarations \
         instead — read a range with offset and limit, or hand one whole block \
         to edit as `old_string` with `whole_block: true`."
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
        let args: Args = tool::parse_args(args)?;
        // A `spill:` path names a file in the session's spill directory; only
        // locators our own writer minted resolve, refused before touching the filesystem.
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
            let mut over = 0usize;
            let budget = output::Budget::new();
            while let Some(e) = entries.next_entry().await? {
                let suffix = if e.file_type().await.is_ok_and(|t| t.is_dir()) {
                    "/"
                } else {
                    ""
                };
                let name = format!("{}{suffix}", e.file_name().to_string_lossy());
                // Count past the budget rather than hold: what costs is the
                // transcript the list would become, not the iteration.
                if !budget.admits(name.len()) {
                    over += 1;
                    continue;
                }
                names.push(name);
            }
            names.sort();
            if names.is_empty() {
                return Ok(ToolOutput::text(format!("{rel}/ is empty")));
            }
            let rows: Vec<String> = names.iter().map(|n| format!("{n}\n")).collect();
            let notice = if over > 0 {
                format!("… {over} more entries not shown\n")
            } else {
                String::new()
            };
            // The listing rides the same spill path as every other body.
            let listed = spill::fit(ctx, &rows, "entries", &notice)?;
            return Ok(ToolOutput::text(format!("{rel}/\n{listed}")));
        }

        // A FIFO reports st_size 0, so the byte cap below never sees one
        // coming: opening it to read blocks, and /dev/zero reads forever.
        if !meta.is_file() {
            return Err(ToolError::Invalid(format!(
                "{rel} is not a regular file; read reads regular files only — FIFOs, devices and sockets are not read"
            )));
        }

        // Sniffing needs the whole file in memory, so the size guard precedes
        // the read; a spill locator is exempt since read promised to serve it by locator.
        if meta.len() > MAX_BYTES && !is_spill {
            return Ok(ToolOutput::text(over_limit(&rel, meta.len())));
        }
        let bytes = tokio::fs::read(&path).await?;
        if let Some(media_type) = llm::message::Image::media_type(&bytes) {
            return Ok(image(&rel, media_type, &bytes));
        }
        if looks_binary(&bytes) {
            return Ok(ToolOutput::text(format!(
                "{rel} is binary ({} bytes); read is for text",
                meta.len()
            )));
        }
        // Lossy decoding would hand back U+FFFD that a model could write back
        // and corrupt the file for real; grep can afford lossy, read cannot.
        let content = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(e) => {
                return Ok(ToolOutput::text(format!(
                    "{rel} is not valid UTF-8 (byte {} is invalid). If it is text in \
                     another encoding, convert it with bash first.",
                    e.valid_up_to()
                )));
            }
        };
        let hash = view_hash(content);
        tracing::info!(target: "pi::read", path = %rel, hash = %hash, "read");
        // The numbering shown is current; a window read clears the whole file,
        // not just its own rows — catching an edit built with no read since the last one.
        ctx.note_view(&path, &hash);

        // A BOM is invisible in a terminal but is a character on line 1:
        // stripped from the view so what the model copies back is what an edit can match.
        let shown = content.strip_prefix('\u{FEFF}').unwrap_or(content);
        let all: Vec<&str> = shown.lines().collect();

        // A range request is an explicit ask for lines; only an unqualified read
        // of a long file is worth answering with a skeleton.
        let ranged = args.offset.is_some() || args.limit.is_some();
        let wants_outline = args.outline.unwrap_or(!ranged && all.len() > OUTLINE_OVER);
        if wants_outline && let Some(lang) = crate::syntax::Lang::of(&rel) {
            let items = crate::syntax::outline(lang, shown);
            if !items.is_empty() {
                // From the items already in hand: asking `rows::spans` here
                // would parse the file a second time for the same answer.
                let spans = crate::rows::of(&items);
                let rows = items
                    .iter()
                    .map(|item| {
                        // The span, not just the opening row: it's what an edit
                        // names, and the only view of a long file that shows where anything ends.
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
                        head: format!("{} {} lines · outline", header(&rel), all.len()),
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
            return Ok(ToolOutput::text(format!(
                "{rel} line {offset} is past the end ({} lines)",
                all.len()
            )));
        }

        let end = start.saturating_add(limit).min(all.len());
        // A construct opening inside the window often closes outside it; a row
        // that says where it ends saves a second read.
        let spans = crate::rows::spans(&rel, content);
        let rows = all[start..end]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let n = start + i + 1;
                let mut row = String::new();
                if line.len() > MAX_LINE {
                    row.push_str(&crate::rows::addr(n, &spans));
                    row.push_str(llm::slice::head_bytes(line, MAX_LINE));
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
                head: header(&rel),
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
