//! A listing as lines: the columns lined up.

use unicode_width::UnicodeWidthStr;

use pi_store::icons;
use pi_store::listing::Listing;
use pi_store::text;

/// Rows as text, each multi-cell row's first cell padded to the widest such
/// cell — done here, since only drawing knows the width to fit.
pub fn lines(listing: &Listing) -> Vec<String> {
    split(listing)
        .into_iter()
        .map(|(head, rest)| head + &rest)
        .collect()
}

/// Rows as `lines` lays them, cut after the padded first cell: what wraps is
/// the rest, under its own column. A prose row's head is empty.
pub fn split(listing: &Listing) -> Vec<(String, String)> {
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
            let (head, cells) = match row.cells.split_first() {
                Some((first, rest)) if !rest.is_empty() => {
                    (format!("{}  ", text::pad(first, width)), rest)
                }
                _ => (String::new(), &row.cells[..]),
            };
            let mut rest = cells.join("  ");
            if let Some(note) = &row.note {
                rest.push_str(icons::KEY_NOTE_SEP);
                rest.push_str(note);
            }
            (head, rest)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pi_store::keys::{Keys, chord};

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
                assert!(chord(spec).is_ok(), "cannot re-read `{spec}`");
            }
        }
    }
}
