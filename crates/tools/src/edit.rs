use async_trait::async_trait;
use hashline::{Anchor, Applied, Landed, Refusal};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::read::{MAX_BYTES, over_limit};
use crate::{Ctx, EditError, Tier, Tool, ToolError, ToolOutput};

// Bytes of landed rows echoed before showing a hunk's ends instead: forty rows
// of `}` and forty of a wrapped call are the same budget only in bytes.
const ECHO_LIMIT: usize = 2_000;
// Rows kept at each end of a hunk once an echo is past ECHO_LIMIT.
const ECHO_ENDS: usize = 3;
// Rows a deletion lists before the rest are counted instead.
const DELETED_ROWS: usize = 40;
// Rows of file the edit did not touch that a sketch keeps either side of a
// change: enough to place it, few enough that the change stays the subject.
const CONTEXT: usize = 4;
// Rows either side of a landing the line diff will align; past that the
// landing is shown whole, the alignment costing rows squared.
const DIFF_LINES: usize = 200;

// What a call that gets the arguments wrong is told, so it resends the call
// instead of staring at a bare serde error for a field it never named.
const ARGS_HINT: &str = "`edit` takes `path` and `edits`: a list of replacements, each \
    naming what to find (`old_string`, or `insert_after`/`insert_before`) and what to put \
    there (`new_string`). Send the whole call again.";

const SHAPE: &str = r#"Replace text in one file, by naming the text to find.

`path` names the file. `edits` is a list, and every entry is matched against the
file as it is now — not against what an earlier entry left. Nothing is written
unless all of them land, and a refusal leaves the file untouched.

An entry names its anchor in exactly one of three ways:

- `old_string`: the text to replace — the literal text, not a regex.
- `insert_after`: new content goes immediately after this text.
- `insert_before`: new content goes immediately before it.

`new_string` is what goes there: the replacement, or, for the two inserts, just
the new content — do not repeat the anchor in it. An empty `new_string` deletes
what `old_string` matched; to delete whole lines, include their newlines, or the
rows are emptied and left blank.

The anchor must match once. Matching twice is refused with every place named,
unless `replace_all` — when a line is not unique, widen the anchor with the
lines around it. An anchor that ends with a newline lands the insert on the next
line; one that does not, lands it inside the line.

`whole_block: true` reads the anchor as the first line of a block — a function,
a class, a markdown section — and takes the whole block, so its body need not be
quoted. The report names the lines that covered.

Reach for bash (sed, perl) when a pattern alone defines the change, and for
write when the file does not exist yet.
"#;

#[derive(Deserialize)]
struct Args {
    path: String,
    edits: Vec<EditArg>,
}

// One entry as the call spells it: the three anchors are optional here and
// refused unless exactly one is given, so a wrong call is answered, not lost.
#[derive(Deserialize)]
struct EditArg {
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    insert_after: Option<String>,
    #[serde(default)]
    insert_before: Option<String>,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    whole_block: bool,
}

// The shapes models send: `edits` as a JSON string, or a lone entry instead of
// a list of one. Accepted here, since refusing them costs a turn for nothing.
fn prepare(mut args: Value) -> Value {
    let Some(edits) = args.get_mut("edits") else {
        return args;
    };
    if let Some(text) = edits.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(text)
    {
        *edits = parsed;
    }
    if edits.is_object() {
        *edits = Value::Array(vec![std::mem::take(edits)]);
    }
    args
}

fn to_edits(path: &str, args: &[EditArg]) -> Result<Vec<hashline::Edit>, ToolError> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| {
            let anchor = match (&arg.old_string, &arg.insert_after, &arg.insert_before) {
                (Some(text), None, None) => Anchor::Replace(text.clone()),
                (None, Some(text), None) => Anchor::After(text.clone()),
                (None, None, Some(text)) => Anchor::Before(text.clone()),
                _ => {
                    let given = [&arg.old_string, &arg.insert_after, &arg.insert_before]
                        .into_iter()
                        .flatten()
                        .count();
                    return Err(ToolError::Edit(
                        EditError::Malformed,
                        format!(
                            "in {path}, edits[{index}]: {} — give exactly one of \
                             `old_string`, `insert_after`, `insert_before`",
                            match given {
                                0 => "no anchor".to_string(),
                                2 => "two anchors".to_string(),
                                n => format!("{n} anchors"),
                            }
                        ),
                    ));
                }
            };
            // `whole_block` needs a parser: a block that never opened would
            // send the model hunting for a line in a file nothing parses.
            if arg.whole_block && syntax::Lang::of(path).is_none() {
                return Err(ToolError::Edit(
                    EditError::Refused,
                    format!(
                        "in {path}, edits[{index}]: `whole_block` needs a parser and {path} has \
                         none — quote the block in `old_string` instead, or use `insert_after`"
                    ),
                ));
            }
            Ok(hashline::Edit {
                anchor,
                new: arg.new_string.clone(),
                replace_all: arg.replace_all,
                whole_block: arg.whole_block,
            })
        })
        .collect()
}

// The syntax break a change would leave behind, and what to look at.
//
// Only "parsed before, does not now" — never "does not parse": a file already
// broken is usually why an edit is happening, and refusing it strands the model.
fn broke_syntax(path: &str, before: &str, after: &str, landed: &[Landed]) -> Option<String> {
    let rows = crate::parses::broke_rows(path, Some(before), after);
    let row = nearest_row(&rows, landed)?;
    let text = crate::parses::row_text(after, row);
    let mut why = format!(
        "{path} would not parse: it did before this edit, and line {row} of what this one \
         produces is `{text}`. An anchor that covers a line too few or too many does exactly \
         this. Re-read and check where the block actually ends. Nothing was written."
    );
    why.push('\n');
    why.push_str(&hunk_help(before, after, landed));
    Some(why)
}

// Which break to name first: the one closest to a line this edit wrote. A
// stray brace makes the whole file one error node opening on row 1.
fn nearest_row(rows: &[usize], landed: &[Landed]) -> Option<usize> {
    if landed.is_empty() {
        return rows.first().copied();
    }
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

// What the landings point at, for a break a bare "line N is `}`" leaves the
// model to hunt: each hunk's displaced lines, and any body that nets otherwise.
fn hunk_help(before: &str, after: &str, landed: &[Landed]) -> String {
    // Hunks spelt out before the rest are summarised.
    const SHOWN: usize = 6;
    let new: Vec<&str> = after.lines().collect();
    let old: Vec<&str> = before.lines().collect();
    let mut out = String::from("The hunks, against the file as it stands:");
    let mut off = String::new();
    for l in landed.iter().take(SHOWN) {
        // The model's own numbering: nothing was written, so `took_at` is the
        // line the anchor sits on, where `start` is where it would have landed.
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
            // Where the construct the anchor opened at actually ends, read off
            // the file the model will address: the extent it got wrong, named.
            if let Some(e) = balanced_end(&old, at) {
                if e > at {
                    line.push_str(&format!(
                        "; it opens at {at} and balances at line {e} — cover to {e}"
                    ));
                } else {
                    // The displaced lines never opened a brace the body fails
                    // to close: the hunk is the problem, not the extent.
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

// The constructs a view of the file opens with, keyed by first line.
fn construct_extents(path: &str, source: &str) -> std::collections::HashMap<usize, (usize, usize)> {
    syntax::Lang::of(path).map_or_else(std::collections::HashMap::new, |l| {
        syntax::extents(l, source)
    })
}

/// The report the model reads: where the edit landed, and what it displaced.
fn echo(path: &str, before: &str, applied: &Applied) -> String {
    let landed = &applied.landed;
    let mut out = hashline::header(path);
    // A block taken whole that was not meant is the miss worth naming: the
    // extent, not the anchor, is what the model got wrong.
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
                    "  covered the block at lines {cs}-{ce}: `{}`\n",
                    crop(covered[0], 60)
                )
            })
        })
        .collect();

    if landed.iter().all(|l| l.gave() == 0) {
        // A pure-deletion report lands nothing, and saying so describes what
        // did not happen: what did is the deletion, and it needs saying.
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
            .saturating_sub(applied.content.lines().count());
        out.push_str(&match gone {
            0 => " nothing moved\n".to_string(),
            1 => " removed 1 line\n".to_string(),
            n => format!(" removed {n} lines\n"),
        });
        took_rows(landed, &mut out);
        return out;
    }

    let added: usize = landed.iter().map(|l| l.gave()).sum();
    if landed.iter().all(|l| l.took.is_empty()) {
        // Nothing was displaced: the count is the only thing the rows below
        // do not already say.
        out.push_str(&match added {
            1 => " added 1 line\n".to_string(),
            n => format!(" added {n} lines\n"),
        });
    } else {
        out.push('\n');
    }
    let lines: Vec<&str> = applied.content.lines().collect();
    // Addressed the way a second edit would name it: the numbering moved, and
    // a block that grew has a new end.
    let spans = crate::rows::spans(path, &applied.content);
    let row = |out: &mut String, n: usize| {
        if let Some(text) = lines.get(n - 1) {
            crate::rows::line(out, n, &spans, text);
        }
    };
    // Rendered once, then measured, then assembled: not built whole and thrown
    // away, and not rendered twice to measure, since a row costs an allocation.
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
    } else {
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
    }
    // A call that added rows may have taken some away as well, and those have
    // no row in the new file to be named by: they are listed as they were.
    let removed: usize = landed
        .iter()
        .filter(|l| l.gave() == 0)
        .map(|l| l.took.len())
        .sum();
    if removed > 0 {
        out.push_str(&match removed {
            1 => " removed 1 line\n".to_string(),
            n => format!(" removed {n} lines\n"),
        });
        took_rows(landed, &mut out);
    }
    out
}

// The rows a call took away, numbered as they were in the file it read.
//
// A hunk that gave nothing has no row in the new file to be named by, and a
// sweep that took more than the model meant shows up row by row.
fn took_rows(landed: &[Landed], out: &mut String) {
    let took: Vec<&Landed> = landed.iter().filter(|l| l.gave() == 0).collect();
    let total: usize = took.iter().map(|l| l.took.len()).sum();
    let mut shown = 0usize;
    for l in took {
        for (i, row) in l.took.iter().enumerate() {
            if shown == DELETED_ROWS {
                out.push_str(&format!("  … and {} more rows\n", total - shown));
                return;
            }
            out.push_str(&format!("{:>4} - {}\n", l.took_at + i, crop(row, 80)));
            shown += 1;
        }
    }
}

// The note a deletion earns when it emptied a line and left the row standing.
fn blank_note(edits: &[usize]) -> String {
    let named: Vec<String> = edits.iter().map(|i| format!("edits[{i}]")).collect();
    let said = if edits.len() == 1 {
        "emptied a line but left the line break, so a blank row stands where it was. \
         Include the trailing newline in the anchor to take the row with it."
    } else {
        "emptied the lines but left the line breaks, so blank rows stand where they \
         were. Include the trailing newline in the anchor to take the rows with them."
    };
    format!("\n{} {said}\n", named.join(", "))
}

// One row of a sketch: a file row under the sign that says which side of the
// edit it is on, or the count of the rows a long run of context left out.
#[derive(Clone, Copy)]
enum Row<'x> {
    // `n` numbers the row in the file it is read in: the old file for a row
    // that went, the new one for a row that came or stayed.
    Line { sign: char, n: usize, text: &'x str },
    Elided(usize),
}

impl<'x> Row<'x> {
    // A row of the file as it stands now that this edit did not touch.
    fn kept(n: usize, text: &'x str) -> Self {
        Self::Line { sign: ' ', n, text }
    }

    // A row the edit displaced, numbered in the file it left.
    fn gone(n: usize, text: &'x str) -> Self {
        Self::Line { sign: '-', n, text }
    }

    // A row the edit left behind that was not there before.
    fn come(n: usize, text: &'x str) -> Self {
        Self::Line { sign: '+', n, text }
    }

    // Whether this row is one the edit left standing.
    fn is_kept(&self) -> bool {
        matches!(self, Self::Line { sign: ' ', .. })
    }

    // Whether this row carries the sign `mark`: how the head counts the rows
    // the edit moved, and how one run is read apart from the next.
    fn has(&self, mark: char) -> bool {
        matches!(self, Self::Line { sign, .. } if *sign == mark)
    }
}

// How a landing's rows line up: what it displaced, what it put there, and the
// rows it left alone — in file order, so each change keeps its surroundings.
fn aligned<'x>(
    at: usize,
    took: &'x [String],
    start: usize,
    gave: &'x [&'x str],
    out: &mut Vec<Row<'x>>,
) {
    // The ends cannot differ under any alignment, and cutting them first keeps
    // the table below to the rows actually in question.
    let head = took
        .iter()
        .zip(gave)
        .take_while(|(was, is)| was == is)
        .count();
    let tail = took[head..]
        .iter()
        .rev()
        .zip(gave[head..].iter().rev())
        .take_while(|(was, is)| was == is)
        .count();
    // The rows the two ends agree on are context, and they are numbered as the
    // file the reader has now: the edit left them standing.
    out.extend((0..head).map(|k| Row::kept(start + k, gave[k])));
    // `at` and `start` now number the middle, the rows the sides do not share.
    let (at, start) = (at + head, start + head);
    let (n, m) = (took.len() - head - tail, gave.len() - head - tail);
    if n > DIFF_LINES || m > DIFF_LINES {
        // Past the cap the alignment costs more than it says, and a block this
        // size is read as the rows it displaced and the rows it put there.
        out.extend((0..n).map(|i| Row::gone(at + i, took[head + i].as_str())));
        out.extend((0..m).map(|j| Row::come(start + j, gave[head + j])));
    } else {
        let (took, gave) = (&took[head..head + n], &gave[head..head + m]);
        // The longest run of rows the two sides share, so a row that survives
        // the edit is not read as one that went and one that arrived.
        let stride = m + 1;
        let mut shared = vec![0u32; (n + 1) * stride];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                shared[i * stride + j] = if took[i] == gave[j] {
                    shared[(i + 1) * stride + j + 1] + 1
                } else {
                    shared[(i + 1) * stride + j].max(shared[i * stride + j + 1])
                };
            }
        }
        // The walk can interleave the two sides, so its runs are read after it:
        // what went first, then what came, with the shared rows between.
        let base = out.len();
        let (mut i, mut j) = (0usize, 0usize);
        while i < n && j < m {
            if took[i] == gave[j] {
                out.push(Row::kept(start + j, gave[j]));
                (i, j) = (i + 1, j + 1);
            } else if shared[(i + 1) * stride + j] >= shared[i * stride + j + 1] {
                out.push(Row::gone(at + i, took[i].as_str()));
                i += 1;
            } else {
                out.push(Row::come(start + j, gave[j]));
                j += 1;
            }
        }
        for (line, row) in (at + i..at + n).zip(&took[i..]) {
            out.push(Row::gone(line, row.as_str()));
        }
        for (line, row) in (start + j..start + m).zip(&gave[j..]) {
            out.push(Row::come(line, row));
        }
        let mut k = base;
        while k < out.len() {
            let end = out[k..]
                .iter()
                .position(Row::is_kept)
                .map_or(out.len(), |p| k + p);
            out[k..end].sort_by_key(|row| !row.has('-'));
            k = end + 1;
        }
    }
    out.extend((0..tail).map(|k| Row::kept(start + m + k, gave[gave.len() - tail + k])));
}

// The rows of the file no landing claimed, from `from` to `to`: an empty range
// shows nothing.
fn untouched<'x>(out: &mut Vec<Row<'x>>, lines: &[&'x str], from: usize, to: usize) {
    out.extend((from..=to).map(|n| Row::kept(n, lines[n - 1])));
}

// What a person watching sees: the rows that went, the rows that came, and
// enough of the rows that stayed to place them.
//
// Separate from the report the model reads, which is a set of addresses it can
// edit against — "what changed" is a different question from "where next".
fn sketch(path: &str, applied: &Applied) -> String {
    let lines: Vec<&str> = applied.content.lines().collect();
    let mut rows: Vec<Row> = Vec::new();
    let mut next = 1usize;
    for l in &applied.landed {
        untouched(&mut rows, &lines, next, l.start.saturating_sub(1));
        let gave = hunk_rows(&lines, l);
        aligned(l.took_at, &l.took, l.start, gave, &mut rows);
        next = l.end.saturating_add(1);
    }
    untouched(&mut rows, &lines, next, lines.len());
    // Two landings inside one line report that line twice, and a row read
    // twice is a row counted twice: each row is shown once.
    let mut seen = std::collections::HashSet::new();
    rows.retain(|row| match row {
        Row::Line { sign, n, .. } if *sign != ' ' => seen.insert((*sign, *n)),
        _ => true,
    });
    // What the head says: the rows the edit moved, not the rows it shows.
    let count = |mark: char| rows.iter().filter(|r| r.has(mark)).count();
    let (plus, minus) = (count('+'), count('-'));
    // A run of context longer than the window either side of a change is shown
    // at both its ends and counted in the middle, the way a reader skims it.
    let mut shown: Vec<Row> = Vec::with_capacity(rows.len());
    for run in rows.chunk_by(|a, b| a.is_kept() == b.is_kept()) {
        if !run[0].is_kept() || run.len() <= CONTEXT * 2 {
            shown.extend_from_slice(run);
        } else {
            shown.extend_from_slice(&run[..CONTEXT]);
            shown.push(Row::Elided(run.len() - CONTEXT * 2));
            shown.extend_from_slice(&run[run.len() - CONTEXT..]);
        }
    }
    // Right-aligned so a three-digit row lines up with a two-digit one, and a
    // counted run starts where the rows it stands for do.
    let width = shown
        .iter()
        .filter_map(|r| match r {
            Row::Line { n, .. } => Some(*n),
            Row::Elided(_) => None,
        })
        .max()
        .map_or(1, |n| n.to_string().len());
    let body: Vec<String> = shown
        .iter()
        .map(|r| match r {
            Row::Line { sign, n, text } => format!("{sign}{n:>width$} {text}"),
            Row::Elided(n) => format!("{}… {n} lines", " ".repeat(width + 2)),
        })
        .collect();
    std::iter::once(format!("{path} +{plus} -{minus}"))
        .chain(body)
        .collect::<Vec<_>>()
        .join("\n")
}

// The refusal, logged under the tag the loop groups repeats by and returned
// under the category the failure deserves: misspelt and off-target differ.
fn refuse(path: &str, refusal: &Refusal, count: usize) -> ToolError {
    tracing::warn!(
        target: "pi::edit",
        stage = "refuse",
        path,
        edits = count,
        reason = refusal.tag(),
        error = %refusal,
        "edit refused"
    );
    let kind = if refusal.is_shape() {
        EditError::Malformed
    } else {
        EditError::Refused
    };
    ToolError::Edit(kind, refusal.to_string())
}

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        SHAPE
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to edit.",
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more replacements. Each is matched against \
                                    the file as it is now, not against what an earlier \
                                    one left.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {
                                "type": "string",
                                "description": "Literal text to replace. Give exactly one \
                                                of old_string, insert_after, insert_before.",
                            },
                            "insert_after": {
                                "type": "string",
                                "description": "Literal text to put new_string immediately \
                                                after.",
                            },
                            "insert_before": {
                                "type": "string",
                                "description": "Literal text to put new_string immediately \
                                                before.",
                            },
                            "new_string": {
                                "type": "string",
                                "description": "The replacement, or for an insert the new \
                                                content alone — do not repeat the anchor. \
                                                Empty deletes what old_string matched.",
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "Take every match instead of refusing an \
                                                anchor that names several. Default false.",
                            },
                            "whole_block": {
                                "type": "boolean",
                                "description": "Read the anchor as the first line of a \
                                                block (a function, a class, a markdown \
                                                section) and take the whole block. Default \
                                                false.",
                            },
                        },
                        "required": ["new_string"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["path", "edits"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args_hinted(prepare(args), ARGS_HINT)?;
        let path = args.path.as_str();
        let edits = to_edits(path, &args.edits)?;
        let real = ctx.workspace.resolve(path, self.tier())?;
        // read refuses these at the same ceiling: below, the whole file is held
        // twice over — content and syntax tree.
        if let Ok(meta) = tokio::fs::metadata(&real).await
            && meta.len() > MAX_BYTES
        {
            return Err(ToolError::Invalid(over_limit(path, meta.len())));
        }
        // Held across the read and the write: two edits to one file in a turn
        // would both read the same bytes, and one would vanish with no error.
        let _guard = ctx.lock_file(&real).await;
        let content = tokio::fs::read_to_string(&real).await.map_err(|e| {
            let hint = match e.kind() {
                // Only the file that is not there earns the pointer to write.
                // One that is not text is never rewritten as text.
                std::io::ErrorKind::NotFound => {
                    ". edit changes existing files; use write to create one"
                }
                std::io::ErrorKind::InvalidData => {
                    ". edit is for text; convert it first, or leave it alone"
                }
                _ => "",
            };
            ToolError::Invalid(format!("{path}: {e}{hint}"))
        })?;
        // Read-before-edit, and the staleness note: both read off the last view
        // of this file, recorded by read/grep/write/edit.
        let Some(viewed) = ctx.viewed_hash(&real) else {
            return Err(ToolError::Invalid(format!(
                "{path}: read it before editing it"
            )));
        };
        let stale = viewed != hashline::view_hash(&content);

        // Nothing has touched the disk yet: a refused call leaves no trace.
        let applied = hashline::apply(path, &content, &edits, &crate::blocks::TreeSitter)
            .map_err(|e| refuse(path, &e, edits.len()))?;
        if let Some(why) = broke_syntax(path, &content, &applied.content, &applied.landed) {
            tracing::warn!(
                target: "pi::edit",
                stage = "syntax",
                path,
                edits = edits.len(),
                "edit refused"
            );
            return Err(ToolError::Edit(EditError::Refused, why));
        }

        ctx.note_write(&real);
        crate::write::atomic_write(&real, applied.content.as_bytes()).await?;
        // The model just saw this change; the note is for outside drift.
        ctx.note_view(&real, &hashline::view_hash(&applied.content));
        // What the tool is being asked to do, by shape: the one number that
        // says whether the insert anchors are earning their place.
        let forms = |want: fn(&Anchor) -> bool| edits.iter().filter(|e| want(&e.anchor)).count();
        tracing::info!(
            target: "pi::edit",
            stage = "apply",
            path,
            edits = edits.len(),
            replace = forms(|a| matches!(a, Anchor::Replace(_))),
            after = forms(|a| matches!(a, Anchor::After(_))),
            before = forms(|a| matches!(a, Anchor::Before(_))),
            blocks = edits.iter().filter(|e| e.whole_block).count(),
            "edit applied"
        );

        let mut report = echo(path, &content, &applied);
        if !applied.left_blank.is_empty() {
            report.push_str(&blank_note(&applied.left_blank));
        }
        if stale {
            report.push_str(&format!("\nfile changed since your last view: {path}"));
        }
        Ok(ToolOutput::text(report).with_preview(sketch(path, &applied)))
    }
}
