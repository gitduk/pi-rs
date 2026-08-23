use async_trait::async_trait;
use hashline::{Change, Landed};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// How many landed lines to echo back per file before summarizing instead.
const ECHO_LIMIT: usize = 40;

#[derive(Deserialize)]
struct Args {
    patch: String,
}

const FORMAT: &str = r#"Line-anchored patch. Sections name a file and the TAG from your last read of it:

[path/to/file.rs#A1B2]
PUT 2.=4:
+replacement line one
+replacement line two

Ops. Every op line starts with a verb — PUT, CUT, MV or REM — and every line
number is the ORIGINAL one from that read: earlier hunks in the same patch never
shift later ones. A header ending in `:` takes `+` body rows; CUT, MV, REM and
the register pastes take none.
  PUT N.=M:   replace original lines N through M, inclusive, with the body
  PUT N*:     replace the whole construct opening at line N; its closing line is
              resolved for you. Any decorator, attribute or doc comment above
              it belongs to it: naming either their rows or N covers them all.
  PUT <N:     insert the body before line N (`<1` is the file head)
  PUT >N:     insert the body after line N (`>$` is the file tail)
  PUT >N*:    insert the body after the construct at N closes
  CUT N.=M    delete lines N through M, capturing them; add `@name` to label it
  CUT N*      the same, for a whole construct
  One line is `PUT N.=N:` or `CUT N.=N`; a bare `PUT N:` / `CUT N` means the
  same and is accepted.
  PUT <N @name / PUT >N @name / PUT N.=M @name   paste a captured register
  MV dest     rename; edits in this section land first, then the file moves
  REM         delete the file; may not share a section with other ops

Body rows start with `+` and are copied verbatim, so `+` alone is a blank line
and leading whitespace is preserved. Never write `-old` or bare context lines:
the range says what goes, the body says what arrives. To delete lines and put
nothing back, use CUT — not a PUT with `-` rows. A literal line of your own that
begins with `-` or `+` takes the prefix like any other: `- item` is written
`+- item`. A body may be any length regardless of how many lines the range
names.

Rejected outright: a stale TAG, two hunks touching the same original line, a
range past the end of the file. Nothing is written unless every section applies."#;

fn echo(path: &str, before: &str, content: &str, landed: &[Landed]) -> String {
    let mut out = format!("[{path}#{}]", hashline::tag(content));
    if landed.is_empty() {
        // A patch of pure CUTs lands nothing, and saying so describes what did
        // not happen. What did is the deletion, which is the whole point of the
        // patch and reads as failure when reported by its absence.
        let gone = before
            .lines()
            .count()
            .saturating_sub(content.lines().count());
        out.push_str(&match gone {
            0 => " nothing moved\n".to_string(),
            1 => " removed 1 line\n".to_string(),
            n => format!(" removed {n} lines\n"),
        });
        return out;
    }
    let lines: Vec<&str> = content.lines().collect();
    let total: usize = landed.iter().map(|l| l.end - l.start + 1).sum();
    out.push('\n');
    if total > ECHO_LIMIT {
        for l in landed {
            out.push_str(&format!(
                "… {} lines now at {}-{}\n",
                l.end - l.start + 1,
                l.start,
                l.end
            ));
        }
        return out;
    }
    for l in landed {
        for n in l.start..=l.end {
            if let Some(text) = lines.get(n - 1) {
                out.push_str(&format!("{n}:{text}\n"));
            }
        }
    }
    out
}

/// What each file's tag is right now, for a refusal that turned on one.
fn tags(loaded: &HashMap<String, String>) -> String {
    let mut out: Vec<String> = loaded
        .iter()
        .map(|(p, c)| format!("{p}#{}", hashline::tag(c)))
        .collect();
    out.sort();
    out.join(" ")
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        FORMAT
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch": { "type": "string", "description": "One or more [path#TAG] sections." },
            },
            "required": ["patch"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)?;
        // The rejected patch itself, because the message alone never says what
        // the model actually wrote — and a model that gets the format wrong
        // gets it wrong the same way for the rest of the session.
        let patch = hashline::parse(&args.patch).map_err(|e| {
            tracing::warn!(
                target: "pi::edit",
                stage = "parse",
                error = %e,
                patch = %args.patch,
                "patch rejected"
            );
            ToolError::Invalid(e.to_string())
        })?;

        // Held for the whole patch: two edits to one file in the same turn would
        // otherwise both read the same bytes, both pass their tag check, and
        // one change would vanish with no error to show for it.
        let mut guards = Vec::new();
        let mut loaded: HashMap<String, String> = HashMap::new();
        for path in patch.paths() {
            let real = ctx.workspace.resolve(path)?;
            guards.push(ctx.lock_file(&real).await);
            let content = tokio::fs::read_to_string(&real).await.map_err(|e| {
                ToolError::Invalid(format!(
                    "{path}: {e}. edit changes existing files; use write to create one"
                ))
            })?;
            loaded.insert(path.to_string(), content);
        }

        let view: HashMap<&str, &str> = loaded
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        // Nothing has touched the disk yet: a rejected patch leaves no trace.
        let plan = hashline::apply(&patch, &view, &crate::blocks::TreeSitter).map_err(|e| {
            // With the tags the files actually have: a stale-tag refusal is
            // unreadable without the number the patch should have carried.
            tracing::warn!(
                target: "pi::edit",
                stage = "apply",
                error = %e,
                tags = %tags(&loaded),
                patch = %args.patch,
                "patch rejected"
            );
            ToolError::Invalid(e.to_string())
        })?;

        let mut report = String::new();
        for change in &plan.changes {
            match change {
                Change::Write {
                    path,
                    content,
                    landed,
                } => {
                    // A patch whose body already matches produces a valid write
                    // and no change. Saying so is what stops the model from
                    // believing a fix landed when nothing moved.
                    if loaded.get(path).is_some_and(|before| before == content) {
                        report.push_str(&format!(
                            "[{path}#{}] unchanged — the patch matches what is already there\n",
                            hashline::tag(content)
                        ));
                        continue;
                    }
                    tokio::fs::write(ctx.workspace.resolve(path)?, content).await?;
                    let before = loaded.get(path).map_or("", String::as_str);
                    report.push_str(&echo(path, before, content, landed));
                }
                Change::Remove { path } => {
                    tokio::fs::remove_file(ctx.workspace.resolve(path)?).await?;
                    report.push_str(&format!("removed {path}\n"));
                }
                Change::Rename {
                    from,
                    to,
                    content,
                    landed,
                } => {
                    let dest = ctx.workspace.resolve(to)?;
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&dest, content).await?;
                    tokio::fs::remove_file(ctx.workspace.resolve(from)?).await?;
                    report.push_str(&format!("{from} → "));
                    let before = loaded.get(from).map_or("", String::as_str);
                    report.push_str(&echo(to, before, content, landed));
                }
            }
        }
        Ok(ToolOutput::text(report))
    }
}
