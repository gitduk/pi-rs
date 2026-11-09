use async_trait::async_trait;
use llm::figures::count;
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
                "{rel} is {why}. Shrink it with bash and read the smaller copy"
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
        warned: false,
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
                let (head, tail) = cut.expect("two spans come from a cut");
                let first = self.rows[head].0;
                let last = self.rows[self.rows.len() - tail - 1].0;
                let subject = match self.kind {
                    Kind::Lines { .. } => "lines",
                    Kind::Outline { .. } => "declarations at lines",
                };
                out.push_str(&format!("… {subject} {first}-{last} omitted.\n"));
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
        // Reserve enough for either gap label and the widest possible line
        // numbers, so naming the omitted range cannot exceed the budget.
        let last = self.rows.last().map_or(0, |(n, _)| *n);
        let gap = format!("… declarations at lines {last}-{last} omitted.\n");
        let spent = self.head.len() + self.note.len() + gap.len();
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
            return format!("{rel} {} · outline{dropped}", count(lines, "line"));
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

// Lines `start..end`, 0-based, as `(1-based number, text)`: only those are
// built, so the cost is the window and not the file.
fn window(text: &str, start: usize, end: usize) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|(i, line)| (i + 1, line))
}

// One numbered row, its line capped at MAX_LINE: the shape both windows print.
fn row(n: usize, line: &str) -> (usize, String) {
    let mut out = crate::rows::numbered(n);
    if line.len() > MAX_LINE {
        out.push_str(llm::slice::head_bytes(line, MAX_LINE));
        out.push_str("… (line truncated)\n");
    } else {
        out.push_str(line);
        out.push('\n');
    }
    (n, out)
}

// A `spill:` file's lines, read from the start up to the end of the window and
// no further: the file is pi's own output and can be hundreds of megabytes, so
// it is never held whole. No total either, which reading past the window would
// be the only way to learn.
async fn spill_lines(
    rel: &str,
    path: &std::path::Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ToolOutput, ToolError> {
    use std::io::{BufRead, BufReader};
    let offset = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let (path, rel) = (path.to_path_buf(), rel.to_string());
    let (rows, more) = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let reader = BufReader::with_capacity(64 * 1024, file);
        spill_window(reader.lines(), offset, limit)
    })
    .await
    .map_err(|e| ToolError::Invalid(format!("{rel}: {e}")))??;

    if rows.is_empty() {
        let at = if offset > 1 {
            format!(" (line {offset} is past the end)")
        } else {
            String::new()
        };
        return Ok(ToolOutput::text(format!("{rel} is empty{at}")));
    }
    let span = format!("{rel}:{}-{}", rows[0].0, rows[rows.len() - 1].0);
    let shown = if more { format!("{span} more") } else { span };
    let note = if more {
        format!(
            "… more lines follow. Read again from line {}.\n",
            offset + rows.len()
        )
    } else {
        String::new()
    };
    let mut body = String::new();
    body.push_str(&header(&rel));
    body.push('\n');
    for (_, row) in &rows {
        body.push_str(row);
    }
    body.push_str(&note);
    Ok(ToolOutput::text(body).with_preview(shown))
}

// The rows of a spill at `offset` for `limit` lines, and whether more follow.
// The lines are read no further than the window and the one past it: this is
// what makes a read of a spill cost its window and not the file.
fn spill_window(
    lines: impl Iterator<Item = std::io::Result<String>>,
    offset: usize,
    limit: usize,
) -> std::io::Result<(Vec<(usize, String)>, bool)> {
    let mut rows = Vec::new();
    for (i, line) in lines.enumerate() {
        let n = i + 1;
        if n < offset {
            continue;
        }
        // Read, not just counted: the line after the window is what says there
        // is more, and reading it is the furthest this goes.
        let line = line?;
        if rows.len() == limit {
            return Ok((rows, true));
        }
        rows.push(row(n, &line));
    }
    Ok((rows, false))
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
        "Read a file as numbered lines, list a directory, or view a PNG, JPEG, GIF, or WebP image. \
         Output starts with `[path]`. \
         An edit anchor is existing text that identifies where to change the file. \
         Use file content as edit anchors. \
         Read the file again after it changes.\n\n\
         For a long file, the tool returns an outline that lists its declarations. \
         Use `offset` and `limit` to read a line range. \
         To edit a complete block, give its outline entry to `edit` as `old_string` with `whole_block: true`.\n\n\
         To read saved tool output, set `path` to its reported `spill:` locator."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Specify a path relative to the workspace or an absolute path to a readable file or directory. For saved tool output, supply its `spill:` locator." },
                "offset": { "type": "integer", "description": "Start at this line number, counting from 1. The default is 1." },
                "limit": { "type": "integer", "description": "Return at most this many lines. The default is 2000." },
                "outline": {
                    "type": "boolean",
                    "description": "Return the file's declarations instead of its lines. \
                                    Long files use this mode automatically unless you specify `offset` or `limit`.",
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
                "{rel} is not a regular file. The `read` tool reads regular files only. \
                 It does not read FIFOs, devices, or sockets."
            )));
        }

        // A spill is written by pi and read back at whatever size it reached:
        // served by its range, never held whole and never noted as viewed.
        if is_spill {
            return spill_lines(&rel, &path, args.offset, args.limit).await;
        }
        // Sniffing needs the whole file in memory, so the size guard precedes
        // the read.
        if meta.len() > MAX_BYTES {
            return Ok(ToolOutput::text(over_limit(&rel, meta.len())));
        }
        let bytes = tokio::fs::read(&path).await?;
        if let Some(media_type) = llm::message::Image::media_type(&bytes) {
            return Ok(image(&rel, media_type, &bytes));
        }
        if looks_binary(&bytes) {
            return Ok(ToolOutput::text(format!(
                "{rel} is binary ({} bytes). Read is for text",
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
        // The whole text, because an edit's staleness check and the diff of
        // what a command changed both read it: this is what a read of a
        // workspace file keeps, and why one loads the file it is asked about.
        ctx.note_view(&path, content);

        // A BOM is invisible in a terminal but is a character on line 1:
        // stripped from the view so what the model copies back is what an edit can match.
        let shown = content.strip_prefix('\u{FEFF}').unwrap_or(content);
        let lines = shown.lines().count();

        // A range request is an explicit ask for lines; only an unqualified read
        // of a long file is worth answering with a skeleton.
        let ranged = args.offset.is_some() || args.limit.is_some();
        let wants_outline = args.outline.unwrap_or(!ranged && lines > OUTLINE_OVER);
        if wants_outline && let Some(lang) = crate::syntax::Lang::of(&rel) {
            let items = crate::syntax::outline(lang, shown);
            if !items.is_empty() {
                // Reuse the declarations' ranges without parsing again.
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
                        head: format!("{} {} · outline", header(&rel), count(lines, "line")),
                        rows,
                        note: "… declarations only, each with the range that replaces it \
                               whole. Read a range with offset and limit.\n"
                            .into(),
                        kind: Kind::Outline { lines },
                    },
                );
            }
        }

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let start = offset - 1;

        if lines == 0 {
            return Ok(ToolOutput::text(format!(
                "{rel} is empty ({} bytes)",
                bytes.len()
            )));
        }
        if start >= lines {
            return Ok(ToolOutput::text(format!(
                "{rel} line {offset} is past the end ({})",
                count(lines, "line")
            )));
        }

        let end = start.saturating_add(limit).min(lines);
        // Only the window is built: a file of a million lines costs the rows it
        // shows, which is what a range read is for.
        let rows = window(shown, start, end)
            .map(|(n, line)| row(n, line))
            .collect();
        let left = lines - end;
        let note = if left > 0 {
            format!(
                "… lines {}-{} not shown. Read again from line {}.\n",
                end + 1,
                lines,
                end + 1
            )
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
                    whole: start == 0 && end == lines,
                },
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A file that says it is a million lines long, handing out one at a time,
    // and counting the bytes actually taken from it.
    struct Endless {
        taken: Arc<AtomicUsize>,
    }

    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            const LINE: &[u8] = b"a line of the spill file\n";
            let whole = buf.len() / LINE.len();
            for i in 0..whole {
                let at = i * LINE.len();
                buf[at..at + LINE.len()].copy_from_slice(LINE);
            }
            let n = whole * LINE.len();
            self.taken.fetch_add(n, Ordering::SeqCst);
            Ok(n)
        }
    }

    // The window is the whole of what a spill read costs: the file past it is
    // never read, which is what a spill of a hundred megabytes needs.
    #[test]
    fn a_spill_read_takes_the_window_and_not_the_file() {
        let taken = Arc::new(AtomicUsize::new(0));
        let reader = std::io::BufReader::with_capacity(
            16 * 1024,
            Endless {
                taken: taken.clone(),
            },
        );
        let (rows, more) = spill_window(reader.lines(), 1, 3).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(more, "a file this long has more past the window");
        let took = taken.load(Ordering::SeqCst);
        assert!(took <= 32 * 1024, "read {took} bytes for three lines");
    }

    // Past the end there is nothing, and the note says so rather than claiming
    // more follows.
    #[test]
    fn the_end_of_a_spill_is_not_reported_as_more() {
        let lines = ["a", "b"].into_iter().map(|l| Ok(l.to_string()));
        let (rows, more) = spill_window(lines, 1, 5).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!more);
        let (rows, more) = spill_window([Ok("a".to_string())].into_iter(), 9, 5).unwrap();
        assert!(rows.is_empty(), "past the end: {}", rows.len());
        assert!(!more);
    }
}
