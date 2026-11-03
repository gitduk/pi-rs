//! Styled text wrapping shared by block layout and terminal drawing.

use std::ops::Range;

use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};

/// Wraps a line to rows of at most `width`, ending rows where words do.
/// Escape sequences (tool output noise) take no columns and are dropped.
///
/// Nothing is dropped but what a `\r` overwrote: a selection maps columns
/// back by row widths.
pub fn fit(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    lay(line, width, |run, row| {
        out.push(line_of(&run[row]).style(line.style))
    });
    out
}

/// How many rows `fit` makes of `line`, without making them.
pub fn rows(line: &Line<'_>, width: usize) -> usize {
    let mut n = 0;
    lay(line, width, |_, _| n += 1);
    n
}

/// `line` up to its first `\n`, cut at the column `width` ends on: for a
/// strip that is clipped, where a word boundary would only waste columns.
pub fn clip_line(line: &Line<'_>, width: usize) -> Line<'static> {
    let (run, ends) = cells(line);
    let first = &run[..ends.first().copied().unwrap_or(run.len())];
    let kept: Vec<Cell> = (first.iter().cloned())
        .scan(0usize, |used, c| {
            *used += c.w;
            (*used <= width).then_some(c)
        })
        .collect();
    line_of(&kept)
}

#[derive(Clone)]
struct Cell {
    text: String,
    style: RStyle,
    w: usize,
}

// The line's cells in order, and where each `\n` ends a run of them. An
// escape the outside world left in the content (tool output) is noise: no
// cells. A tab is expanded after grapheme measurement; after a `\r` the run
// starts over, as `text::screen_line` reads a progress line.
fn cells(line: &Line<'_>) -> (Vec<Cell>, Vec<usize>) {
    let mut chars = Vec::new();
    let (mut esc, mut run_start, mut returned) = (None, 0, false);
    for span in &line.spans {
        let style = line.style.patch(span.style);
        for c in span.content.chars() {
            if pi_store::text::escaping(&mut esc, c) {
                continue;
            }
            match c {
                '\n' => {
                    chars.push((c, style));
                    run_start = chars.len();
                    returned = false;
                }
                '\r' => returned = true,
                _ => {
                    if returned {
                        chars.truncate(run_start);
                        returned = false;
                    }
                    chars.push((c, style));
                }
            }
        }
    }
    let text: String = chars.iter().map(|(c, _)| c).collect();
    let (mut out, mut ends) = (Vec::new(), Vec::new());
    let (mut col, mut at) = (0, 0);
    for (_, g) in pi_store::text::graphemes(&text) {
        let style = chars[at].1;
        at += g.chars().count();
        if g == "\n" {
            ends.push(out.len());
            col = 0;
            continue;
        }
        let w = pi_store::text::grapheme_width(g, col);
        if g == "\t" {
            out.extend(std::iter::repeat_n(
                Cell {
                    text: " ".into(),
                    style,
                    w: 1,
                },
                w,
            ));
        } else if !g.chars().all(char::is_control) {
            out.push(Cell {
                text: g.into(),
                style,
                w,
            });
        }
        col += w;
    }
    (out, ends)
}

// Hands `row` each row of `line` as a range of the run it is cut from.
fn lay(line: &Line<'_>, width: usize, mut row: impl FnMut(&[Cell], Range<usize>)) {
    let width = width.max(1);
    let (run, ends) = cells(line);
    let mut start = 0;
    for end in ends.into_iter().chain([run.len()]) {
        rows_of(&run[start..end], width, &mut row);
        start = end;
    }
}

// Closing marks a row must not open with: they belong to what precedes them.
const NO_LEAD: &[char] = &[
    '，', '。', '、', '；', '：', '？', '！', '）', '」', '』', '》', '】', '”', '’', '…',
];

// Whether a row may end between `a` and `b`: after a space, or beside a wide
// character, whose script needs no spaces between its words.
fn breaks_between(a: &Cell, b: &Cell) -> bool {
    (a.text.chars().all(char::is_whitespace) || a.w > 1 || b.w > 1) && !b.text.starts_with(NO_LEAD)
}

// The rows of one run with no `\n` in it; at least one, maybe empty. A row
// ends at the last break that fits, or mid-word when the word alone is wider.
fn rows_of(run: &[Cell], width: usize, row: &mut impl FnMut(&[Cell], Range<usize>)) {
    let (mut start, mut used) = (0usize, 0usize);
    let mut last_break: Option<usize> = None;
    for k in 0..run.len() {
        if k > start && breaks_between(&run[k - 1], &run[k]) {
            last_break = Some(k);
        }
        while used + run[k].w > width && k > start {
            let end = last_break.take().unwrap_or(k);
            row(run, start..end);
            used = run[end..k].iter().map(|c| c.w).sum();
            start = end;
        }
        used += run[k].w;
    }
    row(run, start..run.len());
}

fn line_of(cells: &[Cell]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for cell in cells {
        match spans.last_mut() {
            Some(s) if s.style == cell.style => s.content.to_mut().push_str(&cell.text),
            _ => spans.push(Span::styled(cell.text.clone(), cell.style)),
        }
    }
    Line::from(spans)
}
