mod common;

use common::ctx;
use serde_json::json;
use tool::Tool;

// "Something was elided" and "the whole of it was kept on disk" must be one
// decision, or a row can vanish behind `…` with no locator to recover it.
#[tokio::test]
async fn nothing_is_elided_without_somewhere_to_recover_it_from() {
    let (_d, c) = ctx();
    let spill = tempfile::tempdir().unwrap();
    let c = c.with_spill_root(spill.path());
    let path = c.workspace.root().join("a.txt");

    // Rows of a fixed width, then one whose width walks the whole boundary.
    let bulk: String = (1..=880)
        .map(|i| format!("{i:04} xxxxxxxxxxxxxxxxxxxxxxxx\n"))
        .collect();
    let (mut elided, mut whole) = (false, false);
    for pad in 0..900 {
        std::fs::write(&path, format!("{bulk}{}\n", "y".repeat(pad))).unwrap();
        let out = toolbox::read::Read
            .execute(json!({ "path": "a.txt", "limit": 2000 }), &c)
            .await
            .unwrap()
            .flatten();
        if out.contains("\n…\n") {
            elided = true;
            assert!(
                out.contains("full output: spill:"),
                "pad {pad}: rows elided with no locator to recover them"
            );
        } else {
            whole = true;
        }
    }
    assert!(
        elided && whole,
        "the sweep never crossed the budget; widen it"
    );
}
