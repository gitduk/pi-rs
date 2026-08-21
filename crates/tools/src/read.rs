use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use hashline::tag;

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

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
        let args: Args = serde_json::from_value(args)?;
        let path = ctx.workspace.resolve(&args.path)?;
        let rel = ctx.workspace.display(&path);

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
        let content = String::from_utf8_lossy(&bytes);
        let tag = tag(&content);

        let all: Vec<&str> = content.lines().collect();

        // A range request is an explicit ask for lines; only an unqualified read
        // of a long file is worth answering with a skeleton.
        let ranged = args.offset.is_some() || args.limit.is_some();
        let wants_outline = args.outline.unwrap_or(!ranged && all.len() > OUTLINE_OVER);
        if wants_outline && let Some(lang) = syntax::Lang::of(&rel) {
            let items = syntax::outline(lang, &content);
            if !items.is_empty() {
                let mut out = format!("[{rel}#{tag}] {} lines · outline\n", all.len());
                for item in &items {
                    out.push_str(&format!("{}:{}\n", item.line, item.text));
                }
                out.push_str(
                    "… declarations only. Read a range with offset and limit, or \
                     replace one whole with edit's `PUT N*:`.\n",
                );
                return Ok(ToolOutput::text(out));
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
        for (i, line) in all[start..end].iter().enumerate() {
            let n = start + i + 1;
            if line.len() > MAX_LINE {
                let cut = crate::bash::head(line, MAX_LINE);
                out.push_str(&format!("{n}:{cut}… (line truncated)\n"));
            } else {
                out.push_str(&format!("{n}:{line}\n"));
            }
        }
        if end < all.len() {
            out.push_str(&format!(
                "… {} more lines; re-read from {}\n",
                all.len() - end,
                end + 1
            ));
        }
        Ok(ToolOutput::text(out))
    }
}
