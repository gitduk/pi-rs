//! Shared by the test binaries that look at a view's output.
//!
//! A test binary takes the whole module and uses part of it, so what one
//! binary does not call is not dead — it is another binary's.
#![allow(dead_code)]

use tools::{Ctx, Workspace};

/// A workspace whose spills land inside it, so a test that overflows a view
/// never writes into the state directory of whoever is running it.
pub fn spilling() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let c = Ctx::new(ws).with_spill_root(dir.path().join("spill"));
    (dir, c)
}

/// The locator a spilled view left behind, or a failure that shows the view.
///
/// One spelling, because every spilling tool prints one: read, bash, grep and
/// glob all hand the model back the same line, and a test that hunted for it
/// its own way would pass while the line it was meant to guard had changed.
pub fn locator_in(out: &str) -> &str {
    out.lines()
        .find_map(|l| {
            l.strip_prefix("full output: ")
                .and_then(|l| l.split(' ').next())
        })
        .unwrap_or_else(|| panic!("what was dropped must be recoverable:\n{out}"))
}

/// What the spill behind `out` actually holds.
pub fn spilled_body(c: &Ctx, out: &str) -> String {
    std::fs::read_to_string(c.spill_path(locator_in(out)).unwrap()).unwrap()
}

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
