//! The check that stops a tool writing a file it just broke.
//!
//! Two tools can leave a file unparseable — a patch whose range covers one line
//! too few, a whole-file write whose content ran short — and one reading of
//! "broke it" answers both. Two copies of it is how the second tool drifts into
//! refusing what the first allows, over a difference nobody chose.

/// Every row a change breaks, ascending, and empty when it breaks nothing.
///
/// Empty too when no parser knows the path, and when what was there already
/// failed to parse: a file the change found broken is not the change's doing,
/// and refusing there would leave it with no way back.
pub(crate) fn broke_rows(path: &str, before: Option<&str>, after: &str) -> Vec<usize> {
    let Some(lang) = syntax::Lang::of(path) else {
        return Vec::new();
    };
    // The result first: content that parses needs nothing said about what came
    // before it, which is every write on the ordinary path.
    let rows = syntax::error_rows(lang, after);
    if rows.is_empty() || before.is_some_and(|b| !syntax::error_rows(lang, b).is_empty()) {
        return Vec::new();
    }
    rows
}

/// The row a change breaks, and that row's text.
pub(crate) fn broke(path: &str, before: Option<&str>, after: &str) -> Option<(usize, String)> {
    let row = *broke_rows(path, before, after).first()?;
    Some((row, row_text(after, row)))
}

/// The row's own text, because a bare line number invites a story about why the
/// parser is wrong instead of a look at the line.
pub(crate) fn row_text(content: &str, row: usize) -> String {
    content
        .lines()
        .nth(row - 1)
        .unwrap_or("")
        .trim()
        .to_string()
}
