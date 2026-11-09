//! Resolves `whole_block` anchors through tree-sitter: the outline is what
//! says where a block starts and where it ends.

use std::collections::BTreeMap;

/// The rows that name a block, mapped to its inclusive 1-based span.
///
/// A row names the block it's in, including an annotation's row above it;
/// where two blocks claim a row, the wider one wins.
pub(crate) fn by_row(path: &str, content: &str) -> BTreeMap<usize, (usize, usize)> {
    let Some(lang) = crate::syntax::Lang::of(path) else {
        return BTreeMap::new();
    };
    let mut out: BTreeMap<usize, (usize, usize)> = BTreeMap::new();
    for (row, (start, end)) in crate::syntax::extents(lang, content) {
        for named in start..=row {
            let wider = out.get(&named).is_none_or(|(s, e)| end - start > e - s);
            if wider {
                out.insert(named, (start, end));
            }
        }
    }
    out
}

/// Every block once, ascending, for a caller that wants the spans rather than
/// the rows that name them.
pub(crate) fn spans(path: &str, content: &str) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = by_row(path, content).into_values().collect();
    spans.sort_unstable();
    spans.dedup();
    spans
}
