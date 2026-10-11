//! The diff an edit reports: the rows it moved under `-`/`+`, aligned
//! against what it displaced, with enough kept rows to place them.

use super::edits::{self, Applied, Landed};

// Rows kept either side of a change: of file the edit did not touch, enough
// to place it, few enough that the change stays the subject.
const CONTEXT: usize = 3;
// Changed rows shown before the rest are counted: the rows the edit wrote are
// what the reader came for, and a window this wide fits what one edit meant.
const CHANGED_RUN: usize = 40;
// Rows either side of a landing the line diff will align; past that the
// landing is shown whole, the alignment costing rows squared.
const DIFF_LINES: usize = 200;

// Clamped to whatever `lines` actually holds: a range that reaches past the
// end, or starts at zero, shows what there is rather than panicking.
pub(super) fn hunk_rows<'a, 'b>(lines: &'a [&'b str], l: &Landed) -> &'a [&'b str] {
    let start = l.start.saturating_sub(1).min(lines.len());
    &lines[start..l.end.min(lines.len()).max(start)]
}

// One row of a sketch: a file row under the sign that says which side of the
// edit it is on, or a mark where a long run left rows out.
#[derive(Clone, Copy)]
enum Row<'x> {
    // `n` numbers the row in the file it is read in: the old file for a row
    // that went, the new one for a row that came or stayed.
    Line { sign: char, n: usize, text: &'x str },
    // Rows left out; the jump in the numbers says how many.
    Gap,
}

impl<'x> Row<'x> {
    fn kept(n: usize, text: &'x str) -> Self {
        Self::Line { sign: ' ', n, text }
    }

    fn gone(n: usize, text: &'x str) -> Self {
        Self::Line { sign: '-', n, text }
    }

    fn come(n: usize, text: &'x str) -> Self {
        Self::Line { sign: '+', n, text }
    }

    fn is_kept(&self) -> bool {
        matches!(self, Self::Line { sign: ' ', .. })
    }

    // A blank kept row; runs of these collapse to one marker.
    fn is_blank(&self) -> bool {
        matches!(self, Self::Line { sign: ' ', text, .. } if text.trim().is_empty())
    }

    // The row's sign, used to count moved rows and split runs apart.
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

// The moved rows: `-` for what went, `+` for what came, with enough kept
// rows to place them. Rendered once, so model and UI show the same bytes.
pub(super) fn sketch(path: &str, applied: &Applied) -> String {
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
        Row::Line { sign, n, .. } if *sign != ' ' => seen.insert((*sign, *n)),
        _ => true,
    });
    // What the head says: the rows the edit moved, not the rows it shows.
    let count = |mark: char| rows.iter().filter(|r| r.has(mark)).count();
    let (plus, minus) = (count('+'), count('-'));
    // A run of changed rows is read from its top; context only from the ends
    // a change borders, since the file's own head is a thousand rows away.
    let mut shown: Vec<Row> = Vec::with_capacity(rows.len());
    let runs: Vec<&[Row]> = rows.chunk_by(|a, b| a.is_kept() == b.is_kept()).collect();
    // A sole run of kept rows has no change to border it — an edit that moved
    // a newline alone — so both its ends are windowed, or neither would show.
    let sole = runs.len() == 1;
    for (i, run) in runs.iter().enumerate() {
        let (head, tail) = if run[0].is_kept() {
            let end = |bordered: bool| if bordered { CONTEXT } else { 0 };
            (end(sole || i > 0), end(sole || i + 1 < runs.len()))
        } else {
            (CHANGED_RUN, 0)
        };
        // Fold only when it drops more than one row: a run one row past its
        // window is shown whole rather than trimmed.
        if run.len() <= head + tail + 1 {
            shown.extend_from_slice(run);
            continue;
        }
        shown.extend_from_slice(&run[..head]);
        // A marker earns its row only where shown rows sit on both sides of
        // it, or where a changed run was cut short.
        if !run[0].is_kept() || (head > 0 && tail > 0) {
            shown.push(Row::Gap);
        }
        shown.extend_from_slice(&run[run.len() - tail..]);
    }
    shown.dedup_by(|a, b| a.is_blank() && b.is_blank());
    // Right-aligned so a three-digit row lines up with a two-digit one, and
    // the mark sits in the number column, apart from any text.
    let width = shown
        .iter()
        .filter_map(|r| match r {
            Row::Line { n, .. } => Some(*n),
            Row::Gap => None,
        })
        .max()
        .map_or(1, |n| n.to_string().len());
    let body: Vec<String> = shown
        .iter()
        .map(|r| match r {
            Row::Line { sign, n, text } => format!("{sign}{n:>width$} {text}"),
            Row::Gap => format!(" {:>width$}", '⋮'),
        })
        .collect();
    std::iter::once(format!("{path} +{plus} -{minus}"))
        .chain(body)
        .collect::<Vec<_>>()
        .join("\n")
}
