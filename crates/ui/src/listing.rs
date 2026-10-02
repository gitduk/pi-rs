//! A listing as lines: the columns lined up.

use unicode_width::UnicodeWidthStr;

use pi_core::store::icons;
use pi_core::store::listing::Listing;
use pi_core::text;

/// Rows as text, each multi-cell row's first cell padded to the widest such
/// cell — done here, since only drawing knows the width to fit.
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
    use pi_core::store::keys::{Keys, parse};

    use super::*;

    #[test]
    fn the_listing_writes_keys_the_way_a_config_would() {
        let rows = lines(&Keys::default().listing()).join("\n");
        assert!(rows.contains("edit.delete.word-back"), "{rows}");
        // Round-trips: the keys shown can be pasted back into [keys].
        for row in lines(&Keys::default().listing()) {
            let keys = row.split(icons::KEY_NOTE_SEP).next().unwrap();
            let keys = keys.split_once("  ").expect("id then keys").1;
            for spec in keys.trim().split(", ") {
                assert!(parse(spec).is_ok(), "cannot re-read `{spec}`");
            }
        }
    }
}
