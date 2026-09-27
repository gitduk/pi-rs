//! How a view spells the address of a line it prints.
//!
//! Five views print lines — a skeleton, a range read, a grep hit, an edit's
//! echo, and the refusal that hands back numbering an edit moved — and the
//! edit tool reads the addresses back out of them. One spelling here rather
//! than five `format!`s is the difference between that staying true and
//! staying true by luck.

use std::collections::HashMap;

/// Name for a file in a report or a refusal, as the views print it.
pub(crate) fn header(path: &str) -> String {
    format!("[{path}]")
}

/// Content hash for the staleness note: a file that changed underneath the
/// model since its last view gets a note beside the report. Not a gate any
/// more — the anchors are the gate.
pub(crate) fn view_hash(content: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in content.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:04X}", (h ^ (h >> 16)) & 0xFFFF)
}

/// Where each construct that opens on a row ends, keyed by the row it opens on.
///
/// Only the constructs spanning more than one row. A single-line item needs no
/// entry: `addr` renders `N-N` for any row it does not find here, so a row
/// carries a span exactly when the span says something the row does not.
///
/// Every construct, not only the declarations an outline lists: a range ending
/// one line off a `match` or a struct literal is what breaks a file.
pub(crate) fn spans(path: &str, content: &str) -> HashMap<usize, usize> {
    let Some(lang) = crate::syntax::Lang::of(path) else {
        return HashMap::new();
    };
    crate::syntax::spans(lang, content)
}

/// The same, for a skeleton — which lists declarations and shows spans for
/// those alone.
pub(crate) fn of(items: &[crate::syntax::Item]) -> HashMap<usize, usize> {
    items
        .iter()
        .filter(|item| item.end > item.line)
        .map(|item| (item.line, item.end))
        .collect()
}

/// A row's address and its colon, ready for the text of the row to follow.
///
/// A row that spans itself prints as a range: the address says where the
/// construct ends, which is what a `*` scope row needs to name.
pub(crate) fn addr(n: usize, spans: &HashMap<usize, usize>) -> String {
    let end = spans.get(&n).copied().unwrap_or(n);
    if end == n {
        format!("{n}:")
    } else {
        format!("{n}-{end}:")
    }
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
