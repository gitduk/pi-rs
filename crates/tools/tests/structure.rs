use serde_json::json;
use tools::{Tool, ToolError};

// Failure messages quote a slice of the output; byte slicing would panic
// mid-UTF-8 and hide the real failure, so take characters instead.
fn tail(s: &str, n: usize) -> String {
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

// A file long enough to trigger the skeleton, with two real declarations in it.
fn long_rust() -> String {
    let filler: String = (0..320).map(|i| format!("// filler {i}\n")).collect();
    format!(
        "{filler}pub struct Point {{\n    x: i32,\n}}\n\nimpl Point {{\n    pub fn new() -> Self {{\n        Self {{ x: 0 }}\n    }}\n}}\n"
    )
}

mod common;
use common::{ctx, every_row_anchors_in_the_body, run, view};

#[tokio::test]
async fn a_long_file_comes_back_as_a_skeleton() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let out = view(&c, "big.rs").await;
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

    let whole = view(&c, "a.rs").await;
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

    let out = view(&c, "notes.txt").await;
    assert!(!out.contains("outline"), "{out}");
    assert!(out.contains("1:line 0"), "{out}");
}

#[tokio::test]
async fn a_whole_block_replaces_a_function_without_quoting_it() {
    let (_d, c) = ctx();
    let src = "pub fn keep() {}\n\npub fn replace_me(a: i32) -> i32 {\n    a * 2\n}\n\npub fn also_keep() {}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    view(&c, "a.rs").await;
    let report = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{
                "old_string": "pub fn replace_me(a: i32) -> i32",
                "new_string": "pub fn replaced() {}\n",
                "whole_block": true,
            }]}),
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

// Three edits in one call all land: each is matched against the file as it
// was, and the one write carries all three.
#[tokio::test]
async fn three_edits_for_one_file_all_land() {
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\n";
    std::fs::write(c.workspace.root().join("a.txt"), src).unwrap();

    view(&c, "a.txt").await;
    tools::edit::Edit
        .execute(
            json!({ "path": "a.txt", "edits": [
                { "insert_after": "one\n", "new_string": "ONE\n" },
                { "insert_after": "two\n", "new_string": "TWO\n" },
                { "insert_after": "three\n", "new_string": "THREE\n" },
            ]}),
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
async fn a_whole_block_takes_the_attribute_above_when_named_by_the_item() {
    let (_d, c) = ctx();
    let src = "#[inline]\npub fn f() {\n    1\n}\n\npub fn g() {}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    view(&c, "a.rs").await;
    tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{
                "old_string": "pub fn f() {",
                "new_string": "pub fn f() { 2 }\n",
                "whole_block": true,
            }]}),
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
async fn a_whole_block_that_names_nothing_refuses() {
    let (_d, c) = ctx();
    let src = "pub fn f() {\n    1\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    view(&c, "a.rs").await;
    let err = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{
                "old_string": "}",
                "new_string": "x",
                "whole_block": true,
            }]}),
            &c,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Edit(_, _)), "{err:?}");
    assert!(err.to_string().contains("no block opens"), "{err}");
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        src
    );
}

#[tokio::test]
async fn a_whole_block_in_an_unparsed_language_says_so() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.txt"), "one\ntwo\n").unwrap();
    view(&c, "a.txt").await;
    let err = tools::edit::Edit
        .execute(
            json!({ "path": "a.txt", "edits": [{
                "old_string": "one",
                "new_string": "x",
                "whole_block": true,
            }]}),
            &c,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("has none"), "{err}");
}

#[tokio::test]
async fn the_outline_names_the_construct_and_feeds_an_edit_without_a_read() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let outline = view(&c, "big.rs").await;
    assert!(outline.contains("326-328:  pub fn new() -> Self {"));
    // The skeleton shows row 326 as `326-328:  pub fn new() -> Self {`: the
    // address names the extent, the text names the block — nothing else was
    // read, and the address is copied back with the line.
    tools::edit::Edit
        .execute(
            json!({ "path": "big.rs", "edits": [{
                "old_string": "326-328:  pub fn new() -> Self {",
                "new_string": "    pub fn new() -> Self { Self { x: 1 } }\n",
                "whole_block": true,
            }]}),
            &c,
        )
        .await
        .unwrap();

    let after = std::fs::read_to_string(c.workspace.root().join("big.rs")).unwrap();
    assert!(after.contains("Self { x: 1 }"), "{}", tail(&after, 200));
    assert!(!after.contains("Self { x: 0 }"));
}
