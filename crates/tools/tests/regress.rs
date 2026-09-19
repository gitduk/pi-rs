mod common;

use common::ctx;
use serde_json::json;
use tools::Tool;

// A row elided from the transcript is only recoverable through the spill file,
// so "something was elided" and "the whole of it was kept on disk" have to be
// one decision. They were two — a transcript budget and a spill threshold, each
// read off a differently assembled string — and between the two thresholds sat
// three bytes where rows vanished behind `…` with no locator to fetch them.
//
// Three bytes is why the sweep is wide and walks one byte at a time: aiming at
// the window means re-deriving the arithmetic that got it wrong, and any change
// to the header, the gap mark or the row format moves it.
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
        let out = tools::read::Read
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
