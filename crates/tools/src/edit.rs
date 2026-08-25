use async_trait::async_trait;
use hashline::{Change, Landed};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// How many landed lines to echo back per file before summarizing instead.
const ECHO_LIMIT: usize = 40;
/// Diff rows a run's display carries for one patch.
const SKETCH_LIMIT: usize = 24;

#[derive(Deserialize)]
struct Args {
    patch: String,
}

const SHAPE: &str = r#"Line-anchored patch. Sections name a file and the TAG from your last read of it:

[path/to/file.rs#A1B2]
PUT 2-4:
+replacement line one
+replacement line two

Addresses. Every one is a line number and a suffix, in that order, and every
number is the ORIGINAL one from that read: earlier hunks in the same patch never
shift later ones. A bare number is not an address.
{addresses}

Ops. Every op line starts with a verb — PUT, CUT, MV or REM. A header ending in
`:` takes `+` body rows; CUT, MV, REM and the register pastes take none.
  PUT <addr>:  put the body there. A gap address inserts, a line address replaces.
  CUT N-M      delete those lines, capturing them; add `@name` to label it
  CUT N*       the same, for a whole construct
  PUT N< @name / PUT N> @name / PUT N-M @name   paste a captured register
  MV dest      rename; edits in this section land first, then the file moves
  REM          delete the file; may not share a section with other ops

Cheapest first: insert at a gap (N< / N>), delete with CUT, address only the
lines that change, and leave unchanged lines out of the body. PUT N* is for a
mostly-rewritten construct, not a one-line change.

Body rows start with `+` and are copied verbatim, so `+` alone is a blank line
and leading whitespace is preserved. Never write `-old` or bare context lines:
the address says what goes, the body says what arrives. To delete lines and put
nothing back, use CUT — not a PUT with `-` rows. A literal line of your own that
begins with `-` or `+` takes the prefix like any other: `- item` is written
`+- item`. A body may be any length regardless of how many lines the address
names.

Rejected outright: a stale TAG, two hunks touching the same original line, an
address past the end of the file, and a patch that would leave the file
unparseable when it parsed before. Nothing is written unless every section
applies."#;

/// One table row, wrapped under its own label rather than running off the side.
///
/// The description is read by a model on every request; a paragraph that used
/// to wrap and now does not is a real cost of generating prose instead of
/// writing it.
fn wrapped(label: &str, width: usize, text: &str) -> String {
    const RIGHT: usize = 78;
    let pad = 2 + width + 2;
    let mut out = format!("  {label:<width$}  ");
    let mut col = pad;
    for (i, word) in text.split_whitespace().enumerate() {
        if i > 0 && col + 1 + word.len() > RIGHT {
            out.push('\n');
            out.push_str(&" ".repeat(pad));
            col = pad;
        } else if i > 0 {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += word.len();
    }
    out
}

/// The description the model reads, with the address forms filled in from the
/// table that defines them.
///
/// Built rather than written out: this prose and the parser disagreeing is not
/// hypothetical — it happened inside the commit that moved the grammar, and the
/// stale line sat two functions away from the rewrite.
fn format() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let width = hashline::FORMS
            .iter()
            .map(|f| f.suffix.len())
            .max()
            .unwrap_or(0)
            + 1;
        let addresses: Vec<String> = hashline::FORMS
            .iter()
            .map(|f| wrapped(&format!("N{}", f.suffix), width, f.means))
            .collect();
        SHAPE.replace("{addresses}", &addresses.join("\n"))
    })
}

fn echo(path: &str, before: &str, content: &str, landed: &[Landed]) -> String {
    let mut out = format!("[{path}#{}]", hashline::tag(content));
    if landed.iter().all(|l| l.gave() == 0) {
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
    let total: usize = landed.iter().map(hashline::Landed::gave).sum();
    out.push('\n');
    if total > ECHO_LIMIT {
        for l in landed.iter().filter(|l| l.gave() > 0) {
            out.push_str(&format!(
                "… {} lines now at {}-{}\n",
                l.gave(),
                l.start,
                l.end
            ));
        }
        return out;
    }
    // What just landed, addressed the way a second edit would name it: the
    // numbering moved, and a construct that grew has a new end.
    let spans = crate::rows::spans(path, content);
    for l in landed {
        for n in l.start..=l.end {
            if let Some(text) = lines.get(n - 1) {
                out.push_str(&crate::rows::addr(n, &spans));
                out.push_str(text);
                out.push('\n');
            }
        }
    }
    out
}

/// Refuse a patch that leaves a file the parser can no longer read.
///
/// Only "parsed before, does not now" — never "does not parse". A file that is
/// already broken is usually the reason an edit is happening, and refusing to
/// touch it would strand the model with no way to repair it.
///
/// This is what a line range costs and `N*` does not: the model resolves the
/// closing line itself, and one off leaves an orphaned brace that applies
/// cleanly. Nothing is written when it fires, so the whole patch stays undone.
///
/// The message carries the hunk addresses against the file as it stands, and
/// any hunk whose body nets a different brace count from the lines it replaces
/// — the first thing to look at when the parse broke.
fn broke_syntax(plan: &hashline::Plan, loaded: &HashMap<String, String>) -> Option<String> {
    for change in &plan.changes {
        let (path, before, after, landed) = match change {
            Change::Write {
                path,
                content,
                landed,
                ..
            } => (path, loaded.get(path), content, Some(landed)),
            Change::Rename {
                from,
                to,
                content,
                landed,
                ..
            } => (to, loaded.get(from), content, Some(landed)),
            Change::Remove { .. } => continue,
        };
        if let Some((row, text)) = crate::parses::broke(path, before.map(String::as_str), after) {
            let mut why = format!(
                "{path} would not parse: line {row} is `{text}`, and it did \
                 parse before this patch. A range that covers one line too few \
                 or too many does exactly this. Re-read and check where the \
                 construct actually ends. Nothing was written."
            );
            if let Some(landed) = landed {
                why.push('\n');
                why.push_str(&hunk_help(after, landed));
            }
            return Some(why);
        }
    }
    None
}

/// What the patch's own hunks point at, for a break that a bare "line N is
/// `}`" leaves the model to hunt down by itself. Each hunk shows the lines it
/// displaces (`took` — the file as it stands, since nothing has been written)
/// and any whose body nets a different brace count from what it displaces —
/// the shape an off-by-one range leaves behind.
fn hunk_help(after: &str, landed: &[Landed]) -> String {
    let new: Vec<&str> = after.lines().collect();
    let mut out = String::from("The hunks, against the file as it stands:");
    let mut off = String::new();
    for (i, l) in landed.iter().enumerate() {
        let addr = if l.took.is_empty() {
            format!("{}", l.start)
        } else if l.took.len() == 1 {
            format!("{}-{}", l.start, l.start)
        } else {
            format!("{}-{}", l.start, l.start + l.took.len() - 1)
        };
        if i < 6 {
            if l.took.is_empty() {
                out.push_str(&format!("\n  {addr}(insertion)"));
            } else {
                let cur = l
                    .took
                    .iter()
                    .map(|s| crop(s, 60))
                    .collect::<Vec<_>>()
                    .join("\n    ");
                out.push_str(&format!("\n  {addr}: `{cur}`"));
            }
        } else if i == 6 {
            out.push_str(&format!("\n  … {} more", landed.len() - i));
        }
        let took: isize = l.took.iter().map(|s| brace_net(s)).sum();
        let gave: isize = hunk_rows(&new, l).iter().map(|s| brace_net(s)).sum();
        if took != gave {
            off.push_str(&format!(
                "\n  {addr}: its body nets {gave}, the lines it displaces net {took}"
            ));
        }
    }
    if !off.is_empty() {
        out.push_str("\nBrace balance:");
        out.push_str(&off);
    }
    out
}
/// Clamped to whatever `lines` actually holds: a range that reaches past the
/// end, or starts at zero, shows what there is rather than panicking.
fn hunk_rows<'a, 'b>(lines: &'a [&'b str], l: &Landed) -> &'a [&'b str] {
    &lines[l.start.saturating_sub(1).min(lines.len())..l.end.min(lines.len())]
}
fn brace_net(s: &str) -> isize {
    s.chars().fold(0, |n, c| match c {
        '{' => n + 1,
        '}' => n - 1,
        _ => n,
    })
}

fn crop(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut t: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        t.push('…');
    }
    t
}

/// What a person watching sees: the lines that went, and the lines that came.
///
/// Separate from the report the model reads, which is a set of addresses it can
/// edit against. "Where can I edit next" and "what just changed" are different
/// questions, and only the second one has a reader.
///
/// The first line rides beside the tool's name, so it carries the counts; the
/// rest are the diff itself, capped, because a display is not a transcript.
fn sketch(changes: &[Change], loaded: &HashMap<String, String>) -> String {
    let (mut plus, mut minus) = (0usize, 0usize);
    let mut files: Vec<(&str, Vec<String>)> = Vec::new();
    for change in changes {
        let (path, content, landed) = match change {
            Change::Write {
                path,
                content,
                landed,
            } => {
                // The same skip the report makes: a body that already matched
                // wrote nothing, and counting the file says it did.
                if loaded.get(path).is_some_and(|before| before == content) {
                    continue;
                }
                (path, content, landed)
            }
            Change::Rename {
                to,
                content,
                landed,
                ..
            } => (to, content, landed),
            Change::Remove { path } => {
                // Counted, not listed: deleting a file removes every line in
                // it, and `+0 -0` would read as nothing having happened. What
                // those lines said is not what a reader needs here.
                minus += loaded.get(path).map_or(0, |c| c.lines().count());
                files.push((path.as_str(), Vec::new()));
                continue;
            }
        };
        let lines: Vec<&str> = content.lines().collect();
        let mut rows = Vec::new();
        for l in landed {
            let gave = hunk_rows(&lines, l);
            // A hunk whose body already matched changed nothing, and a diff
            // that shows it says something happened that did not.
            if l.took == gave {
                continue;
            }
            minus += l.took.len();
            plus += gave.len();
            rows.extend(l.took.iter().map(|old| format!("-{old}")));
            rows.extend(gave.iter().map(|new| format!("+{new}")));
        }
        files.push((path.as_str(), rows));
    }

    let head = match files.as_slice() {
        [(one, _)] => format!("{one} +{plus} -{minus}"),
        many => format!("{} files +{plus} -{minus}", many.len()),
    };
    // A path before each file's hunks, but only once there is more than one
    // file to tell apart: with a single one the head already said which.
    let named = files.iter().filter(|(_, r)| !r.is_empty()).count() > 1;
    let mut rows = Vec::new();
    for (path, mut own) in files {
        if own.is_empty() {
            continue;
        }
        if named {
            rows.push(path.to_string());
        }
        rows.append(&mut own);
    }
    if rows.len() > SKETCH_LIMIT {
        let more = rows.len() - SKETCH_LIMIT;
        rows.truncate(SKETCH_LIMIT);
        rows.push(format!("… {more} more"));
    }
    std::iter::once(head)
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
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
        format()
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
        let args: Args = crate::parse_args(args)?;
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

        if let Some(why) = broke_syntax(&plan, &loaded) {
            tracing::warn!(
                target: "pi::edit",
                stage = "syntax",
                error = %why,
                patch = %args.patch,
                "patch rejected"
            );
            return Err(ToolError::Invalid(why));
        }

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
        Ok(ToolOutput::text(report).with_preview(sketch(&plan.changes, &loaded)))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parse_break_shows_the_hunk_addresses() {
        let before = "fn f() {\n    a;\n}\n\nfn g() {";
        let landed = vec![Landed {
            start: 5,
            end: 5,
            took: vec!["fn g() {".into()],
        }];
        let help = hunk_help(before, &landed);
        assert!(help.contains("5-5"), "{help}");
        assert!(help.contains("fn g() {"), "{help}");
        assert!(!help.contains("Brace balance"), "{help}");
    }

    #[test]
    fn a_body_with_one_brace_too_many_is_called_out() {
        let after = "fn f() {\n}\n}\n";
        let landed = vec![Landed {
            start: 3,
            end: 3,
            took: vec![String::new()],
        }];
        let help = hunk_help(after, &landed);
        assert!(help.contains("Brace balance:"), "{help}");
        assert!(help.contains("nets -1"), "{help}");
    }
}
