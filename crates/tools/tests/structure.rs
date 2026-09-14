use serde_json::json;
use tools::{Ctx, Tool, ToolError, Workspace};

fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

async fn run(tool: &dyn Tool, args: serde_json::Value, ctx: &Ctx) -> String {
    tool.execute(args, ctx).await.unwrap().flatten()
}

// A file long enough to trigger the skeleton, with two real declarations in it.
fn long_rust() -> String {
    let filler: String = (0..320).map(|i| format!("// filler {i}\n")).collect();
    format!(
        "{filler}pub struct Point {{\n    x: i32,\n}}\n\nimpl Point {{\n    pub fn new() -> Self {{\n        Self {{ x: 0 }}\n    }}\n}}\n"
    )
}

mod common;
use common::every_row_anchors_in_the_body;

#[tokio::test]
async fn a_long_file_comes_back_as_a_skeleton() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let out = run(&tools::read::Read, json!({ "path": "big.rs" }), &c).await;
    assert!(out.starts_with("[big.rs] 329 lines · outline"), "{out}");
    // The span, so the model can replace one whole without a second read, and
    // the indent, so a method does not read like a top-level item.
    assert!(out.contains("321-323:pub struct Point {"), "{out}");
    every_row_anchors_in_the_body(&out, "big.rs", &body, 3);
    assert!(out.contains("325-329:impl Point {"), "{out}");
    assert!(out.contains("326-328:  pub fn new() -> Self {"), "{out}");
    // 329 lines of source must not come back as 329 lines of output.
    assert!(out.lines().count() < 10, "{out}");
    assert!(!out.contains("// filler"), "{out}");
}

#[tokio::test]
async fn a_range_request_is_answered_with_lines_not_a_skeleton() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    // Asking for a range is an explicit ask for lines.
    let out = run(
        &tools::read::Read,
        json!({ "path": "big.rs", "offset": 321, "limit": 3 }),
        &c,
    )
    .await;
    // A declaration's row says where it ends; an ordinary row says only itself.
    // Both are anchors the model can quote verbatim: a form it cannot quote is
    // the tool teaching a grammar its own parser refuses.
    assert!(out.contains("321-323:pub struct Point {"), "{out}");
    assert!(out.contains("\n322:    x: i32,"), "{out}");
    assert!(!out.contains("outline"), "{out}");
    every_row_anchors_in_the_body(&out, "big.rs", &body, 3);
}

#[tokio::test]
async fn a_construct_that_closes_past_the_window_still_says_where() {
    // The whole reason the range view carries spans: `impl Point` opens at 325
    // and closes at 329, and a three-line window shows neither the closing
    // brace nor any way to count to it.
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let out = run(
        &tools::read::Read,
        json!({ "path": "big.rs", "offset": 325, "limit": 3 }),
        &c,
    )
    .await;
    assert!(out.contains("325-329:impl Point {"), "{out}");
    assert!(
        !out.contains("\n329:}"),
        "the window must still end at 327: {out}"
    );

    // And what it says anchors in the file. Ordinary rows too: those are the
    // ones a bare number used to be printed for.
    every_row_anchors_in_the_body(&out, "big.rs", &body, 3);
}

#[tokio::test]
async fn a_short_file_is_still_read_whole_and_outline_can_be_forced() {
    let (_d, c) = ctx();
    std::fs::write(
        c.workspace.root().join("a.rs"),
        "pub fn one() {}\npub fn two() {}\n",
    )
    .unwrap();

    let whole = run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    assert!(whole.contains("1:pub fn one() {}"), "{whole}");
    assert!(!whole.contains("outline"), "{whole}");

    let forced = run(
        &tools::read::Read,
        json!({ "path": "a.rs", "outline": true }),
        &c,
    )
    .await;
    assert!(forced.contains("· outline"), "{forced}");
    assert!(forced.contains("2:pub fn two() {}"), "{forced}");
}

#[tokio::test]
async fn a_long_file_in_an_unparsed_language_still_reads_as_lines() {
    let (_d, c) = ctx();
    let body: String = (0..320).map(|i| format!("line {i}\n")).collect();
    std::fs::write(c.workspace.root().join("notes.txt"), &body).unwrap();

    let out = run(&tools::read::Read, json!({ "path": "notes.txt" }), &c).await;
    assert!(!out.contains("outline"), "{out}");
    assert!(out.contains("1:line 0"), "{out}");
}

#[tokio::test]
async fn a_scope_row_replaces_a_whole_function_without_counting_lines() {
    let (_d, c) = ctx();
    let src = "pub fn keep() {}\n\npub fn replace_me(a: i32) -> i32 {\n    a * 2\n}\n\npub fn also_keep() {}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    let report = tools::edit::Edit
        .execute(
            json!({ "patch": "[a.rs]\n@pub fn replace_me(a: i32) -> i32\n-pub fn replace_me(a: i32) -> i32 {\n-    a * 2\n-}\n+pub fn replaced() {}\n" }),
            &c,
        )
        .await
        .unwrap()
        .flatten();

    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "pub fn keep() {}\n\npub fn replaced() {}\n\npub fn also_keep() {}\n"
    );
    assert!(report.contains("3:pub fn replaced() {}"), "{report}");
}

// Three sections for one file stack: every section resolves against what the
// earlier ones left, so all three land. Each built off the same original
// would leave only the last one on disk.
#[tokio::test]
async fn three_sections_for_one_file_all_land() {
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\n";
    std::fs::write(c.workspace.root().join("a.txt"), src).unwrap();

    run(&tools::read::Read, json!({ "path": "a.txt" }), &c).await;
    tools::edit::Edit
        .execute(
            json!({ "patch": "[a.txt]\n=one\n+ONE\n\n[a.txt]\n=two\n+TWO\n\n[a.txt]\n=three\n+THREE" }),
            &c,
        )
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.txt")).unwrap(),
        "one\nONE\ntwo\nTWO\nthree\nTHREE\n"
    );
}

#[tokio::test]
async fn a_scope_row_takes_the_attribute_above_when_named_by_it() {
    let (_d, c) = ctx();
    let src = "#[inline]\npub fn f() {\n    1\n}\n\npub fn g() {}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    tools::edit::Edit
        .execute(
            json!({ "patch": "[a.rs]\n@#[inline]\n-#[inline]\n-pub fn f() {\n-    1\n-}\n+pub fn f() { 2 }\n" }),
            &c,
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "pub fn f() { 2 }\n\npub fn g() {}\n"
    );
}

#[tokio::test]
async fn a_scope_row_on_a_closing_brace_refuses() {
    let (_d, c) = ctx();
    let src = "pub fn f() {\n    1\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    let err = tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n*}\n+x\n" }), &c)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Patch(_, _)), "{err:?}");
    assert!(err.to_string().contains("no construct opens"), "{err}");
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        src
    );
}

#[tokio::test]
async fn a_scope_row_in_an_unparsed_language_says_so() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.txt"), "one\ntwo\n").unwrap();
    run(&tools::read::Read, json!({ "path": "a.txt" }), &c).await;
    let err = tools::edit::Edit
        .execute(json!({ "patch": "[a.txt]\n*one\n+x\n" }), &c)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no construct opens"), "{err}");
}

#[tokio::test]
async fn the_outline_names_the_construct_and_feeds_an_edit_without_a_read() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let outline = run(&tools::read::Read, json!({ "path": "big.rs" }), &c).await;
    assert!(outline.contains("326-328:  pub fn new() -> Self {"));
    // The skeleton shows row 326 as `326-328:  pub fn new() -> Self {`: the
    // text is the scope, the span is the extent — nothing else was read.
    tools::edit::Edit
        .execute(
            json!({ "patch": "[big.rs]\n@  pub fn new() -> Self {\n-  pub fn new() -> Self {\n-        Self { x: 0 }\n-    }\n+    pub fn new() -> Self { Self { x: 1 } }\n" }),
            &c,
        )
        .await
        .unwrap();

    let after = std::fs::read_to_string(c.workspace.root().join("big.rs")).unwrap();
    assert!(
        after.contains("Self { x: 1 }"),
        "{}",
        &after[after.len() - 200..]
    );
    assert!(!after.contains("Self { x: 0 }"));
}
