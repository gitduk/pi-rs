use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::walk::{globs, looks_binary, root_of, walker};
use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

const DEFAULT_LIMIT: usize = 200;
const PER_FILE_LIMIT: usize = 50;
const MAX_BYTES: u64 = 10 << 20;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    /// File-name globs; only matching files are searched.
    #[serde(default)]
    glob: Vec<String>,
    #[serde(default)]
    insensitive: bool,
    #[serde(default)]
    files_only: bool,
    #[serde(default)]
    limit: Option<usize>,
}

struct Hit {
    path: String,
    tag: String,
    lines: Vec<(u64, String)>,
    truncated: bool,
}

pub struct Grep;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents by regular expression. Respects .gitignore and skips \
         binaries. Results come back as `[path#TAG]` sections with numbered lines, \
         the same shape read returns — so a match can be edited without reading \
         the file first."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex syntax." },
                "path": { "type": "string", "description": "Subdirectory to search. Default the workspace root." },
                "glob": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only search files matching these, e.g. [\"*.rs\"].",
                },
                "insensitive": { "type": "boolean" },
                "files_only": { "type": "boolean", "description": "List paths instead of matching lines." },
                "limit": { "type": "integer", "description": "Max matching lines. Default 200." },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)?;
        let root = root_of(&ctx.workspace, &args.path)?;
        let set = globs(&args.glob)?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let ws = ctx.workspace.clone();

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(args.insensitive)
            .line_terminator(Some(b'\n'))
            .build(&args.pattern)
            .map_err(|e| ToolError::Invalid(format!("bad pattern `{}`: {e}", args.pattern)))?;

        // Reading and searching are blocking; the parallel walker needs its own
        // threads either way, so the whole sweep goes off the async runtime.
        let (mut hits, skipped) = tokio::task::spawn_blocking(move || {
            let (tx, rx) = std::sync::mpsc::channel::<Result<Hit, ()>>();
            walker(&root).build_parallel().run(|| {
                let tx = tx.clone();
                let matcher = matcher.clone();
                let set = set.clone();
                let ws = ws.clone();
                Box::new(move |entry| {
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return ignore::WalkState::Continue;
                    }
                    if set.as_ref().is_some_and(|s| !s.is_match(entry.path())) {
                        return ignore::WalkState::Continue;
                    }
                    if entry.metadata().is_ok_and(|m| m.len() > MAX_BYTES) {
                        let _ = tx.send(Err(()));
                        return ignore::WalkState::Continue;
                    }
                    let Ok(bytes) = std::fs::read(entry.path()) else {
                        return ignore::WalkState::Continue;
                    };
                    if looks_binary(&bytes) {
                        return ignore::WalkState::Continue;
                    }

                    // Searched lossily, not raw: one stray byte would make the
                    // UTF-8 sink error out and drop the whole file in silence.
                    // It also makes the tag identical to the one `read` emits.
                    let text = String::from_utf8_lossy(&bytes);

                    let mut lines: Vec<(u64, String)> = Vec::new();
                    let mut truncated = false;
                    let mut searcher = SearcherBuilder::new().line_number(true).build();
                    let _ = searcher.search_slice(
                        &matcher,
                        text.as_bytes(),
                        UTF8(|n, line| {
                            if lines.len() >= PER_FILE_LIMIT {
                                truncated = true;
                                return Ok(false);
                            }
                            lines.push((n, line.trim_end_matches('\n').to_string()));
                            Ok(true)
                        }),
                    );

                    if !lines.is_empty() {
                        // The tag comes from the same bytes that were searched,
                        // so an edit anchored on it cannot be racing this read.
                        let tag = hashline::tag(&text);
                        let _ = tx.send(Ok(Hit {
                            path: ws.display(entry.path()),
                            tag,
                            lines,
                            truncated,
                        }));
                    }
                    ignore::WalkState::Continue
                })
            });
            drop(tx);

            let mut hits = Vec::new();
            let mut skipped = 0usize;
            for msg in rx {
                match msg {
                    Ok(h) => hits.push(h),
                    Err(()) => skipped += 1,
                }
            }
            (hits, skipped)
        })
        .await
        .map_err(|e| ToolError::Invalid(format!("search failed: {e}")))?;

        // Parallel walking returns files in completion order; the transcript
        // should not change between identical searches.
        hits.sort_by(|a, b| a.path.cmp(&b.path));

        if hits.is_empty() {
            let note = if skipped > 0 {
                format!(" ({skipped} files over the size limit were skipped)")
            } else {
                String::new()
            };
            return Ok(ToolOutput::useless(format!(
                "no match for `{}`{note}",
                args.pattern
            )));
        }

        let total: usize = hits.iter().map(|h| h.lines.len()).sum();
        let mut out = String::new();

        if args.files_only {
            for h in &hits {
                out.push_str(&format!("{} ({} matches)\n", h.path, h.lines.len()));
            }
            return Ok(ToolOutput::text(out)
                .with_preview(format!("{} files, {total} matches", hits.len())));
        }

        let mut shown = 0usize;
        for h in &hits {
            if shown >= limit {
                break;
            }
            out.push_str(&format!("[{}#{}]\n", h.path, h.tag));
            // The fourth view that prints an address, and the last one still
            // printing a bare number the parser refuses. No spans: a match is
            // rarely a construct's opening row, and an outline per hit file
            // would cost a parse for a view that only points.
            let spans = std::collections::HashMap::new();
            for (n, text) in &h.lines {
                if shown >= limit {
                    break;
                }
                out.push_str(&crate::rows::addr(*n as usize, &spans));
                out.push_str(text);
                out.push('\n');
                shown += 1;
            }
            if h.truncated {
                out.push_str(&format!(
                    "… more than {PER_FILE_LIMIT} matches in this file\n"
                ));
            }
        }
        if total > shown {
            out.push_str(&format!(
                "… {} more matches; narrow the pattern or raise limit\n",
                total - shown
            ));
        }
        if skipped > 0 {
            out.push_str(&format!(
                "… {skipped} files over the size limit were not searched\n"
            ));
        }

        Ok(ToolOutput::text(out).with_preview(format!("{} files, {total} matches", hits.len())))
    }
}
