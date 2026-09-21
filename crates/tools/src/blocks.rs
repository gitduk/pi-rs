//! Resolves `whole_block` anchors through tree-sitter: the outline is what
//! says where a block starts and where it ends.

/// The inclusive 1-based rows of the block at `line`, if there is one. Both
/// ends: an annotation above the line belongs to what it annotates, so the
/// start may sit above `line`.
pub(crate) fn extent_of(path: &str, content: &str, line: usize) -> Option<(usize, usize)> {
    crate::syntax::block(crate::syntax::Lang::of(path)?, content, line)
}

/// Every row a block opens on, in order. A `whole_block` anchor names one of
/// these by prefix, matched against the line itself, so no per-language name
/// grammar is needed.
pub(crate) fn openings(path: &str, content: &str) -> Vec<usize> {
    extents(path, content)
        .into_iter()
        .map(|(opens, _)| opens)
        .collect()
}

/// Every opening row with the block it opens, for a refusal naming several
/// candidates at once: one walk of the file, where resolving each opening on
/// its own would parse once per candidate.
pub(crate) fn extents(path: &str, content: &str) -> Vec<(usize, usize)> {
    let Some(lang) = crate::syntax::Lang::of(path) else {
        return Vec::new();
    };
    let mut rows: Vec<(usize, usize)> = crate::syntax::extents(lang, content)
        .into_values()
        .collect();
    rows.sort_unstable();
    rows
}
