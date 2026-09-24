//! A listing as lines: the columns lined up.

use unicode_width::UnicodeWidthStr;

use crate::store::listing::Listing;
use crate::store::{icons, text};

/// The rows as text: the first cell of every row padded to the widest first
/// cell among the rows that have more than one, so the columns line up, and the
/// note after the cells.
///
/// Padded here rather than where the answer was built, because the width a row
/// has to fit in exists only where the drawing happens — and there are two
/// drawings. A row of one cell is prose: it is what it says, and padding it
/// would indent it under a column that is not its.
pub fn lines(listing: &Listing) -> Vec<String> {
    let width = listing
        .rows
        .iter()
        .filter(|r| r.cells.len() > 1)
        .map(|r| UnicodeWidthStr::width(r.cells[0].as_str()))
        .max()
        .unwrap_or(0);
    listing
        .rows
        .iter()
        .map(|row| {
            let mut out = String::new();
            let table = row.cells.len() > 1;
            for (at, cell) in row.cells.iter().enumerate() {
                if at > 0 {
                    out.push_str("  ");
                }
                if at == 0 && table {
                    out.push_str(&text::pad(cell, width));
                } else {
                    out.push_str(cell);
                }
            }
            if let Some(note) = &row.note {
                out.push_str(icons::KEY_NOTE_SEP);
                out.push_str(note);
            }
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::listing::Row;

    // The first cell is the column: it is padded to the widest of them, so the
    // rows read down as well as across. A sentence between the rows is not part
    // of that column and does not stretch it.
    #[test]
    fn the_first_cell_lines_the_rows_up_and_prose_is_left_alone() {
        let listing = Listing::of([
            Row::new(["tier", "= exec", "✓"]),
            Row::new(["summarize_model", "= haiku"]),
            Row::new(["a sentence that is not a column at all"]),
            Row::new(["path.to.somewhere", "= 3"]),
        ]);
        assert_eq!(
            lines(&listing),
            vec![
                "tier               = exec  ✓",
                "summarize_model    = haiku",
                "a sentence that is not a column at all",
                "path.to.somewhere  = 3",
            ]
        );
    }

    // The note is the aside, not another column: it is said with the separator
    // every other note is, wherever the cells before it ended.
    #[test]
    fn a_note_follows_the_cells_in_the_note_voice() {
        let listing = Listing::of([
            Row::new(["normal.history.older", "ctrl+up, ctrl+k"]).noting("the older line"),
            Row::new(["edit.newline", "alt+enter"]).noting("a new line in the buffer"),
        ]);
        assert_eq!(
            lines(&listing),
            vec![
                "normal.history.older  ctrl+up, ctrl+k  ·  the older line",
                "edit.newline          alt+enter  ·  a new line in the buffer",
            ]
        );
    }

    // Width is columns, not bytes: a label in a double-width script lines up
    // with a Latin one the same way it does on the screen.
    #[test]
    fn a_wide_label_is_padded_by_what_it_covers() {
        let listing = Listing::of([Row::new(["模型", "= haiku"]), Row::new(["tier", "= exec"])]);
        assert_eq!(lines(&listing), vec!["模型  = haiku", "tier  = exec"]);
    }
}
