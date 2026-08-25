//! How a view spells the address of a line it prints.
//!
//! Three views print lines — a skeleton, a range read, an edit's echo — and
//! `hashline` parses what the model copies back out of them. One spelling here
//! rather than three `format!`s is the difference between that staying true and
//! staying true by luck.

use std::collections::HashMap;

/// Where each construct that opens on a row ends, keyed by the row it opens on.
///
/// Only the constructs spanning more than one row. A single-line item needs no
/// entry: `addr` renders `N-N` for any row it does not find here, so a row
/// carries a span exactly when the span says something the row does not.
pub(crate) fn spans(path: &str, content: &str) -> HashMap<usize, usize> {
    let Some(lang) = syntax::Lang::of(path) else {
        return HashMap::new();
    };
    of(&syntax::outline(lang, content))
}

/// The same, for a caller that already has the skeleton.
pub(crate) fn of(items: &[syntax::Item]) -> HashMap<usize, usize> {
    items
        .iter()
        .filter(|item| item.end > item.line)
        .map(|item| (item.line, item.end))
        .collect()
}

/// A row's address and its colon, ready for the text of the row to follow.
///
/// Rendered by `hashline`, which is the crate that parses it back: two
/// `format!`s pointing opposite ways is how a view starts printing an address
/// its own parser rejects. A row that spans itself prints as the bare number,
/// since that is the address `PUT N:` takes.
pub(crate) fn addr(n: usize, spans: &HashMap<usize, usize>) -> String {
    let end = spans.get(&n).copied().unwrap_or(n);
    format!("{}:", hashline::Target::Range { start: n, end })
}
