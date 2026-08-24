use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::SystemTime;

use crate::walk::{globs, root_of, walker};
use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

const DEFAULT_LIMIT: usize = 200;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files by path pattern, newest first. Respects .gitignore. A pattern \
         with no `/` matches at any depth, so `*.rs` finds every Rust file. Use \
         grep when you need to match file contents."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "e.g. `*.rs`, `src/**/mod.rs`" },
                "path": { "type": "string", "description": "Subdirectory to search. Default the workspace root." },
                "limit": { "type": "integer", "description": "Max paths. Default 200." },
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
        let root = root_of(&ctx.workspace, &args.path)?;
        let set = globs(std::slice::from_ref(&args.pattern))?
            .ok_or_else(|| ToolError::Invalid("empty pattern".into()))?;
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);
        let ws = ctx.workspace.clone();

        // The walk is blocking IO; running it on the async runtime would stall
        // every other tool in the same turn.
        let found = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<(SystemTime, String)> = Vec::new();
            for entry in walker(&root).build().flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                if !set.is_match(entry.path()) {
                    continue;
                }
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified().map_err(Into::into))
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                hits.push((mtime, ws.display(entry.path())));
            }
            // Newest first: an agent hunting the file it just touched wants the
            // recent end, and the tail is what a limit should drop.
            hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            hits
        })
        .await
        .map_err(|e| ToolError::Invalid(format!("walk failed: {e}")))?;

        if found.is_empty() {
            return Ok(ToolOutput::useless(format!(
                "no file matches `{}`",
                args.pattern
            )));
        }

        let total = found.len();
        let mut out = String::new();
        for (_, path) in found.iter().take(limit) {
            out.push_str(path);
            out.push('\n');
        }
        if total > limit {
            out.push_str(&format!(
                "… {} more; narrow the pattern or raise limit\n",
                total - limit
            ));
        }
        Ok(ToolOutput::text(out).with_preview(format!("{total} files")))
    }
}
