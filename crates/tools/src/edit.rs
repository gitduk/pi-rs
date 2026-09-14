use async_trait::async_trait;
use hashline::{Change, Landed};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

use crate::read::{MAX_BYTES, over_limit};
use crate::{Ctx, PatchError, Tier, Tool, ToolError, ToolOutput};

// How many bytes of landed rows to echo back per file before showing each
// hunk's ends instead of all of it. Bytes, not rows: forty rows of `}` and
// forty rows of a wrapped call are the same budget and an order of magnitude
// apart, and what a transcript pays for is the bytes.
const ECHO_LIMIT: usize = 2_000;
// Rows kept at each end of a hunk once a patch is past ECHO_LIMIT.
const ECHO_ENDS: usize = 3;
// Diff rows a run's display carries for one patch.
const SKETCH_LIMIT: usize = 24;
// What a call that omits the argument is told, so it resends `patch`
// instead of staring at a bare serde error for a field it never named.
const ARGS_HINT: &str = "`edit` takes a single argument, `patch`: a string of \
    one or more `[path]` sections, each a header line followed by marked rows \
    like `-old` / `+new`. Send the whole call again with `patch`.";

#[derive(Deserialize)]
struct Args {
    patch: String,
}

const SHAPE: &str = r#"Content-anchored patch: the landing is decided by the content itself, and
anchors are verified against the file — a mismatch refuses rather than lands
wrong. Reach for bash (sed, perl, tr, awk) when a pattern or position alone
defines the change: regex substitution, line ranges, character translation.
Rows are marked: `-` matches and deletes, `=` matches and keeps, `+` adds new
content. One operation is a run of marked rows whose `-`/`=` rows match the
file exactly once — the content is the anchor, so nothing here needs a line
number and nothing drifts. Sections name a file:

[path/to/file.rs]

-replace_me();
+replaced();

=    let mut out = String::new();
+    out.reserve(4096);

@fn trimmed()
-stale_body();

Forms: `-` rows then `+` rows replace; `=` then `+` inserts after the anchor;
`+` then `=` inserts before it. `@opening-line` scopes the rows under it to
that construct; a `@` scope followed by one bare `-` deletes the whole
construct, and followed by a single `-text` sweeps every row in it containing
`text` — with no scope the sweep runs file-wide, and the echo lists every row
it took. A `*` row names a construct as a point anchor: `+` rows above it
insert before the construct, `+` rows below it insert after. Blank lines separate
operations. Prefixes are exactly one character and the content is verbatim —
a row that starts with a marker character needs no escape (`--x` is `-`
marking `-x`).

Matching is all-or-nothing: an anchor matching nowhere is refused with the
closest real line; a mixed operation matching more than once is refused with
every candidate — widen the `=` context or add an `@` scope until it names
one place. The whole patch must still parse when applied, or everything is
refused. Nothing is written unless every section applies. File moves and
deletions belong to bash (mv, rm), not here.
"#;

// The description the model reads, with the address forms filled in from the
// table that defines them.
//
// Built rather than written out: this prose and the parser disagreeing is not
// hypothetical — it happened inside the commit that moved the grammar, and the
// stale line sat two functions away from the rewrite.
fn format() -> &'static str {
    SHAPE
}

// The constructs a view of the file opens with, keyed by first line.
fn construct_extents(path: &str, source: &str) -> HashMap<usize, (usize, usize)> {
    syntax::Lang::of(path).map_or_else(HashMap::new, |l| syntax::extents(l, source))
}
fn echo(path: &str, before: &str, content: &str, landed: &[Landed]) -> String {
    let mut out = hashline::header(path);
    // A number that opened the wrong construct still lands and parses — the
    // echo names what it covered, making the miss visible.
    let old: Vec<&str> = before.lines().collect();
    let extents = construct_extents(path, before);
    let resolved: Vec<Option<String>> = landed
        .iter()
        .map(|l| {
            let (cs, ce) = *extents.get(&l.took_at)?;
            let covered = old.get(cs - 1..ce)?;
            (ce > cs
                && l.took.len() == covered.len()
                && l.took.iter().zip(covered).all(|(took, was)| took == was))
            .then(|| {
                format!(
                    "  covered the construct at lines {cs}-{ce}: `{}`\n",
                    crop(covered[0], 60)
                )
            })
        })
        .collect();
    if landed.iter().all(|l| l.gave() == 0) {
        // A patch of pure deletions lands nothing, and saying so describes what did
        // not happen. What did is the deletion, which is the whole point of the
        // patch and reads as failure when reported by its absence.
        let named: Vec<&String> = resolved.iter().flatten().collect();
        if !named.is_empty() {
            out.push('\n');
        }
        for what in named {
            out.push_str(what);
        }
        let gone = before
            .lines()
            .count()
            .saturating_sub(content.lines().count());
        out.push_str(&match gone {
            0 => " nothing moved\n".to_string(),
            1 => " removed 1 line\n".to_string(),
            n => format!(" removed {n} lines\n"),
        });
        // The rows themselves, numbered as they were: a sweep that took more
        // than the model meant shows up row by row, not as a bare count.
        let total: usize = landed.iter().map(|l| l.took.len()).sum();
        let mut shown = 0usize;
        for l in landed {
            for (i, row) in l.took.iter().enumerate() {
                if shown == 40 {
                    out.push_str(&format!("  … and {} more rows\n", total - shown));
                    return out;
                }
                out.push_str(&format!("{:>4} - {}\n", l.took_at + i, crop(row, 80)));
                shown += 1;
            }
        }
        return out;
    }
    let lines: Vec<&str> = content.lines().collect();
    out.push('\n');
    // Addressed the way a second edit would name it: the numbering moved, and
    // a construct that grew has a new end.
    let spans = crate::rows::spans(path, content);
    let row = |out: &mut String, n: usize| {
        if let Some(text) = lines.get(n - 1) {
            crate::rows::line(out, n, &spans, text);
        }
    };
    // Rendered once, then measured, then assembled. Not built-whole-and-thrown
    // away when it turns out too long, and not rendered twice to measure it
    // either: a row costs an allocation to spell, and `addr` is where it goes.
    let rendered: Vec<Vec<String>> = landed
        .iter()
        .map(|l| {
            (l.start..=l.end)
                .map(|n| {
                    let mut r = String::new();
                    row(&mut r, n);
                    r
                })
                .collect()
        })
        .collect();
    let total: usize = rendered.iter().flatten().map(String::len).sum::<usize>()
        + resolved.iter().flatten().map(String::len).sum::<usize>();
    if total <= ECHO_LIMIT {
        for (rows, star) in rendered.iter().zip(&resolved) {
            if let Some(what) = star {
                out.push_str(what);
            }
            rows.iter().for_each(|r| out.push_str(r));
        }
        return out;
    }
    for (rows, star) in rendered.iter().zip(&resolved) {
        if let Some(what) = star {
            out.push_str(what);
        }
        // Whole anyway, where eliding would not actually save rows.
        if rows.len() <= ECHO_ENDS * 2 + 1 {
            rows.iter().for_each(|r| out.push_str(r));
            continue;
        }
        rows[..ECHO_ENDS].iter().for_each(|r| out.push_str(r));
        out.push_str(&format!("… {} lines\n", rows.len() - ECHO_ENDS * 2));
        rows[rows.len() - ECHO_ENDS..]
            .iter()
            .for_each(|r| out.push_str(r));
    }
    out
}

// Refuse a patch that leaves a file the parser can no longer read.
//
// Only "parsed before, does not now" — never "does not parse". A file that is
// already broken is usually the reason an edit is happening, and refusing to
// touch it would strand the model with no way to repair it.
//
// This is what a line range costs and `N*` does not: the model resolves the
// closing line itself, and one off leaves an orphaned brace that applies
// cleanly. Nothing is written when it fires, so the whole patch stays undone.
//
// The message carries the hunk addresses against the file as it stands, and
// any hunk whose body nets a different brace count from the lines it replaces
// — the first thing to look at when the parse broke.
fn broke_syntax(plan: &hashline::Plan, loaded: &HashMap<String, String>) -> Option<String> {
    for change in &plan.changes {
        let (path, before, after, landed) = match change {
            Change::Write {
                path,
                content,
                landed,
            } => (path, loaded.get(path), content, Some(landed)),
        };
        let before = before.map_or("", String::as_str);
        let rows = crate::parses::broke_rows(path, Some(before), after);
        if let Some(row) = nearest_row(&rows, landed) {
            let text = crate::parses::row_text(after, row);
            // Numbered in the result, not in the file: nothing was written, so
            // the row is not one the model can go and read.
            let mut why = format!(
                "{path} would not parse: it did before this patch, and line \
                 {row} of what this one produces is `{text}`. A range that \
                 covers one line too few or too many does exactly this. \
                 Re-read and check where the construct actually ends. Nothing \
                 was written."
            );
            if let Some(landed) = landed {
                why.push('\n');
                why.push_str(&hunk_help(path, before, after, landed));
            }
            return Some(why);
        }
    }
    None
}

// Which break to name first: the one closest to a line this patch wrote. A
// stray brace makes the whole file one error node opening on row 1.
fn nearest_row(rows: &[usize], landed: Option<&Vec<Landed>>) -> Option<usize> {
    let Some(landed) = landed.filter(|l| !l.is_empty()) else {
        return rows.first().copied();
    };
    let distance = |row: &usize| {
        landed
            .iter()
            .map(|l| {
                let (lo, hi) = (l.start.min(l.end), l.start.max(l.end));
                row.saturating_sub(hi).max(lo.saturating_sub(*row))
            })
            .min()
            .unwrap_or(0)
    };
    rows.iter()
        .min_by_key(|row| (distance(row), **row))
        .copied()
}

// What the patch's own hunks point at, for a break that a bare "line N is
// `}`" leaves the model to hunt down by itself. Each hunk shows the lines it
// displaces (`took` — the file as it stands, since nothing has been written)
// and any whose body nets a different brace count from what it displaces —
// the shape an off-by-one range leaves behind.
fn hunk_help(_path: &str, before: &str, after: &str, landed: &[Landed]) -> String {
    // Hunks spelt out before the rest are summarised.
    const SHOWN: usize = 6;
    let new: Vec<&str> = after.lines().collect();
    let old: Vec<&str> = before.lines().collect();
    let mut out = String::from("The hunks, against the file as it stands:");
    let mut off = String::new();
    for l in landed.iter().take(SHOWN) {
        // The model's own numbering: nothing was written, so `took_at` is where
        // it addressed, where `start` is where the hunk would have landed.
        //
        // Whatever surface a hunk covers, its address prints the way the
        // grammar writes it — a single line as `N`, a span as `N-M` — so the
        // shapes the model sees in the help are the ones its parser takes.
        let at = l.took_at;
        let took_len = l.took.len().saturating_sub(1);
        let addr = if took_len == 0 {
            format!("{at}")
        } else {
            format!("{at}-{}", at + took_len)
        };
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
        let took: isize = l.took.iter().map(|s| brace_net(s)).sum();
        let gave: isize = hunk_rows(&new, l).iter().map(|s| brace_net(s)).sum();
        if took != gave {
            let mut line =
                format!("\n  {addr}: its body nets {gave}, the lines it displaces net {took}");
            // Where the construct the range opened at actually ends, read off
            // the file the model will address — the number it got wrong,
            // stated instead of left to re-derive.
            if let Some(e) = balanced_end(&old, at) {
                if e > at {
                    line.push_str(&format!(
                        "; it opens at {at} and balances at line {e} — cover to {e}"
                    ));
                } else {
                    // The displaced lines never opened a brace the body fails
                    // to close: the hunk itself is the problem, and naming
                    // the line as both open and close would read as a
                    line.push_str(&format!(
                        "; the imbalance sits at line {e} — replace or cut it"
                    ));
                }
            }
            off.push_str(&line);
        }
    }
    if let Some(rest) = landed.len().checked_sub(SHOWN).filter(|n| *n > 0) {
        out.push_str(&format!("\n  … {rest} more"));
    }
    if !off.is_empty() {
        out.push_str("\nBrace balance:");
        out.push_str(&off);
    }
    out
}
// Clamped to whatever `lines` actually holds: a range that reaches past the
// end, or starts at zero, shows what there is rather than panicking.
fn hunk_rows<'a, 'b>(lines: &'a [&'b str], l: &Landed) -> &'a [&'b str] {
    let start = l.start.saturating_sub(1).min(lines.len());
    &lines[start..l.end.min(lines.len()).max(start)]
}
fn brace_net(s: &str) -> isize {
    s.chars().fold(0, |n, c| match c {
        '{' => n + 1,
        '}' => n - 1,
        _ => n,
    })
}

// The first line at or after `start` where the running brace count stops
// being positive — where the construct that opens there actually ends.
fn balanced_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut net = 0isize;
    for (i, l) in lines.iter().enumerate().skip(start.saturating_sub(1)) {
        net += brace_net(l);
        if net <= 0 {
            return Some(i + 1);
        }
    }
    None
}

fn crop(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut t: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        t.push('…');
    }
    t
}

// What a person watching sees: the lines that went, and the lines that came.
//
// Separate from the report the model reads, which is a set of addresses it can
// edit against. "Where can I edit next" and "what just changed" are different
// questions, and only the second one has a reader.
//
// The first line rides beside the tool's name, so it carries the counts; the
// rest are the diff itself, each row carrying the file line it was or became,
// capped, because a display is not a transcript.
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
        };
        let lines: Vec<&str> = content.lines().collect();
        let mut row_lines: Vec<(char, usize, &str)> = Vec::new();
        for l in landed {
            let gave = hunk_rows(&lines, l);
            // A hunk whose body already matched changed nothing, and a diff
            // that shows it says something happened that did not.
            if l.took == gave {
                continue;
            }
            minus += l.took.len();
            plus += gave.len();
            // Removed rows are numbered in the file they left, added rows in
            // the one they joined: an earlier hunk's net change moves the two
            // apart.
            for (i, old) in l.took.iter().enumerate() {
                row_lines.push(('-', l.took_at + i, old));
            }
            for (i, new) in gave.iter().enumerate() {
                row_lines.push(('+', l.start + i, new));
            }
        }
        // Right-aligned so a three-digit row lines up with a two-digit one.
        let width = row_lines
            .iter()
            .map(|(_, n, _)| *n)
            .max()
            .map_or(1, |n| n.to_string().len());
        let rows: Vec<String> = row_lines
            .iter()
            .map(|(sign, n, text)| format!("{n:>width$} {sign} {text}"))
            .collect();
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
                "patch": { "type": "string", "description": "One or more [path] sections." },
            },
            "required": ["patch"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args_hinted(args, ARGS_HINT)?;
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
            ToolError::Patch(PatchError::Malformed, e.to_string())
        })?;

        // Held for the whole patch: two edits to one file in the same turn would
        // otherwise both read the same bytes, both pass their tag check, and
        // one change would vanish with no error to show for it.
        let mut guards = Vec::new();
        let mut loaded: HashMap<String, String> = HashMap::new();
        let mut reals: HashMap<String, std::path::PathBuf> = HashMap::new();
        let mut stale: Vec<String> = Vec::new();
        for path in patch.paths() {
            let real = ctx.workspace.resolve(path, self.tier())?;
            reals.insert(path.to_string(), real.clone());
            // read refuses these at the same ceiling: below, the whole file is
            // held three times over — content, plan and syntax tree.
            if let Ok(meta) = tokio::fs::metadata(&real).await
                && meta.len() > MAX_BYTES
            {
                return Err(ToolError::Invalid(over_limit(path, meta.len())));
            }
            guards.push(ctx.lock_file(&real).await);
            let content = tokio::fs::read_to_string(&real).await.map_err(|e| {
                ToolError::Invalid(format!(
                    "{path}: {e}. edit changes existing files; use write to create one"
                ))
            })?;
            // Read-before-edit, and the staleness note: both read off the
            // last view of this file, recorded by read/grep/write/edit.
            let viewed = ctx.viewed_hash(&real);
            let Some(viewed) = viewed else {
                return Err(ToolError::Invalid(format!(
                    "{path}: read it before editing it"
                )));
            };
            if viewed != hashline::view_hash(&content) {
                stale.push(path.to_string());
            }
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
                patch = %args.patch,
                "patch rejected"
            );
            ToolError::Patch(PatchError::Unbalanced, e.to_string())
        })?;

        if let Some(why) = broke_syntax(&plan, &loaded) {
            tracing::warn!(
                target: "pi::edit",
                stage = "syntax",
                error = %why,
                patch = %args.patch,
                "patch rejected"
            );
            return Err(ToolError::Patch(PatchError::Unbalanced, why));
        }

        let mut report = String::new();
        // Every change path was resolved and locked above; a miss means
        // hashline broke that contract — fail loudly, never re-resolve.
        let locked = |p: &String| -> std::path::PathBuf { reals[p].clone() };
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
                            "{} unchanged — the patch matches what is already there\n",
                            hashline::header(path)
                        ));
                        continue;
                    }
                    ctx.note_write(&reals[path]);
                    tokio::fs::write(locked(path), content).await?;
                    // The model just saw this change; the note is for outside drift.
                    ctx.note_view(&reals[path], &hashline::view_hash(content));
                    let before = loaded.get(path).map_or("", String::as_str);
                    report.push_str(&echo(path, before, content, landed));
                }
            }
        }
        if !stale.is_empty() {
            report.push_str(&format!(
                "\nfile changed since your last view: {}",
                stale.join(", ")
            ));
        }

        Ok(ToolOutput::text(report).with_preview(sketch(&plan.changes, &loaded)))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parse_break_shows_the_hunk_addresses() {
        let src = "fn f() {\n    a;\n}\n\nfn g() {";
        let landed = vec![Landed {
            start: 5,
            end: 5,
            took: vec!["fn g() {".into()],
            took_at: 5,
        }];
        let help = hunk_help("a.rs", src, src, &landed);
        assert!(help.contains("5"), "{help}");
        assert!(!help.contains("5-5"), "{help}");
        assert!(help.contains("fn g() {"), "{help}");
        assert!(!help.contains("Brace balance"), "{help}");
    }

    #[test]
    fn a_hunk_is_addressed_where_the_model_wrote_it_not_where_it_landed() {
        // An earlier hunk that grew moves every later one. Naming the landing
        // row hands back a number the model cannot find in the file it read.
        let before = "a\nb\nc\n";
        let after = "A\nA\nb\nC\n";
        let landed = vec![
            Landed {
                start: 1,
                end: 2,
                took: vec!["a".into()],
                took_at: 1,
            },
            Landed {
                start: 4,
                end: 4,
                took: vec!["c".into()],
                took_at: 3,
            },
        ];
        let help = hunk_help("a.rs", before, after, &landed);
        assert!(help.contains("\n  3: `c`"), "{help}");
        assert!(!help.contains("4: `c`"), "{help}");
    }

    #[test]
    fn a_body_with_one_brace_too_many_is_called_out() {
        let before = "fn f() {\n}\n\n";
        let after = "fn f() {\n}\n}\n";
        let landed = vec![Landed {
            start: 3,
            end: 3,
            took: vec![String::new()],
            took_at: 3,
        }];
        let help = hunk_help("a.rs", before, after, &landed);
        assert!(help.contains("Brace balance:"), "{help}");
        assert!(help.contains("nets -1"), "{help}");
        assert!(help.contains("the imbalance sits at line 3"), "{help}");
        // A blank line opens no construct, so `3*` would be `3` again — the
        // advice that cost a turn every time it was followed.
        assert!(!help.contains("3*"), "{help}");
    }

    #[test]
    fn a_short_range_is_told_where_the_construct_actually_ends() {
        let src = "fn a() {\n    1\n}\n\nfn b() {\n    2\n}\n";
        // The body was dropped but the close stayed outside the range, so the
        // displaced brace never balances; the help says where it does.
        let landed = vec![Landed {
            start: 5,
            end: 4,
            took: vec!["fn b() {".into()],
            took_at: 5,
        }];
        let help = hunk_help("a.rs", src, src, &landed);
        assert!(help.contains("opens at 5"), "{help}");
        assert!(help.contains("balances at line 7"), "{help}");
    }

    #[test]
    fn the_break_named_is_the_one_nearest_what_the_patch_wrote() {
        let landed = vec![Landed {
            start: 170,
            end: 174,
            took: vec!["x".into()],
            took_at: 170,
        }];
        // A stray brace reports the whole file as one error opening on row 1.
        assert_eq!(nearest_row(&[1, 171, 400], Some(&landed)), Some(171));
        // Distance decides, not order: row 1 loses to anything closer.
        assert_eq!(nearest_row(&[1, 300], Some(&landed)), Some(300));
        assert_eq!(nearest_row(&[1], Some(&landed)), Some(1));
        assert_eq!(nearest_row(&[1, 400], None), Some(1));
        assert_eq!(nearest_row(&[], Some(&landed)), None);
    }
}
