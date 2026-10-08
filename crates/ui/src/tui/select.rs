//! Text selected with the mouse in the transcript: dragged, or a word or a
//! line clicked twice or thrice. Read off the drawn rows, frames and borders
//! left out and wrapped rows joined back into the line they came from.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// A cell of the transcript region: its row there, and its column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Point {
    pub(super) row: usize,
    pub(super) col: usize,
}

/// From where the press landed to where the pointer is, either way round.
#[derive(Clone, Copy, Debug)]
pub(super) struct Selection {
    pub(super) anchor: Point,
    pub(super) head: Point,
}

impl Selection {
    // Start and end in reading order; the end column is exclusive.
    fn span(&self) -> (Point, Point) {
        let (a, b) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        (
            a,
            Point {
                col: b.col + 1,
                ..b
            },
        )
    }

    // The columns of `row` it covers, if any.
    fn cols(&self, row: usize) -> Option<(usize, usize)> {
        let (from, to) = self.span();
        if row < from.row || row > to.row {
            return None;
        }
        let start = if row == from.row { from.col } else { 0 };
        let end = if row == to.row { to.col } else { usize::MAX };
        Some((start, end))
    }
}

/// One drawn row as selection reads it.
pub(super) struct Drawn {
    pub(super) text: String,
    /// The logical line it shows part of; rows sharing one are its wraps.
    pub(super) line: Option<(usize, usize)>,
    /// Columns of frame or margin before the text, never copied.
    pub(super) lead: usize,
}

/// The selected text: each row from past its lead, a wrapped row joined to
/// the next without a break, padding at a line's end left off.
pub(super) fn text(sel: &Selection, rows: &[Drawn], width: usize) -> String {
    let mut out = String::new();
    let (from, to) = sel.span();
    for at in from.row..=to.row.min(rows.len().saturating_sub(1)) {
        let row = &rows[at];
        let Some((start, end)) = sel.cols(at) else {
            continue;
        };
        let piece = cols(&row.text, start.max(row.lead), end);
        let next = rows.get(at + 1);
        // A row filled to the edge and followed by more of its line wrapped
        // there; a shorter one ended where the line did.
        let wrapped = row.line.is_some()
            && next.is_some_and(|n| n.line == row.line)
            && row.text.trim_end().chars().map(cell).sum::<usize>() + 1 >= width;
        if wrapped && at < to.row {
            out.push_str(&piece);
        } else {
            out.push_str(piece.trim_end());
            if at < to.row {
                out.push('\n');
            }
        }
    }
    out
}

fn cell(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// The characters of `s` whose cells start in `from..to`.
pub(super) fn cols(s: &str, from: usize, to: usize) -> String {
    let mut at = 0;
    let mut out = String::new();
    for c in s.chars() {
        if at >= from && at < to {
            out.push(c);
        }
        at += cell(c);
    }
    out
}

/// The word under `col` in `s`, as a column range: letters, digits and `_`
/// run together, so does a run of CJK; anything else is a word of one.
pub(super) fn word_at(s: &str, col: usize) -> (usize, usize) {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut cells: Vec<(usize, char)> = Vec::new();
    let mut at = 0;
    for c in s.chars() {
        cells.push((at, c));
        at += cell(c);
    }
    let Some(hit) = cells.iter().rposition(|(start, _)| *start <= col) else {
        return (col, col + 1);
    };
    if !word(cells[hit].1) {
        let (start, c) = cells[hit];
        return (start, start + cell(c).max(1));
    }
    let first = cells[..hit]
        .iter()
        .rposition(|(_, c)| !word(*c))
        .map_or(0, |i| i + 1);
    let last = cells[hit..]
        .iter()
        .position(|(_, c)| !word(*c))
        .map_or(cells.len(), |i| hit + i);
    let (end_at, end_c) = cells[last - 1];
    (cells[first].0, end_at + cell(end_c))
}

/// `line` with the cells `from..to` drawn reversed.
pub(super) fn reversed(line: Line<'static>, from: usize, to: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut at = 0;
    for span in line.spans {
        for c in span.content.chars() {
            let style = if at >= from && at < to {
                span.style.add_modifier(Modifier::REVERSED)
            } else {
                span.style
            };
            match spans.last_mut() {
                Some(s) if s.style == style => s.content.to_mut().push(c),
                _ => spans.push(Span::styled(c.to_string(), style)),
            }
            at += cell(c);
        }
    }
    Line::from(spans).style(line.style)
}

/// Every drawn row with what `sel` covers of it reversed, past each row's
/// lead as the copy reads it.
pub(super) fn highlight(
    rows: Vec<Line<'static>>,
    sel: &Selection,
    lead: impl Fn(usize) -> usize,
) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(at, line)| match sel.cols(at) {
            Some((from, to)) => reversed(line, from.max(lead(at)), to),
            None => line,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(text: &str, line: Option<(usize, usize)>, lead: usize) -> Drawn {
        Drawn {
            text: text.into(),
            line,
            lead,
        }
    }

    fn sel(a: (usize, usize), b: (usize, usize)) -> Selection {
        Selection {
            anchor: Point { row: a.0, col: a.1 },
            head: Point { row: b.0, col: b.1 },
        }
    }

    // What lands on the clipboard is the text, not the screen: no frame, no
    // break where only the width broke the line, a break where it ended.
    #[test]
    fn a_selection_copies_the_text_not_the_frame_or_the_wraps() {
        let width = 10;
        let rows = [
            drawn("│ abcdefgh", Some((0, 0)), 2),
            drawn("│ ij      ", Some((0, 0)), 2),
            drawn("│ next    ", Some((0, 1)), 2),
        ];
        assert_eq!(text(&sel((0, 0), (2, 9)), &rows, width), "abcdefghij\nnext");
        // Dragged backwards, from the middle of a word.
        assert_eq!(text(&sel((1, 3), (0, 4)), &rows, width), "cdefghij");
    }

    #[test]
    fn a_word_is_its_letters_or_its_run_of_cjk() {
        let s = "let foo_bar = 中文字;";
        assert_eq!(cols(s, word_at(s, 6).0, word_at(s, 6).1), "foo_bar");
        let (from, to) = word_at(s, 16);
        assert_eq!(cols(s, from, to), "中文字");
        assert_eq!(cols(s, word_at(s, 12).0, word_at(s, 12).1), "=");
    }

    #[test]
    fn reversing_counts_cells_not_chars() {
        let line = Line::from("中ab");
        let out = reversed(line, 2, 3);
        let marked: String = out
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, "a");
    }
}
