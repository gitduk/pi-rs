use serde_json::json;
use tools::{Ctx, Registry, Tier, Tool, ToolError, Workspace};

fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

async fn run(tool: &dyn Tool, args: serde_json::Value, ctx: &Ctx) -> String {
    tool.execute(args, ctx).await.unwrap().flatten()
}

#[tokio::test]
async fn read_numbers_lines_under_a_content_tag() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\ntwo\nthree\n").unwrap();

    let out = run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    assert_eq!(
        out.lines().next().unwrap(),
        format!("[a.rs#{}]", hashline::tag("one\ntwo\nthree\n"))
    );
    assert!(out.contains("\n1:one\n2:two\n3:three\n"), "{out}");
}

#[test]
fn the_tag_moves_when_the_file_does() {
    let before = hashline::tag("one\n");
    let after = hashline::tag("one\ntwo\n");
    assert_ne!(before, after, "a stale anchor must be detectable");
    assert_eq!(before.len(), 4);
}

#[tokio::test]
async fn read_honors_offset_and_limit_and_reports_the_remainder() {
    let (_d, c) = ctx();
    let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
    std::fs::write(c.workspace.root().join("a.txt"), &body).unwrap();

    let out = run(
        &tools::read::Read,
        json!({ "path": "a.txt", "offset": 3, "limit": 2 }),
        &c,
    )
    .await;
    assert!(out.contains("3:l3\n4:l4\n"), "{out}");
    assert!(!out.contains("5:l5"), "{out}");
    assert!(out.contains("6 more lines; re-read from 5"), "{out}");
}

#[tokio::test]
async fn read_past_the_end_is_marked_useless_not_an_error() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.txt"), "one\n").unwrap();
    let out = tools::read::Read
        .execute(json!({ "path": "a.txt", "offset": 99 }), &c)
        .await
        .unwrap();
    assert!(out.useless);
}

#[tokio::test]
async fn read_refuses_binary_and_lists_directories() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("bin"), [0u8, 1, 2, 0]).unwrap();
    let out = tools::read::Read
        .execute(json!({ "path": "bin" }), &c)
        .await
        .unwrap();
    assert!(out.useless && out.flatten().contains("binary"));

    std::fs::create_dir(c.workspace.root().join("sub")).unwrap();
    std::fs::write(c.workspace.root().join("sub/x.rs"), "").unwrap();
    let out = run(&tools::read::Read, json!({ "path": "sub" }), &c).await;
    assert!(out.contains("x.rs"), "{out}");
}

#[tokio::test]
async fn write_creates_parents_and_round_trips_through_read() {
    let (_d, c) = ctx();
    let out = run(
        &tools::write::Write,
        json!({ "path": "a/b/c.rs", "content": "fn main() {}\n" }),
        &c,
    )
    .await;
    assert!(out.contains("wrote 1 line,"), "{out}");

    let back = run(&tools::read::Read, json!({ "path": "a/b/c.rs" }), &c).await;
    assert!(back.contains("1:fn main() {}"), "{back}");
    // write and read must agree on the tag, or the first edit is always rejected.
    let tag = out.split('#').nth(1).unwrap().split(']').next().unwrap();
    assert!(back.starts_with(&format!("[a/b/c.rs#{tag}]")), "{back}");
}

#[tokio::test]
async fn tools_refuse_paths_outside_the_workspace() {
    let (_d, c) = ctx();
    let a = tools::read::Read
        .execute(json!({ "path": "../x" }), &c)
        .await;
    let b = tools::write::Write
        .execute(json!({ "path": "/tmp/x", "content": "" }), &c)
        .await;
    let c2 = tools::bash::Bash
        .execute(json!({ "command": "true", "cwd": "../" }), &c)
        .await;
    assert!(matches!(a, Err(ToolError::Escape(_))), "{a:?}");
    assert!(matches!(b, Err(ToolError::Escape(_))), "{b:?}");
    assert!(matches!(c2, Err(ToolError::Escape(_))), "{c2:?}");
}

#[tokio::test]
async fn bash_captures_streams_and_the_exit_code() {
    let (_d, c) = ctx();
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "echo hi; echo bad >&2; exit 3" }),
        &c,
    )
    .await;
    assert!(out.contains("<stdout>\nhi\n</stdout>"), "{out}");
    assert!(out.contains("<stderr>\nbad\n</stderr>"), "{out}");
    assert!(out.contains("exit 3"), "{out}");
}

#[tokio::test]
async fn bash_runs_in_the_workspace_and_can_be_redirected() {
    let (_d, c) = ctx();
    std::fs::create_dir(c.workspace.root().join("sub")).unwrap();
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "pwd", "cwd": "sub" }),
        &c,
    )
    .await;
    assert!(out.contains("sub"), "{out}");
}

#[tokio::test]
async fn bash_times_out_without_hanging_the_turn() {
    let (_d, c) = ctx();
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "sleep 5", "timeout_ms": 150 }),
        &c,
    )
    .await;
    assert!(out.contains("timed out after 150ms"), "{out}");
}

#[tokio::test]
async fn silent_success_is_marked_useless() {
    let (_d, c) = ctx();
    let out = tools::bash::Bash
        .execute(json!({ "command": "true" }), &c)
        .await
        .unwrap();
    assert!(out.useless);
}

#[test]
fn registry_exposes_a_stable_ordered_tool_block() {
    let r = Registry::builtin();
    assert_eq!(r.names(), vec!["bash", "edit", "read", "write"]);
    let names: Vec<String> = r.defs().iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, vec!["bash", "edit", "read", "write"]);
    assert_eq!(r.get("edit").unwrap().tier(), Tier::Write);
    assert_eq!(r.get("bash").unwrap().tier(), Tier::Exec);
    assert_eq!(r.get("read").unwrap().tier(), Tier::Read);
}

#[test]
fn restrict_rejects_a_typo_rather_than_disarming_the_agent() {
    assert_eq!(
        Registry::builtin().restrict(&["reed".into()]).unwrap_err(),
        "reed"
    );
    let r = Registry::builtin()
        .restrict(&["read".into(), "bash".into()])
        .unwrap();
    assert_eq!(r.names(), vec!["bash", "read"]);
}

#[tokio::test]
async fn a_huge_file_is_refused_before_it_is_read_into_memory() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("big.log");
    let f = std::fs::File::create(&path).unwrap();
    f.set_len((10 << 20) + 1).unwrap();
    drop(f);

    let out = tools::read::Read
        .execute(json!({ "path": "big.log" }), &c)
        .await
        .unwrap();
    assert!(out.useless);
    assert!(out.flatten().contains("read limit"), "{}", out.flatten());
}

#[tokio::test]
async fn multibyte_output_respects_the_byte_budget_and_stays_valid_utf8() {
    let (_d, c) = ctx();
    // 60k CJK chars = 180k bytes; a char-counted clamp would blow past the cap.
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "printf '中%.0s' $(seq 1 60000)" }),
        &c,
    )
    .await;
    assert!(
        out.contains("bytes elided"),
        "{}",
        &out[..80.min(out.len())]
    );
    assert!(out.len() < 40_000, "clamped output was {} bytes", out.len());
}

/// Read a file the way the model would, then edit it with the TAG that read returned.
async fn read_then_edit(c: &Ctx, path: &str, ops: &str) -> Result<String, ToolError> {
    let view = run(&tools::read::Read, json!({ "path": path }), c).await;
    let tag = view.split('#').nth(1).unwrap().split(']').next().unwrap();
    let patch = format!("[{path}#{tag}]\n{ops}");
    tools::edit::Edit
        .execute(json!({ "patch": patch }), c)
        .await
        .map(|o| o.flatten())
}

#[tokio::test]
async fn edit_applies_a_patch_anchored_to_the_tag_read_returned() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let report = read_then_edit(&c, "a.rs", "PUT 2.=2:\n+TWO\n")
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
    // The report must carry the new tag and the new numbering, or the model
    // has to re-read before it can edit again.
    assert!(
        report.starts_with(&format!("[a.rs#{}]", hashline::tag("one\nTWO\nthree\n"))),
        "{report}"
    );
    assert!(report.contains("2:TWO"), "{report}");
}

#[tokio::test]
async fn two_edits_in_a_row_need_no_re_read() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let first = read_then_edit(&c, "a.rs", "PUT 1.=1:\n+a\n+b\n")
        .await
        .unwrap();
    let tag = first.split('#').nth(1).unwrap().split(']').next().unwrap();
    let second = format!("[a.rs#{tag}]\nPUT 4.=4:\n+THREE\n");
    tools::edit::Edit
        .execute(json!({ "patch": second }), &c)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "a\nb\ntwo\nTHREE\n"
    );
}

#[tokio::test]
async fn a_stale_tag_leaves_the_file_untouched() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let err = tools::edit::Edit
        .execute(json!({ "patch": "[a.rs#0000]\nPUT 1.=1:\n+X\n" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Re-read it"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\ntwo\n",
        "nothing may be written"
    );
}

#[tokio::test]
async fn a_multi_file_patch_is_all_or_nothing_on_disk() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "a\n").unwrap();
    std::fs::write(c.workspace.root().join("b.rs"), "b\n").unwrap();

    let patch = format!(
        "[a.rs#{}]\nPUT 1.=1:\n+A\n[b.rs#0000]\nPUT 1.=1:\n+B\n",
        hashline::tag("a\n")
    );
    assert!(
        tools::edit::Edit
            .execute(json!({ "patch": patch }), &c)
            .await
            .is_err()
    );
    // a.rs was valid, but a half-applied patch is worse than a rejected one.
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "a\n"
    );
}

#[tokio::test]
async fn edit_moves_a_file_and_reports_the_destination() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\n").unwrap();

    let report = read_then_edit(&c, "a.rs", "PUT 1.=1:\n+ONE\nMV lib/a.rs\n")
        .await
        .unwrap();
    assert!(report.starts_with("a.rs → [lib/a.rs#"), "{report}");
    assert!(!c.workspace.root().join("a.rs").exists());
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("lib/a.rs")).unwrap(),
        "ONE\n"
    );
}

#[tokio::test]
async fn edit_refuses_a_file_it_cannot_read_and_says_to_use_write() {
    let (_d, c) = ctx();
    let err = tools::edit::Edit
        .execute(json!({ "patch": "[new.rs#0000]\nPUT 1.=1:\n+x\n" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("use write to create one"), "{err}");
}

#[tokio::test]
async fn edit_cannot_reach_outside_the_workspace() {
    let (_d, c) = ctx();
    let r = tools::edit::Edit
        .execute(json!({ "patch": "[../escape.rs#0000]\nREM\n" }), &c)
        .await;
    assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
}
