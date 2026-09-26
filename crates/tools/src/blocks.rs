//! Resolves `whole_block` anchors through tree-sitter: the outline is what
//! says where a block starts and where it ends.

use std::collections::BTreeMap;

/// The rows that name a block, mapped to that block's inclusive 1-based rows.
///
/// A construct's own row names it, and so does every row of an annotation above
/// it: naming either is asking about the same block, the one the annotation
/// belongs to. Where two blocks claim one row — `#[inline]` is a construct of
/// its own and also part of the function under it — the wider one wins, which
/// is the answer an anchor on that row is after either way.
///
/// Ordered by row, which is the order a refusal offers what the file opens in:
/// a reader wanting them in order takes the keys as they come.
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
