use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// Drop a `[path#TAG]` header if the content opens with one.
pub fn undecorate(content: &str) -> &str {
    let mut rest = content;
    if let Some((first, tail)) = rest.split_once('\n')
        && is_header(first)
    {
        rest = tail;
    }
    rest
}

fn is_header(line: &str) -> bool {
    let line = line.trim_end();
    let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) else {
        return false;
    };
    match inner.rsplit_once('#') {
        Some((path, tag)) => {
            !path.is_empty() && tag.len() == 4 && tag.chars().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

fn numbered_throughout(text: &str) -> bool {
    let mut any = false;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        // Through hashline, because what a prefix looks like is that crate's
        // grammar and this is the third place in the tree that has to agree
        // with it. A local "all digits" test was right until addresses grew a
        // range, and then it silently stopped recognising read's own output.
        let Some((n, _)) = line.split_once(':') else {
            return false;
        };
        if hashline::Target::read(n).is_none() {
            return false;
        }
        any = true;
    }
    any
}

/// Drop the `N:` from every line, once every line is known to carry one.
fn strip_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        match line.split_once(':') {
            Some((_, rest)) => out.push_str(rest),
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

/// Undo whatever decoration `read` and `grep` put on their output.
///
/// The numbered format invites pasting it straight back, and a file written
/// that way is corrupt in a way nothing downstream notices. Both tests are
/// deliberately strict — a lone `[path#ABCD]` header, and *every* line
/// numbered — so real content that merely resembles them survives untouched.
pub fn clean(content: &str) -> String {
    let body = undecorate(content);
    if numbered_throughout(body) {
        strip_numbers(body)
    } else {
        body.to_string()
    }
}

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create a file or replace its entire contents. Missing parent directories \
         are created. To change part of an existing file, use edit. Replacing a \
         file that parses with content that does not is refused, and nothing is \
         written."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path." },
                "content": { "type": "string", "description": "Full file contents." },
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args(args)?;
        let path = ctx.workspace.resolve(&args.path)?;
        let rel = ctx.workspace.display(&path);
        let _guard = ctx.lock_file(&path).await;

        let content = clean(&args.content);

        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
            return Err(ToolError::Invalid(format!("{rel} is a directory")));
        }
        // Only an overwrite is gated. A new file that does not parse is a stub
        // or a scaffold, and there is nothing behind it to lose; an overwrite
        // that does not parse is most often content that ran short, and the
        // tail of a working file goes with it, unmentioned by either side.
        if let Ok(old) = tokio::fs::read_to_string(&path).await
            && let Some((row, text)) = crate::parses::broke(&rel, Some(&old), &content)
        {
            return Err(ToolError::Invalid(format!(
                "{rel} would not parse: line {row} is `{text}`, and it did \
                 parse before this write. Content that ran short does exactly \
                 this — check that the end of what you sent is the end of the \
                 file. Nothing was written."
            )));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Temp then rename: a write interrupted halfway would otherwise leave a
        // truncated file where a whole one used to be.
        let tmp = path.with_extension(format!(
            "{}.pi-tmp",
            path.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        tokio::fs::write(&tmp, &content).await?;
        tokio::fs::rename(&tmp, &path).await?;

        let mut note = "";
        #[cfg(unix)]
        if content.starts_with("#!") {
            use std::os::unix::fs::PermissionsExt;
            // A script the agent cannot run is one it will debug for a turn
            // before noticing why.
            let perms = std::fs::Permissions::from_mode(0o755);
            if tokio::fs::set_permissions(&path, perms).await.is_ok() {
                note = " · made executable";
            }
        }

        let lines = content.lines().count();
        let unit = if lines == 1 { "line" } else { "lines" };
        let tag = hashline::tag(&content);
        Ok(ToolOutput::text(format!(
            "[{rel}#{tag}] wrote {lines} {unit}, {} bytes{note}",
            content.len()
        )))
    }
}
