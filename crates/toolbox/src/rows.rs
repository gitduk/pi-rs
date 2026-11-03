//! How a view spells the address of a line it prints.
//!
//! Views print lines — a skeleton, a grep hit, an edit's
//! echo, and the refusal that hands back numbering an edit moved — and the
//! edit tool reads the addresses back out of them. One spelling here rather
//! than separate `format!`s is the difference between that staying true and
//! staying true by luck.

use std::collections::HashMap;

/// Name for a file in a report or a refusal, as the views print it.
pub(crate) fn header(path: &str) -> String {
    format!("[{path}]")
}

/// Content hash for the log: names the version a view was built on, for
/// when an edit goes wrong.
pub(crate) fn view_hash(content: &str) -> String {
    format!("{:016X}", tool::stamp::fnv(content.as_bytes()))
}

/// Where each outline declaration ends, keyed by its opening row.
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
