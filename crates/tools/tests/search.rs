mod common;

use common::run;
use serde_json::json;
use tools::{Ctx, Workspace};

fn tree() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    std::fs::create_dir_all(r.join("src/deep")).unwrap();
    std::fs::write(r.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        r.join("src/deep/util.rs"),
        "pub fn helper() {}\n// TODO: rename\n",
    )
    .unwrap();
    std::fs::write(r.join("README.md"), "# Title\nTODO: write docs\n").unwrap();
    let ws = Workspace::new(r).unwrap();
    (dir, Ctx::new(ws))
}

// The parallel walker finishes files in whatever order it likes; two
// identical searches must still answer in the same order.
#[tokio::test]
async fn grep_order_does_not_change_between_identical_searches() {
    let (_d, c) = tree();
    let a = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    let b = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    assert_eq!(a, b);
}

// Overlapping roots must not report one file twice: the dedup is what keeps a
// count honest when the targets nest.
#[tokio::test]
async fn overlapping_roots_report_one_file_once() {
    let (_d, c) = tree();
    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "path": ["src", "src/deep"] }),
        &c,
    )
    .await;
    assert_eq!(out.matches("[src/deep/util.rs]").count(), 1, "{out}");
}

// A file whose bytes are not UTF-8 is searched as bytes: a UTF-8 sink would
// drop it, and its matches, silently.
#[tokio::test]
async fn a_file_with_invalid_utf8_still_reports_its_matches() {
    let (_d, c) = tree();
    let mut bytes = b"fn a() {}\n// TODO: fix \xF0\x28 here\n".to_vec();
    bytes.extend_from_slice(b"fn b() {}\n");
    std::fs::write(c.workspace.root().join("src/odd.rs"), &bytes).unwrap();

    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "glob": ["odd.rs"] }),
        &c,
    )
    .await;
    assert!(out.contains("src/odd.rs"), "{out}");
    assert!(out.contains("2:"), "{out}");
}

// Over the window the cut lands between whole sections — never inside a row —
// the count answers for what was dropped, and the spill holds the rest. A
// single section bigger than the whole window is not split: it goes whole,
// leaving only the locator.
#[tokio::test]
async fn an_over_long_grep_drops_whole_sections_and_spills_the_rest() {
    let (_d, c) = common::spilling();
    let r = c.workspace.root();
    // Each file is a section of its own, and no one section is over budget: the
    // cut has to land between them.
    for i in 0..40 {
        std::fs::write(
            r.join(format!("f{i:02}.txt")),
            format!("NEEDLE {}\n", "z".repeat(2_000)),
        )
        .unwrap();
    }

    let out = run(&tools::grep::Grep, json!({ "pattern": "NEEDLE" }), &c).await;
    assert!(out.len() <= tools::spill::MAX_OUTPUT, "{} bytes", out.len());
    for line in out.lines().filter(|l| l.starts_with(char::is_numeric)) {
        let (_, text) = line.split_once(':').expect("a row is addressed");
        assert_eq!(text.len(), "NEEDLE ".len() + 2_000, "half a row: {line}");
    }
    // The count has to answer for the byte cut too: a body saying only what the
    // line limit dropped reads as near-complete when most sections went.
    assert!(out.contains("of 40 files did not fit the window"), "{out}");
    let whole = common::spilled_body(&c, &out);
    assert!(whole.contains("f39.txt"), "the spill must hold what went");
    assert!(
        !out.contains("f39.txt"),
        "nothing was actually dropped\n{out}"
    );

    // One file whose one section is bigger than the window: nothing of it is
    // shown, and the locator is where it all went.
    let (_d2, c2) = common::spilling();
    std::fs::write(
        c2.workspace.root().join("min.js"),
        format!("NEEDLE {}\n", "z".repeat(tools::spill::MAX_OUTPUT)),
    )
    .unwrap();
    let out = run(&tools::grep::Grep, json!({ "pattern": "NEEDLE" }), &c2).await;
    assert!(!out.contains("NEEDLE"), "a section was split\n{out}");
    let whole = common::spilled_body(&c2, &out);
    assert!(whole.contains("NEEDLE"), "the spill must hold what went");
}
