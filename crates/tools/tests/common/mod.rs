//! Shared by the test binaries that look at a view's output.

/// Every address a view printed, handed back to the parser that must read it.
///
/// Three guards in one, because each has already failed here. The addresses are
/// found by shape rather than by spelling — the first version looked for `.=`,
/// and when the grammar moved to `-` it matched nothing and passed silently.
/// The count is asserted, because a view whose rows all happen to be one kind
/// proves nothing about the other: that version passed while every non-spanning
/// row printed a bare number the parser rejects. And it is shared, because the
/// view it was not applied to is the one that stayed broken.
pub fn every_address_parses(out: &str, path: &str, body: &str, least: usize) {
    let mut checked = 0;
    for row in out.lines() {
        let Some((addr, _)) = row.split_once(':') else {
            continue;
        };
        if !addr.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let patch = format!("[{path}#{}]\nCUT {addr}\n", hashline::tag(body));
        assert!(
            hashline::parse(&patch).is_ok(),
            "`{addr}` is printed but not parsed:\n{out}"
        );
        checked += 1;
    }
    assert!(checked >= least, "checked only {checked} of:\n{out}");
}
