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

/// A file long enough to trigger the skeleton, with two real declarations in it.
fn long_rust() -> String {
    let filler: String = (0..320).map(|i| format!("// filler {i}\n")).collect();
    format!(
        "{filler}pub struct Point {{\n    x: i32,\n}}\n\nimpl Point {{\n    pub fn new() -> Self {{\n        Self {{ x: 0 }}\n    }}\n}}\n"
    )
}

#[tokio::test]
async fn a_long_file_comes_back_as_a_skeleton() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let out = run(&tools::read::Read, json!({ "path": "big.rs" }), &c).await;
    assert!(
        out.starts_with(&format!(
            "[big.rs#{}] 329 lines · outline",
            hashline::tag(&body)
        )),
        "{out}"
    );
    assert!(out.contains("321:pub struct Point {"), "{out}");
    assert!(out.contains("326:    pub fn new() -> Self {"), "{out}");
    // 329 lines of source must not come back as 329 lines of output.
    assert!(out.lines().count() < 10, "{out}");
    assert!(!out.contains("// filler"), "{out}");
}

#[tokio::test]
async fn a_range_request_is_answered_with_lines_not_a_skeleton() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("big.rs"), long_rust()).unwrap();

    // Asking for a range is an explicit ask for lines.
    let out = run(
        &tools::read::Read,
        json!({ "path": "big.rs", "offset": 321, "limit": 3 }),
        &c,
    )
    .await;
    assert!(out.contains("321:pub struct Point {"), "{out}");
    assert!(out.contains("322:    x: i32,"), "{out}");
    assert!(!out.contains("outline"), "{out}");
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
async fn a_block_op_replaces_a_whole_function_without_counting_lines() {
    let (_d, c) = ctx();
    let src = "pub fn keep() {}\n\npub fn replace_me(a: i32) -> i32 {\n    a * 2\n}\n\npub fn also_keep() {}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    let patch = format!(
        "[a.rs#{}]\nPUT 3*:\n+pub fn replaced() {{}}\n",
        hashline::tag(src)
    );
    let report = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap()
        .flatten();

    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "pub fn keep() {}\n\npub fn replaced() {}\n\npub fn also_keep() {}\n"
    );
    assert!(report.contains("3:pub fn replaced() {}"), "{report}");
}

#[tokio::test]
async fn a_block_op_takes_the_attribute_above_when_pointed_at_it() {
    let (_d, c) = ctx();
    let src = "#[inline]\npub fn f() {\n    1\n}\n\npub fn g() {}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    let patch = format!(
        "[a.rs#{}]\nPUT 1*:\n+pub fn f() {{ 2 }}\n",
        hashline::tag(src)
    );
    tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "pub fn f() { 2 }\n\npub fn g() {}\n"
    );
}

#[tokio::test]
async fn a_block_op_on_a_closing_brace_is_refused() {
    let (_d, c) = ctx();
    let src = "pub fn f() {\n    1\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    let patch = format!("[a.rs#{}]\nPUT 3*:\n+x\n", hashline::tag(src));
    let err = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Invalid(_)));
    assert!(err.to_string().contains("opens no construct"), "{err}");
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        src
    );
}

#[tokio::test]
async fn a_block_op_in_an_unparsed_language_says_so() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.txt"), "one\ntwo\n").unwrap();
    let patch = format!("[a.txt#{}]\nPUT 1*:\n+x\n", hashline::tag("one\ntwo\n"));
    let err = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Name the lines with `N.=M`"),
        "{err}"
    );
}

#[tokio::test]
async fn an_outline_line_number_feeds_straight_into_a_block_edit() {
    let (_d, c) = ctx();
    let body = long_rust();
    std::fs::write(c.workspace.root().join("big.rs"), &body).unwrap();

    let outline = run(&tools::read::Read, json!({ "path": "big.rs" }), &c).await;
    let tag = outline
        .split('#')
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap()
        .to_string();
    // The skeleton gave line 326 for `pub fn new`; nothing else was read.
    let patch =
        format!("[big.rs#{tag}]\nPUT 326*:\n+    pub fn new() -> Self {{ Self {{ x: 1 }} }}\n");
    tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
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
