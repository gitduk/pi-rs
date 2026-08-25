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

/// Deliver the assembled view. Anything over the threshold is spilled and the
/// transcript keeps a bounded head and tail plus the locator.
fn deliver(ctx: &Ctx, out: String) -> Result<ToolOutput, ToolError> {
    match spill::write(ctx, &out)? {
        None => Ok(ToolOutput::text(out)),
        Some(s) => Ok(ToolOutput::text(format!(
            "{}\n{}",
            spill::prune(&out),
            s.note()
        ))),
    }
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
                "path": { "type": "string", "description": "Workspace-relative path." },
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
        // A `spill:` path names a file in the session's spill directory, which
        // the workspace gate would refuse. Only locators our own writer mints
        // resolve; anything else is refused before the filesystem is touched.
        let (path, rel) = match args.path.strip_prefix("spill:") {
            Some(_) => {
                let path = ctx.spill_path(&args.path)?;
                (path, args.path.clone())
            }
            None => {
                let p = ctx.workspace.resolve(&args.path)?;
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

        if meta.len() > MAX_BYTES {
            // Sniffing needs the whole file in memory, so the guard precedes the read.
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
                let mut out = format!("[{rel}#{tag}] {} lines · outline\n", all.len());
                for item in &items {
                    // The span, not just the opening row: it is what an edit
                    // names, and a skeleton is the only view of a long file
                    // that shows where anything ends.
                    out.push_str(&crate::rows::addr(item.line, &spans));
                    for _ in 0..item.depth {
                        out.push_str("  ");
                    }
                    out.push_str(&item.text);
                    out.push('\n');
                }
                out.push_str(
                    "… declarations only, each with the range that replaces it whole. \
                     Read a range with offset and limit.\n",
                );
                return deliver(ctx, out);
            }
        }

        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let start = offset - 1;

        if start >= all.len() {
            return Ok(ToolOutput::useless(format!(
                "[{rel}#{tag}]\nline {offset} is past the end ({} lines)",
                all.len()
            )));
        }

        let end = (start + limit).min(all.len());
        let mut out = format!("[{rel}#{tag}]\n");
        // A construct opening inside the window often closes outside it, and a
        // row that says where it ends is the difference between one read and
        // two.
        let spans = crate::rows::spans(&rel, content);
        for (i, line) in all[start..end].iter().enumerate() {
            let n = start + i + 1;
            out.push_str(&crate::rows::addr(n, &spans));
            if line.len() > MAX_LINE {
                out.push_str(brain::slice::head_bytes(line, MAX_LINE));
                out.push_str("… (line truncated)\n");
            } else {
                out.push_str(line);
                out.push('\n');
            }
        }
        if end < all.len() {
            out.push_str(&format!(
                "… {} more lines; re-read from {}\n",
                all.len() - end,
                end + 1
            ));
        }
        // The progress line shows the rows that actually came back: offset
        // may have been clamped, and the file may have ended first.
        let mut output = deliver(ctx, out)?;
        if ranged {
            output = output.with_preview(format!("[{rel}#{tag} {}-{}]", start + 1, end));
        }
        Ok(output)
    }
}
