//! What this run has masked, as `/mask` shows it: the name that introduced
//! each value, its two ends, and how often it left.

use llm::mask::Footprint;
use pi_store::listing::{Listing, Role, Row};
use pi_store::text::{clip, pad, widest};

/// Widest a name column gets before it is cut: room for a long variable name,
/// short enough that the value still fits on the line beside it.
const NAME_MAX: usize = 22;

/// What this run has masked, as the surface shows it.
pub fn listing() -> Listing {
    listing_of(super::dial::footprints())
}

fn listing_of(prints: Option<Vec<Footprint>>) -> Listing {
    match prints {
        // No key, so nothing is masked at all: saying "nothing masked" would
        // read as "the run had nothing to hide".
        None => Listing::say(["masking is off — there is no key to name values by"]),
        Some(prints) if prints.is_empty() => {
            Listing::say(["nothing masked yet — no credential has reached a request"])
        }
        Some(prints) => rows(prints),
    }
}

fn rows(mut prints: Vec<Footprint>) -> Listing {
    let total: u64 = prints.iter().map(|print| print.hits).sum();
    let values = prints.len();
    // What left most stands first: that is what a reader came to see.
    prints.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| label(a).cmp(label(b))));
    // The report lines up every column but the last; the ends are padded so
    // they read left to right under their heading instead of flush right.
    let shown: Vec<String> = prints.iter().map(ends).collect();
    let shown_w = widest(shown.iter().map(String::as_str)).max("VALUE".len());
    let mut rows = vec![
        Row::new(["Mask"]).with_role(Role::Title),
        Row::new([
            "values".into(),
            values.to_string(),
            "masked".into(),
            format!("{total}× total"),
        ])
        .with_role(Role::Metric),
        Row::new([
            "NAME".into(),
            "CHARS".into(),
            "MASKED".into(),
            pad("VALUE", shown_w),
        ])
        .with_role(Role::Section),
    ];
    rows.extend(prints.iter().zip(shown).map(|(print, ends)| {
        Row::new([
            clip(label(print), NAME_MAX),
            print.len.to_string(),
            print.hits.to_string(),
            pad(&ends, shown_w),
        ])
        .with_role(Role::Budget)
    }));
    Listing::report(rows)
}

// What a listing calls a value: the name its context gave it, or a dash when
// only its shape identified it.
fn label(print: &Footprint) -> &str {
    print.name.as_deref().unwrap_or("—")
}

// The two ends of a value, marked as cut when there is more of it than shown.
fn ends(print: &Footprint) -> String {
    let shown = print.head.chars().count() + print.tail.chars().count();
    // Both ends only while they stay a minority of the value: two thirds of a
    // short secret is enough to guess the rest of it.
    if shown * 2 <= print.len {
        format!("{}…{}", print.head, print.tail)
    } else {
        print.head.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::mask::Mask;

    // The listing is the only place a person sees what was masked: the name
    // that introduced each value, the two ends of it, marked only when there
    // is more of it than is shown, and how often it left.
    #[test]
    fn the_listing_shows_name_ends_and_how_often_it_left() {
        let mut mask = Mask::new([7; 32]);
        mask.add(Some("OLD_PASSWORD"), "hunter2-the-password")
            .expect("password token");
        mask.add(Some("provider.up.api_key"), "short-key")
            .expect("API key token");
        // Twelve characters: its two ends would be two thirds of it.
        mask.add(None, "abcdefghijkl").expect("unnamed token");
        let _ = mask.cover("hunter2-the-password");

        let prints = mask.footprints();
        let listing = rows(prints.clone());
        // The ends identify a value here; its name need not, since a value
        // found by shape alone is named by nothing.
        let row_of = |print: &Footprint| {
            listing
                .rows
                .iter()
                .find(|row| row.role == Role::Budget && row.cells[3].trim_end() == ends(print))
                .expect("a row per value")
        };
        let table: Vec<&Row> = listing
            .rows
            .iter()
            .filter(|row| row.role == Role::Budget)
            .collect();
        assert_eq!(table.len(), prints.len());
        // The column set is the listing's whole shape: a row of another width
        // would put the ends under the wrong heading.
        let header = listing
            .rows
            .iter()
            .find(|row| row.role == Role::Section)
            .expect("a header");
        assert_eq!(header.cells.len(), table[0].cells.len(), "{header:?}");
        for print in &prints {
            let row = row_of(print);
            assert_eq!(
                row.cells[0].trim_end(),
                clip(label(print), NAME_MAX),
                "{row:?}"
            );
            assert!(row.cells[0].trim_end().len() <= NAME_MAX, "{row:?}");
            assert!(!row.cells[0].contains("{{"), "{row:?}");
            assert!(row.cells[3].trim_end().starts_with(&print.head), "{row:?}");
            assert_eq!(row.cells[1].trim(), print.len.to_string(), "{row:?}");
            assert_eq!(row.cells[2].trim(), print.hits.to_string(), "{row:?}");
        }
        // A value found by shape alone is named by nothing, and says so.
        assert_eq!(row_of(&prints[2]).cells[0].trim_end(), "—");
        let long = prints
            .iter()
            .find(|print| print.len > print.head.chars().count() + print.tail.chars().count())
            .expect("a value longer than its ends");
        let row = row_of(long);
        assert!(row.cells[3].contains('\u{2026}'), "{row:?}");
        // Both ends while the hidden middle is the longer half of the value.
        assert!(row.cells[3].trim_end().ends_with(&long.tail), "{row:?}");
        // A value those two ends would give away shows its head alone.
        let short = prints
            .iter()
            .find(|print| print.len < 2 * (print.head.chars().count() + print.tail.chars().count()))
            .expect("a value shorter than twice its ends");
        let row = row_of(short);
        assert_eq!(row.cells[3].trim_end(), short.head, "{row:?}");

        let summary = listing
            .rows
            .iter()
            .find(|row| row.role == Role::Metric)
            .expect("a summary");
        assert_eq!(summary.cells[1], prints.len().to_string(), "{summary:?}");
        let total: u64 = prints.iter().map(|print| print.hits).sum();
        assert!(
            summary.cells[3].starts_with(&total.to_string()),
            "{summary:?}"
        );
    }

    // A run with no key masks nothing at all, which is not the same answer as
    // a run that has masked nothing yet, and neither is an empty list.
    #[test]
    fn a_run_with_nothing_armed_and_a_run_with_nothing_masked_read_differently() {
        let said = |listing: &Listing| {
            listing
                .rows
                .iter()
                .flat_map(|row| row.cells.iter())
                .cloned()
                .collect::<String>()
        };
        let off = listing_of(None);
        let empty = listing_of(Some(Vec::new()));
        assert_eq!(off.rows.len(), 1);
        assert_eq!(empty.rows.len(), 1);
        assert_ne!(said(&off), said(&empty));
    }
}
