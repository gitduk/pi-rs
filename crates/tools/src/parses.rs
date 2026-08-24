//! The check that stops a tool writing a file it just broke.
//!
//! Two tools can leave a file unparseable — a patch whose range covers one line
//! too few, a whole-file write whose content ran short — and one reading of
//! "broke it" answers both. Two copies of it is how the second tool drifts into
//! refusing what the first allows, over a difference nobody chose.

/// The row a change breaks, and that row's text.
///
/// `None` when the content parses, when no parser knows the path, or when what
/// was there already failed to parse: a file the change found broken is not the
/// change's doing, and refusing there would leave it with no way back.
pub(crate) fn broke(path: &str, before: Option<&str>, after: &str) -> Option<(usize, String)> {
    let lang = syntax::Lang::of(path)?;
    // The result first: content that parses needs nothing said about what came
    // before it, which is every write on the ordinary path.
    let row = syntax::first_error(lang, after)?;
    if before.is_some_and(|b| syntax::first_error(lang, b).is_some()) {
        return None;
    }
    // The row's own text, because a bare line number invites a story about why
    // the parser is wrong instead of a look at the line.
    let text = after.lines().nth(row - 1).unwrap_or("").trim().to_string();
    Some((row, text))
}
