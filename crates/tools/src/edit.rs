use async_trait::async_trait;
use hashline::{Change, Landed};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;

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
    one or more `[path#TAG]` sections, each a header line followed by op lines \
    like `PUT 3:`. Send the whole call again with `patch`.";

#[derive(Deserialize)]
struct Args {
    patch: String,
}

const SHAPE: &str = r#"Line-anchored patch. Sections name a file and the TAG from your last read of it:

[path/to/file.rs#A1B2]
PUT 2-4:
+replacement line one
+replacement line two

Addresses. `N` is a single line, `N-M` two or more, `N*` a construct, and
every number is the ORIGINAL one from that read: earlier hunks in the same
patch never shift later ones. `N-N` is not an address — a single line is `N`.
{addresses}

Direction belongs to PUT, not to an address: `PUT 4:UP` inserts above line 4,
`PUT 4:DOWN` below it, `PUT 3-5:DOWN` below the range, `PUT 2*:DOWN` past where
the construct closes.

Ops. Every op line starts with a verb — PUT, CUT, MV or RM. A header ending in
`:` takes `+` body rows; CUT, MV, RM and the register pastes take none.
  PUT <addr>:                  put the body there; the address alone replaces
  PUT <addr>:UP / <addr>:DOWN  insert the body above / below it
  PUT <addr>:UP @name / <addr>:DOWN @name / <addr> @name   paste a captured register
  CUT N-M                      delete those lines, capturing them; `@name` labels it
  CUT N*                       the same, for a whole construct
  MV dest                      rename; edits in this section land first, then the file moves
  RM                           delete the file; may not share a section with other ops

  (`@` alone is the anonymous register, filled by whatever unlabeled CUT ran
  before it.)

Cheapest first: insert with `:UP` / `:DOWN` — `1:UP` is the file head, the last
line's `:DOWN` the file tail — delete with CUT, address only the lines that
change, and leave unchanged lines out of the body. PUT N* is for a
mostly-rewritten construct, not a one-line change.

Every hunk in one patch names the numbers your last read showed, so six places
changing is one call with six hunks. A second call is not wrong, but its
addresses must come from what the first one printed back, not from that read:
the lines below an edit that changed a line count have all moved.

Body rows start with `+` and are copied verbatim, so `+` alone is a blank line
and leading whitespace is preserved. Never write `-old` or bare context lines:
the address says what goes, the body says what arrives. To delete lines and put
nothing back, use CUT — not a PUT with `-` rows. A literal line of your own that
begins with `-` or `+` takes the prefix like any other: `- item` is written
`+- item`. A body may be any length regardless of how many lines the address
names.

Rejected outright: a stale TAG, an address a previous edit renumbered, two hunks
touching the same original line, an address past the end of the file, and a
patch that would leave the file unparseable when it parsed before. Nothing is
written unless every section applies."#;

// One table row, wrapped under its own label rather than running off the side.
//
// The description is read by a model on every request; a paragraph that used
// to wrap and now does not is a real cost of generating prose instead of
// writing it.
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

// The description the model reads, with the address forms filled in from the
// table that defines them.
//
// Built rather than written out: this prose and the parser disagreeing is not
// hypothetical — it happened inside the commit that moved the grammar, and the
// stale line sat two functions away from the rewrite.
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
    let mut out = hashline::header(path, &hashline::tag(content));
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
    let total: usize = rendered.iter().flatten().map(String::len).sum();
    if total <= ECHO_LIMIT {
        rendered.iter().flatten().for_each(|r| out.push_str(r));
        return out;
    }
    for rows in &rendered {
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

// `N*` spelt out, but only where it resolves to more than the row itself.
// Advice is followed, and `N*` on a one-row construct is `N` under another
// name — a whole turn spent restating the failure.
fn star(extents: &HashMap<usize, (usize, usize)>, line: usize) -> String {
    match extents.get(&line) {
        Some((start, end)) if end > start => format!(" or use `{line}*`"),
        _ => String::new(),
    }
}

// What the patch's own hunks point at, for a break that a bare "line N is
// `}`" leaves the model to hunt down by itself. Each hunk shows the lines it
// displaces (`took` — the file as it stands, since nothing has been written)
// and any whose body nets a different brace count from what it displaces —
// the shape an off-by-one range leaves behind.
fn hunk_help(path: &str, before: &str, after: &str, landed: &[Landed]) -> String {
    // Hunks spelt out before the rest are summarised.
    const SHOWN: usize = 6;
    let new: Vec<&str> = after.lines().collect();
    let old: Vec<&str> = before.lines().collect();
    let mut out = String::from("The hunks, against the file as it stands:");
    let mut off = String::new();
    // One parse for the whole message: the advice checks itself against what
    // `N*` would actually resolve to, once per hunk.
    let extents = syntax::Lang::of(path).map_or_else(HashMap::new, |l| syntax::extents(l, before));
    for l in landed.iter().take(SHOWN) {
        // The model's own numbering: nothing was written, so `took_at` is where
        // it addressed, where `start` is where the hunk would have landed.
        //
        // Whatever surface a hunk covers, its address prints the way the
        // grammar writes it — a single line as `N`, a span as `N-M` — so the
        // shapes the model sees in the help are the ones its parser takes.
        let at = l.took_at;
        let addr = hashline::Target::Range {
            start: at,
            end: at + l.took.len().saturating_sub(1),
        }
        .to_string();
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
                    let star = star(&extents, at);
                    line.push_str(&format!(
                        "; it opens at {at} and balances at line {e} — cover to {e}{star}"
                    ));
                } else {
                    // The displaced lines never opened a brace the body fails
                    // to close: the hunk itself is the problem, and naming
                    // the line as both open and close would read as a
                    // contradiction.
                    let star = star(&extents, e);
                    line.push_str(&format!(
                        "; the imbalance sits at line {e} — replace or cut it{star}"
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
    &lines[l.start.saturating_sub(1).min(lines.len())..l.end.min(lines.len())]
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

// Refuse addresses this session's own earlier edits moved, and show the rows
// they now sit on.
//
// An edit hands back the new tag, so the tag check passes while the numbers
// behind it point one row off. Rejecting alone would cost a read turn every
// time, and a second edit with no read between is a capability worth keeping —
// so the refusal carries the numbering itself and clears the mark, leaving the
// model to resend against what it has just been shown.
fn renumbered(
    patch: &hashline::Patch,
    reals: &HashMap<String, std::path::PathBuf>,
    loaded: &HashMap<String, String>,
    ctx: &Ctx,
) -> Option<String> {
    const AROUND: usize = 3;
    let mut out = String::new();
    for section in &patch.sections {
        let Some(real) = reals.get(&section.path) else {
            continue;
        };
        let Some(from) = ctx.shifted_from(real) else {
            continue;
        };
        let content = loaded.get(&section.path).map_or("", String::as_str);
        // A tag that no longer matches is a bigger problem than numbering, and
        // saying so is `apply`'s job; nothing here should pre-empt it.
        if section.tag != hashline::tag(content) {
            continue;
        }
        // The highest line any op reaches, not the lowest: one hunk below the
        // shift does not make the ones above it safe, and a range is unsafe as
        // soon as either end is.
        let Some(highest) = section.ops.iter().filter_map(op_span).map(|(_, e)| e).max() else {
            continue;
        };
        if highest < from {
            continue;
        }
        ctx.forget_shift(real);
        let lines: Vec<&str> = content.lines().collect();
        let spans = crate::rows::spans(&section.path, content);
        out.push_str(&format!(
            "{} was renumbered from line {from} on by your own last edit — the TAG is \
             current, the line numbers are not. Around the lines this patch names:\n",
            section.path
        ));
        let mut shown: Vec<usize> = section
            .ops
            .iter()
            .filter_map(op_span)
            // Both ends clamped into the file: an edit that shortened it is
            // exactly when an address runs past the end, and the tail is what
            // the model needs to see there — not an empty window.
            .flat_map(|(s, e)| {
                let (s, e) = (s.min(lines.len()), e.min(lines.len()));
                s.saturating_sub(AROUND).max(1)..=(e + AROUND).min(lines.len())
            })
            .collect();
        shown.sort_unstable();
        shown.dedup();
        // Rendered before it is spent, so the budget is the bytes this actually
        // costs rather than a guess at them. A patch whose hunks span most of a
        // file asks for most of the file back, and this message exists to save
        // a read turn, not to be one.
        let rendered: Vec<(usize, String)> = shown
            .into_iter()
            .map(|n| {
                let mut row = String::new();
                crate::rows::line(&mut row, n, &spans, lines[n - 1]);
                (n, row)
            })
            .collect();
        let keep = crate::rows::fits(rendered.iter(), |(_, r)| r.len(), ECHO_LIMIT);
        let mut last = 0;
        for (n, row) in rendered.iter().take(keep) {
            if *n > last + 1 {
                out.push_str(crate::rows::GAP);
            }
            out.push_str(row);
            last = *n;
        }
        if keep < rendered.len() {
            out.push_str(crate::rows::GAP);
        }
        out.push_str("Rebuild the hunks against these numbers and send it again.");
    }
    (!out.is_empty()).then_some(out)
}

// The original lines an op names, or None for one that names none.
//
// A `N*` is counted at N alone: where its construct closes is the resolver's
// answer and this runs before the resolver. Under-reaching there costs a
// refusal that did not fire, never one that fired wrongly.
fn op_span(op: &hashline::Op) -> Option<(usize, usize)> {
    use hashline::{LinePos, Op, Target};
    let of = |t: &Target| match *t {
        Target::Range { start, end } => (start, end),
        Target::Block { line } => (line, line),
    };
    match op {
        Op::Replace { target, .. } | Op::Cut { target, .. } => Some(of(target)),
        // `1:UP` is the file head however the numbering moved.
        Op::InsertBefore { line: 1, .. } => None,
        Op::InsertBefore { line, .. } => Some((*line, *line)),
        Op::InsertAfter { at, .. } => Some(match at {
            LinePos::At(n) | LinePos::AfterBlock(n) => (*n, *n),
        }),
        Op::Remove | Op::Move { .. } => None,
    }
}

// What each file's tag is right now, for a refusal that turned on one.
fn tags(loaded: &HashMap<String, String>) -> String {
    let mut out: Vec<String> = loaded
        .iter()
        .map(|(p, c)| hashline::header(p, &hashline::tag(c)))
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
        for path in patch.paths() {
            let real = ctx.workspace.resolve(path, self.tier())?;
            reals.insert(path.to_string(), real.clone());
            guards.push(ctx.lock_file(&real).await);
            let content = tokio::fs::read_to_string(&real).await.map_err(|e| {
                ToolError::Invalid(format!(
                    "{path}: {e}. edit changes existing files; use write to create one"
                ))
            })?;
            loaded.insert(path.to_string(), content);
        }

        if let Some(why) = renumbered(&patch, &reals, &loaded, ctx) {
            tracing::warn!(
                target: "pi::edit",
                stage = "shift",
                error = %why,
                patch = %args.patch,
                "patch rejected"
            );
            return Err(ToolError::Patch(PatchError::Renumbered, why));
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

        // After the guards, since a rejected patch wrote nothing and moved no
        // line. The report below hands back a fresh tag; this is what stops
        // that tag from vouching for numbering it does not cover.
        // Bookkeeping runs beside each write, never ahead of the batch: a
        // failure partway through would otherwise leave the tracker describing
        // a file that was never written.
        //
        // Both ends of a rename, since the model's numbers for the old path are
        // the only ones it has for the new one. A path that no longer holds the
        // file it was noted for is forgotten instead, or the note outlives the
        // content and refuses edits to whatever is written there next.
        let track = |change: &Change, gone: &[&String], moved: &[&String]| {
            let resolve = |path: &&String| match reals.get(*path) {
                // Every path but a rename's destination is already resolved.
                Some(real) => Some(real.clone()),
                None => ctx.workspace.resolve(path, self.tier()).ok(),
            };
            for real in gone.iter().filter_map(resolve) {
                ctx.forget_shift(&real);
            }
            if let Some(from) = hashline::first_shifted_line(std::slice::from_ref(change)) {
                for real in moved.iter().filter_map(resolve) {
                    ctx.note_shift(&real, from);
                }
            }
        };

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
                            "{} unchanged — the patch matches what is already there\n",
                            hashline::header(path, &hashline::tag(content))
                        ));
                        continue;
                    }
                    tokio::fs::write(ctx.workspace.resolve(path, self.tier())?, content).await?;
                    track(change, &[], &[path]);
                    let before = loaded.get(path).map_or("", String::as_str);
                    report.push_str(&echo(path, before, content, landed));
                }
                Change::Remove { path } => {
                    tokio::fs::remove_file(ctx.workspace.resolve(path, self.tier())?).await?;
                    track(change, &[path], &[]);
                    report.push_str(&format!("removed {path}\n"));
                }
                Change::Rename {
                    from,
                    to,
                    content,
                    landed,
                } => {
                    let dest = ctx.workspace.resolve(to, self.tier())?;
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&dest, content).await?;
                    tokio::fs::remove_file(ctx.workspace.resolve(from, self.tier())?).await?;
                    track(change, &[from], &[to]);
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
        // Here `5*` does resolve, so it is worth offering.
        assert!(help.contains("use `5*`"), "{help}");
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
