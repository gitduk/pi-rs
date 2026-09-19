//! Shared by the test binaries that look at a view's output.
//!
//! A test binary takes the whole module and uses part of it, so what one
//! binary does not call is not dead — it is another binary's.
#![allow(dead_code)]

use tools::read::Read;
use tools::{Ctx, Tool, Workspace};

/// A file as a model would read it, which is also what lets a later edit run.
/// A caller that wants only that second half drops the result.
pub async fn view(c: &Ctx, path: &str) -> String {
    run(&Read, serde_json::json!({ "path": path }), c).await
}

/// A workspace of its own, for a test that asserts on a tool's own behaviour.
pub fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

/// One call, unwrapped, for a test that expects it to land.
pub async fn run(tool: &dyn Tool, args: serde_json::Value, ctx: &Ctx) -> String {
    tool.execute(args, ctx).await.unwrap().flatten()
}

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
