//! Text selected with the mouse in the transcript: dragged, or a word or a
//! line clicked twice or thrice. Read off the drawn rows, frames and borders
//! left out and wrapped rows joined back into the line they came from.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

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
    for at in from.row..=to.row {
        let Some(row) = rows.get(at) else {
            break;
        };
        let Some((start, end)) = sel.cols(at) else {
            continue;
        };
        let piece = cols(&row.text, start.max(row.lead), end);
        let next = rows.get(at + 1);
        // A row filled to the edge and followed by more of its line wrapped
        // there; a shorter one ended where the line did.
        let wrapped = row.line.is_some()
            && next.is_some_and(|n| n.line == row.line)
            && pi_store::text::width(row.text.trim_end()) + 1 >= width;
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

/// Whole graphemes intersecting the screen columns `from..to`.
pub(super) fn cols(s: &str, from: usize, to: usize) -> String {
    let mut at = 0;
    let mut out = String::new();
    for (_, g) in pi_store::text::graphemes(s) {
        let w = pi_store::text::grapheme_width(g, at);
        if at < to && at + w > from {
            out.push_str(g);
        }
        at += w;
    }
    out
}

/// A word under the pointer, with each non-word grapheme kept whole.
pub(super) fn word_at(s: &str, col: usize) -> (usize, usize) {
    let word = |g: &str| {
        g.chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    };
    let mut cells = Vec::new();
    let mut at = 0;
    for (_, g) in pi_store::text::graphemes(s) {
        let w = pi_store::text::grapheme_width(g, at);
        cells.push((at, at + w, word(g)));
        at += w;
    }
    let Some(hit) = cells
        .iter()
        .position(|&(start, end, _)| start <= col && col < end)
    else {
        return (col, col + 1);
    };
    if !cells[hit].2 {
        return (cells[hit].0, cells[hit].1);
    }
    let first = cells[..hit].iter().rposition(|c| !c.2).map_or(0, |i| i + 1);
    let last = cells[hit..]
        .iter()
        .position(|c| !c.2)
        .map_or(cells.len(), |i| hit + i);
    (cells[first].0, cells[last - 1].1)
}

/// `line` with the cells `from..to` drawn reversed.
pub(super) fn reversed(line: Line<'static>, from: usize, to: usize) -> Line<'static> {
    modified(
        line,
        from,
        to,
        Style::new().add_modifier(Modifier::REVERSED),
    )
}

/// `line` with `mark` laid over the cells `from..to`.
pub(super) fn modified(line: Line<'static>, from: usize, to: usize, mark: Style) -> Line<'static> {
    let line = super::screen::clip_line(&line, usize::MAX).style(line.style);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut at = 0;
    for span in line.spans {
        for (_, g) in pi_store::text::graphemes(&span.content) {
            let w = pi_store::text::grapheme_width(g, at);
            let style = if at + w > from && at < to {
                span.style.patch(mark)
            } else {
                span.style
            };
            match spans.last_mut() {
                Some(s) if s.style == style => s.content.to_mut().push_str(g),
                _ => spans.push(Span::styled(g.to_string(), style)),
            }
            at += w;
        }
    }
    Line::from(spans).style(line.style)
}

/// Every drawn row with what `sel` covers of it reversed, past each row's
/// lead as the copy reads it; the rows start at `from` of the selection's.
pub(super) fn highlight(
    rows: Vec<Line<'static>>,
    sel: &Selection,
    from: usize,
    lead: impl Fn(usize) -> usize,
) -> Vec<Line<'static>> {
    rows.into_iter()
        .enumerate()
        .map(|(at, line)| match sel.cols(from + at) {
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
