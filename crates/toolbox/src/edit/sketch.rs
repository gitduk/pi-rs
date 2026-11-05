//! The diff an edit reports: the rows it moved under `-`/`+`, aligned
//! against what it displaced, with enough kept rows to place them.

use super::edits::{self, Applied, Landed};
use similar::{Algorithm, DiffTag, capture_diff_slices_deadline};
use std::time::{Duration, Instant};
use tool::row::{Row, Sign, render};

// Rows kept either side of a change: of file the edit did not touch, enough
// to place it, few enough that the change stays the subject.
const CONTEXT: usize = 3;
// Changed rows the model is shown before the rest are counted: a window this
// wide fits what one edit meant. The screen's copy is uncut.
pub(crate) const CHANGED_RUN: usize = 40;
// A rewrite this far apart costs more to align than it says: its rows are
// read as the block they replaced, whole.
const ALIGN: Duration = Duration::from_millis(50);

// Clamped to whatever `lines` actually holds: a range that reaches past the
// end, or starts at zero, shows what there is rather than panicking.
pub(super) fn hunk_rows<'a, 'b>(lines: &'a [&'b str], l: &Landed) -> &'a [&'b str] {
    let start = l.start.saturating_sub(1).min(lines.len());
    &lines[start..l.end.min(lines.len()).max(start)]
}

// The rows of one landing, under the marks a diff reads by.
fn kept(n: usize, text: String) -> Row {
    Row::Line {
        sign: Sign::Kept,
        n,
        text,
    }
}

fn gone(n: usize, text: String) -> Row {
    Row::Line {
        sign: Sign::Gone,
        n,
        text,
    }
}

fn come(n: usize, text: String) -> Row {
    Row::Line {
        sign: Sign::Come,
        n,
        text,
    }
}

fn sign_of(row: &Row) -> Option<Sign> {
    match row {
        Row::Line { sign, .. } => Some(*sign),
        _ => None,
    }
}

fn is_kept(row: &Row) -> bool {
    matches!(
        row,
        Row::Line {
            sign: Sign::Kept,
            ..
        }
    )
}

// A blank kept row; runs of these collapse to one marker.
fn is_blank(row: &Row) -> bool {
    matches!(row, Row::Line { sign: Sign::Kept, text, .. } if text.trim().is_empty())
}

// How a landing's rows line up: what it displaced, what it put there, and the
// rows it left alone — in file order, so each change keeps its surroundings.
fn aligned(at: usize, took: &[String], start: usize, gave: &[&str], out: &mut Vec<Row>) {
    // Rows this diff aligns, and the numbers they carry: the old file for a
    // row that went, the new one for a row that came or stayed.
    let was: Vec<&str> = took.iter().map(String::as_str).collect();
    // A run of moved rows reads top to bottom whatever order the ops arrive
    // in: what went, then what came, with the kept rows between.
    let base = out.len();
    let deadline = Instant::now().checked_add(ALIGN);
    for op in capture_diff_slices_deadline(Algorithm::Myers, &was, gave, deadline) {
        if op.tag() == DiffTag::Equal {
            out.extend(op.new_range().map(|j| kept(start + j, gave[j].to_string())));
            continue;
        }
        out.extend(op.old_range().map(|i| gone(at + i, was[i].to_string())));
        out.extend(op.new_range().map(|j| come(start + j, gave[j].to_string())));
    }
    let mut k = base;
    while k < out.len() {
        let end = out[k..]
            .iter()
            .position(is_kept)
            .map_or(out.len(), |p| k + p);
        out[k..end].sort_by_key(|row| sign_of(row) != Some(Sign::Gone));
        k = end + 1;
    }
}

// The rows of the file no landing claimed, from `from` to `to`: an empty range
// shows nothing.
fn untouched(out: &mut Vec<Row>, lines: &[&str], from: usize, to: usize) {
    out.extend((from..=to).map(|n| kept(n, lines[n - 1].to_string())));
}

// What a landing changed, as the rows a screen draws: the file's head, then
// the rows it moved — `-` for what went, `+` for what came — with enough kept
// rows to place them; a changed run past `changed_run` rows is cut.
pub(crate) fn rows(path: &str, applied: &Applied, changed_run: usize) -> Vec<Row> {
    // Shown without the mark, like every other view of the file: a row carrying
    // an invisible character is a row an anchor copied from it cannot match.
    let lines: Vec<&str> = edits::split_bom(&applied.content).1.lines().collect();
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
        Row::Line { sign, n, .. } if *sign != Sign::Kept => seen.insert((*sign, *n)),
        _ => true,
    });
    // What the head says: the rows the edit moved, not the rows it shows.
    let count = |moved: Sign| rows.iter().filter(|r| sign_of(r) == Some(moved)).count();
    let (plus, minus) = (count(Sign::Come), count(Sign::Gone));
    // A run of changed rows is read from its top; context only from the ends
    // a change borders, since the file's own head is a thousand rows away.
    let mut shown: Vec<Row> = Vec::with_capacity(rows.len());
    let runs: Vec<&[Row]> = rows.chunk_by(|a, b| is_kept(a) == is_kept(b)).collect();
    // A sole run of kept rows has no change to border it — an edit that moved
    // a newline alone — so both its ends are windowed, or neither would show.
    let sole = runs.len() == 1;
    for (i, run) in runs.iter().enumerate() {
        let (head, tail) = if is_kept(&run[0]) {
            let end = |bordered: bool| if bordered { CONTEXT } else { 0 };
            (end(sole || i > 0), end(sole || i + 1 < runs.len()))
        } else {
            (changed_run, 0)
        };
        // Fold only when it drops more than one row: a run one row past its
        // window is shown whole rather than trimmed.
        if run.len() <= head.saturating_add(tail).saturating_add(1) {
            shown.extend_from_slice(run);
            continue;
        }
        shown.extend_from_slice(&run[..head]);
        // A marker earns its row only where shown rows sit on both sides of
        // it, or where a changed run was cut short.
        if !is_kept(&run[0]) || (head > 0 && tail > 0) {
            shown.push(Row::Gap);
        }
        shown.extend_from_slice(&run[run.len() - tail..]);
    }
    shown.dedup_by(|a, b| is_blank(a) && is_blank(b));
    std::iter::once(Row::File {
        path: path.to_string(),
        plus,
        minus,
    })
    .chain(shown)
    .collect()
}

// The same rows as one text: what the model reads, and what a screen stored
// before the rows had a reader.
pub(super) fn sketch(path: &str, applied: &Applied, changed_run: usize) -> String {
    render(&rows(path, applied, changed_run))
}
