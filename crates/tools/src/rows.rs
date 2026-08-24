//! How a view spells the address of a line it prints.
//!
//! Three views print lines — a skeleton, a range read, an edit's echo — and
//! `hashline` parses what the model copies back out of them. One spelling here
//! rather than three `format!`s is the difference between that staying true and
//! staying true by luck.

use std::collections::HashMap;

/// Where each construct that opens on a row ends, keyed by the row it opens on.
///
/// Only the constructs spanning more than one row: a single-line item's range
/// is the number already in front of it, and `5.=5` on every `const` is noise.
/// An unknown language has no constructs and every row prints bare.
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
pub(crate) fn addr(n: usize, spans: &HashMap<usize, usize>) -> String {
    match spans.get(&n) {
        Some(end) => format!("{n}.={end}:"),
        None => format!("{n}:"),
    }
}
