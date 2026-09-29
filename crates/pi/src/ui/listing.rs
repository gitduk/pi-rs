//! A listing as lines: the columns lined up.

use unicode_width::UnicodeWidthStr;

use crate::store::icons;
use crate::store::listing::Listing;
use crate::text;

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
