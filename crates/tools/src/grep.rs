use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::walk::{excludes, globs, looks_binary, roots_of, walker};
use crate::{Ctx, Tier, Tool, ToolError, ToolOutput, spill};

const DEFAULT_LIMIT: usize = 200;
const PER_FILE_LIMIT: usize = 50;
const MAX_BYTES: u64 = 10 << 20;

/// One search target or several: a directory is walked, a file is searched alone.
#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn list(&self) -> Vec<&str> {
        match self {
            Self::One(p) => vec![p],
            Self::Many(ps) => ps.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<OneOrMany>,
    // File-name globs; only matching files are searched.
    #[serde(default)]
    glob: Vec<String>,
    // Path globs; matching files are skipped. A bare name also excludes the
    // directory of that name, `__pycache__`-style.
    #[serde(default)]
    exclude: Vec<String>,
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
         binaries. `path` takes one target or many — directories or files. Results \
         come back as `[path#TAG]` sections with numbered lines, the same shape read \
         returns — so a match can be edited without reading the file first. Reach \
         for this before running rg or find in bash: it already knows what to ignore."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Rust regex syntax." },
                "path": {
                    "type": ["string", "array"],
                    "items": { "type": "string" },
                    "description": "One target or several — directories or files; an absolute path may leave the workspace. Default the workspace root.",
                },
                "glob": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only search files matching these, e.g. [\"*.rs\"].",
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Skip files matching these, e.g. [\"*_test.go\"]. A bare name also excludes the directory of that name.",
                },
                "insensitive": { "type": "boolean" },
                "files_only": { "type": "boolean", "description": "List paths instead of matching lines." },
                "limit": { "type": "integer", "description": "Max matching lines, or files under files_only. Default 200." },
            },
            "required": ["pattern"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args(args)?;
        let targets: Vec<&str> = args.path.as_ref().map(|p| p.list()).unwrap_or_default();
        let roots = roots_of(&ctx.workspace, &targets, self.tier())?;
        let set = globs(&args.glob)?;
        let skip = excludes(&args.exclude)?;
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
            let (tx, rx) = std::sync::mpsc::channel::<Result<Hit, PathBuf>>();
            for root in &roots {
                walker(&ws, root, skip.clone()).build_parallel().run(|| {
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
                            let _ = tx.send(Err(entry.path().to_path_buf()));
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
            }
            drop(tx);

            let mut hits = Vec::new();
            let mut skipped: Vec<PathBuf> = Vec::new();
            for msg in rx {
                match msg {
                    Ok(h) => hits.push(h),
                    Err(p) => skipped.push(p),
                }
                // One file can sit over the limit under two covering roots; count it once.
                skipped.sort_unstable();
                skipped.dedup();
            }
            (hits, skipped.len())
        })
        .await
        .map_err(|e| ToolError::Invalid(format!("search failed: {e}")))?;

        // Parallel walking returns files in completion order; the transcript
        // should not change between identical searches.
        hits.sort_by(|a, b| a.path.cmp(&b.path));
        // Overlapping roots search one file twice; keep one copy.
        hits.dedup_by(|a, b| a.path == b.path);

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
        // The pattern leads, the tallies are ranked under it: a row saying
        // only how much it found names nothing the caller can recognise.
        let preview = format!("{} [{} files · {total} matches]", args.pattern, hits.len());
        // The two lines that close either view: what the limit left out, and
        // what the sweep could not read. Spelt once because both views owe both.
        let over = |left: usize, unit: &str| {
            let mut note = String::new();
            if left > 0 {
                note.push_str(&format!(
                    "… {left} more {unit}; narrow the pattern or raise limit\n"
                ));
            }
            if skipped > 0 {
                note.push_str(&format!(
                    "… {skipped} files over the size limit were not searched\n"
                ));
            }
            note
        };

        if args.files_only {
            let rows: Vec<String> = hits
                .iter()
                .take(limit)
                .map(|h| format!("{} ({} matches)\n", h.path, h.lines.len()))
                .collect();
            let notice = over(hits.len() - rows.len(), "files");
            return Ok(
                ToolOutput::text(spill::fit(ctx, &rows, "files", &notice)?).with_preview(preview)
            );
        }

        // One per hit file, so a body over budget drops whole sections: a row
        // parted from the `[path#TAG]` above it has no tag for an edit to name.
        let mut sections: Vec<String> = Vec::new();
        let mut shown = 0usize;
        for h in &hits {
            if shown >= limit {
                break;
            }
            let mut section = format!("{}\n", hashline::header(&h.path, &h.tag));
            // The one view that prints addresses without spans: a match is
            // rarely a construct's opening row, and a parse per hit file would
            // cost more than a view that only points is worth.
            let spans = std::collections::HashMap::new();
            for (n, text) in &h.lines {
                if shown >= limit {
                    break;
                }
                crate::rows::line(&mut section, *n as usize, &spans, text);
                shown += 1;
            }
            if h.truncated {
                section.push_str(&format!(
                    "… more than {PER_FILE_LIMIT} matches in this file\n"
                ));
            }
            sections.push(section);
        }
        let notice = over(total - shown, "matches");

        Ok(ToolOutput::text(spill::fit(ctx, &sections, "files", &notice)?).with_preview(preview))
    }
}
