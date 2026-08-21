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
        format!("[a.rs#{}]", tools::read::tag("one\ntwo\nthree\n"))
    );
    assert!(out.contains("\n1:one\n2:two\n3:three\n"), "{out}");
}

#[test]
fn the_tag_moves_when_the_file_does() {
    let before = tools::read::tag("one\n");
    let after = tools::read::tag("one\ntwo\n");
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
    assert!(out.contains("wrote 1 lines"), "{out}");

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
    assert_eq!(r.names(), vec!["bash", "read", "write"]);
    let names: Vec<String> = r.defs().iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, vec!["bash", "read", "write"]);
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
