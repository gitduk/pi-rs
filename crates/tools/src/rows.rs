//! How a view spells the address of a line it prints.
//!
//! Five views print lines — a skeleton, a range read, a grep hit, an edit's
//! echo, and the refusal that hands back numbering an edit moved — and
//! `hashline` parses what the model copies back out of them. One spelling here
//! rather than five `format!`s is the difference between that staying true and
//! staying true by luck.

use std::collections::HashMap;

/// Where each construct that opens on a row ends, keyed by the row it opens on.
///
/// Only the constructs spanning more than one row. A single-line item needs no
/// entry: `addr` renders `N-N` for any row it does not find here, so a row
/// carries a span exactly when the span says something the row does not.
///
/// Every construct, not only the declarations an outline lists: a range ending
/// one line off a `match` or a struct literal is what breaks a file.
pub(crate) fn spans(path: &str, content: &str) -> HashMap<usize, usize> {
    let Some(lang) = syntax::Lang::of(path) else {
        return HashMap::new();
    };
    syntax::spans(lang, content)
}

/// The same, for a skeleton — which lists declarations and shows spans for
/// those alone.
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

/// What stands where a view's dropped rows were. Here rather than at each
/// view, so the mark elision takes across the tree is one thing and not two.
pub(crate) const GAP: &str = "…\n";

/// One printed row: its address, its text, its newline. Appended rather than
/// returned, since every caller is building a view line by line.
pub(crate) fn line(out: &mut String, n: usize, spans: &HashMap<usize, usize>, text: &str) {
    out.push_str(&addr(n, spans));
    out.push_str(text);
    out.push('\n');
}

/// How many of `items` fit in `room`, taken in the order given.
///
/// The one thing every view that spends a transcript budget has in common:
/// where the cut lands and what marks it differ per view, but "stop when the
/// bytes run out" does not.
pub(crate) fn fits<T>(
    items: impl Iterator<Item = T>,
    size: impl Fn(&T) -> usize,
    mut room: usize,
) -> usize {
    items
        .map_while(|item| {
            room = room.checked_sub(size(&item))?;
            Some(())
        })
        .count()
}
