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
async fn read_leaves_the_workspace_but_write_and_bash_do_not() {
    let (_d, c) = ctx();
    let outside = tempfile::tempdir().unwrap();
    let o = outside.path();
    std::fs::write(o.join("x.txt"), "hi\n").unwrap();

    let a = tools::read::Read
        .execute(json!({ "path": o.join("x.txt").to_str().unwrap() }), &c)
        .await
        .unwrap();
    assert!(a.flatten().contains("hi"), "{a:?}");

    let b = tools::write::Write
        .execute(
            json!({ "path": o.join("y.rs").to_str().unwrap(), "content": "fn y() {}\n" }),
            &c,
        )
        .await;
    assert!(matches!(b, Err(ToolError::Escape(_))), "{b:?}");
    assert!(!o.join("y.rs").exists());

    let d = tools::bash::Bash
        .execute(json!({ "command": "pwd", "cwd": o.to_str().unwrap() }), &c)
        .await;
    assert!(matches!(d, Err(ToolError::Escape(_))), "{d:?}");
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
    let err = tools::bash::Bash
        .execute(json!({ "command": "sleep 5", "timeout_ms": 150 }), &c)
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Timeout { ms: 150 }), "{err}");
    assert_eq!(err.code(), Some("TOOL_TIMEOUT"));
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
    assert_eq!(
        r.names(),
        vec!["bash", "edit", "glob", "grep", "read", "todo", "write"]
    );
    let names: Vec<String> = r.defs().iter().map(|d| d.name.clone()).collect();
    assert_eq!(
        names,
        vec!["bash", "edit", "glob", "grep", "read", "todo", "write"]
    );
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
        out.contains("bytes omitted"),
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

    let report = read_then_edit(&c, "a.rs", "PUT 2:\n+TWO\n")
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
async fn edit_writes_new_rows_with_the_files_line_ending() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\r\ntwo\r\nthree\r\n").unwrap();
    let view = run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    let tag = view.split('#').nth(1).unwrap().split(']').next().unwrap();
    let out = tools::edit::Edit
        .execute(
            json!({ "patch": format!("[a.rs#{tag}]\nPUT 2:\n+TWO\n") }),
            &c,
        )
        .await
        .unwrap();
    let written = std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap();
    assert_eq!(written, "one\r\nTWO\r\nthree\r\n");
    assert!(out.preview.is_some(), "a real change sketches what it did");
}

const THREE_FNS: &str = "\
pub fn keep() -> i32 {
    1
}

pub fn target() -> i32 {
    2
}
";

#[tokio::test]
async fn a_range_one_line_short_is_refused_rather_than_applied() {
    // `target` runs 5..=7. The model writes the whole function, brace and all,
    // but names 5..=6 — so the original line 7 survives and the file has one
    // brace too many. The patch itself is perfectly well formed; nothing before
    // this check could tell that the end was resolved wrong. That is the cost
    // of writing ranges by hand instead of letting `N*` find the boundary.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, THREE_FNS).unwrap();

    let err = read_then_edit(
        &c,
        "a.rs",
        "PUT 5-6:\n+pub fn target() -> i32 {\n+    99\n+}\n",
    )
    .await
    .unwrap_err();
    let said = err.to_string();
    // The row's own text: a bare line number invites a story about the parser.
    assert!(said.contains("line 8 is `}`"), "{said}");
    assert!(said.contains("Nothing was written"), "{said}");

    // Refused means refused: the file on disk is untouched.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);
}

#[tokio::test]
async fn the_same_range_named_correctly_still_applies() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();

    let out = read_then_edit(
        &c,
        "a.rs",
        "PUT 5-7:\n+pub fn target() -> i32 {\n+    99\n+}\n",
    )
    .await
    .unwrap();
    assert!(out.contains("99"), "{out}");
}

#[tokio::test]
async fn a_file_that_was_already_broken_stays_editable() {
    // The check is "parsed before, does not now". A file that never parsed is
    // usually the reason an edit is happening; refusing it would strand the
    // model with no way to repair it.
    let (_d, c) = ctx();
    let broken = "pub fn a() -> i32 {\n    1\n";
    std::fs::write(c.workspace.root().join("a.rs"), broken).unwrap();

    let out = read_then_edit(&c, "a.rs", "PUT 2:\n+    2\n")
        .await
        .unwrap();
    assert!(out.contains("2"), "{out}");
}

#[tokio::test]
async fn a_language_the_parser_does_not_know_is_not_gated() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.toml"), "[a]\nb = 1\n").unwrap();

    let out = read_then_edit(&c, "a.toml", "PUT 2:\n+b = 2\n")
        .await
        .unwrap();
    assert!(out.contains("b = 2"), "{out}");
}

#[tokio::test]
async fn an_edit_shows_what_went_and_what_came() {
    // The report the model reads is a set of addresses it can edit against.
    // The display answers the other question — what changed — and only that
    // one has a reader.
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();
    let tag = hashline::tag(THREE_FNS);

    let out = tools::edit::Edit
        .execute(
            json!({ "patch": format!("[a.rs#{tag}]\nPUT 5-7:\n+pub fn target() -> i32 {{\n+    99\n+}}\n") }),
            &c,
        )
        .await
        .unwrap();

    let sketch = out.preview.unwrap();
    let (head, rows) = sketch.split_once('\n').unwrap();
    assert_eq!(head, "a.rs +3 -3");
    // The diff rows carry the file line each one was or became, so a reader
    // can locate the change without counting diff rows. The mark is the
    // second word, after the row number.
    assert!(
        rows.contains(&format!("6 + {}{}", " ".repeat(4), "99")),
        "{rows}"
    );
    assert!(rows.contains("5 - pub fn target() -> i32 {"), "{rows}");
    assert!(
        rows.lines()
            .filter(|l| l.split_whitespace().nth(1) == Some("-"))
            .count()
            == 3,
        "{rows}"
    );
    // The addresses stay where the model reads them, not here.
    assert!(!sketch.contains("5-7:"), "{sketch}");
}

#[tokio::test]
async fn each_side_of_a_hunk_is_numbered_in_the_file_it_belongs_to() {
    // The first hunk swaps one line for two, so the removed `b` was line 2
    // before the patch but sits at line 3 after it. Removed rows must show
    // the old number, added rows the new one.
    let (_d, c) = ctx();
    let src = "a\nb\nc\nd\ne\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    let tag = hashline::tag(src);
    let out = tools::edit::Edit
        .execute(
            json!({ "patch": format!("[a.rs#{tag}]\nPUT 1:\n+AA\n+BB\nCUT 2:\n") }),
            &c,
        )
        .await
        .unwrap();
    let sketch = out.preview.unwrap();
    assert!(sketch.contains("2 - b"), "{sketch}");
    assert!(sketch.contains("2 + BB"), "{sketch}");
}

#[tokio::test]
async fn a_cut_reports_the_lines_it_took() {
    // A hunk that gives nothing has no row in the new file to name, and is
    // exactly the one a reader most wants shown.
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();
    let tag = hashline::tag(THREE_FNS);

    let out = tools::edit::Edit
        .execute(json!({ "patch": format!("[a.rs#{tag}]\nCUT 5-7\n") }), &c)
        .await
        .unwrap();

    let sketch = out.preview.unwrap();
    assert!(sketch.starts_with("a.rs +0 -3"), "{sketch}");
    assert_eq!(
        sketch
            .lines()
            .filter(|l| l.split_whitespace().nth(1) == Some("-"))
            .count(),
        3
    );
    assert!(sketch.contains("\n5 - "), "{sketch}");
}

#[tokio::test]
async fn a_file_the_patch_did_not_change_is_not_counted_as_one_that_did() {
    // The report says "unchanged" and nothing is written; a head reading
    // "2 files" tells whoever only sees the display the opposite.
    let (_d, c) = ctx();
    let root = c.workspace.root();
    std::fs::write(root.join("a.rs"), THREE_FNS).unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();

    let patch = format!(
        "[a.rs#{}]\nPUT 5-7:\n+pub fn target() -> i32 {{\n+    99\n+}}\n[b.rs#{}]\nPUT 1:\n+fn b() {{}}\n",
        hashline::tag(THREE_FNS),
        hashline::tag("fn b() {}\n"),
    );
    let out = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap();

    let sketch = out.preview.unwrap();
    assert!(sketch.starts_with("a.rs +3 -3"), "{sketch}");
    assert!(!sketch.contains("b.rs"), "{sketch}");
    // One file left standing, so nothing has to be told apart by name.
    assert!(!sketch.lines().any(|l| l == "a.rs"), "{sketch}");
}

#[tokio::test]
async fn two_files_each_say_which_hunks_are_theirs() {
    let (_d, c) = ctx();
    let root = c.workspace.root();
    std::fs::write(root.join("a.rs"), THREE_FNS).unwrap();
    std::fs::write(root.join("b.rs"), "fn b() {}\n").unwrap();

    let patch = format!(
        "[a.rs#{}]\nPUT 2:\n+    11\n[b.rs#{}]\nPUT 1:\n+fn b() -> i32 {{ 2 }}\n",
        hashline::tag(THREE_FNS),
        hashline::tag("fn b() {}\n"),
    );
    let out = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap();

    let sketch = out.preview.unwrap();
    assert!(sketch.starts_with("2 files +2 -2"), "{sketch}");
    // Diff rows lead with their row number, so a name row is anything whose
    // second word is not the `+`/`-` mark.
    let named: Vec<&str> = sketch
        .lines()
        .filter(|l| !matches!(l.split_whitespace().nth(1), Some("+") | Some("-")))
        .collect();
    assert_eq!(named, vec!["2 files +2 -2", "a.rs", "b.rs"], "{sketch}");
}

#[tokio::test]
async fn a_removed_file_counts_the_lines_that_went_with_it() {
    // `+0 -0` on a delete reads as nothing having happened.
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();
    let tag = hashline::tag(THREE_FNS);

    let out = tools::edit::Edit
        .execute(json!({ "patch": format!("[a.rs#{tag}]\nRM\n") }), &c)
        .await
        .unwrap();
    let n = THREE_FNS.lines().count();
    assert_eq!(out.preview.unwrap(), format!("a.rs +0 -{n}"));
}

#[tokio::test]
async fn an_overwrite_that_would_not_parse_is_refused() {
    // The whole-file counterpart of a range that stops one line short: content
    // that ran out mid-file. The tool cannot tell a short answer from a short
    // file, but the parser can, and the tail of a working file is what is lost.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, THREE_FNS).unwrap();

    let err = tools::write::Write
        .execute(
            json!({ "path": "a.rs", "content": "pub fn a() -> i32 {\n    1\n" }),
            &c,
        )
        .await
        .unwrap_err();
    let said = err.to_string();
    // The construct that never closed, not the line the content stopped on:
    // that is the one the model has to look at to see what it left out.
    assert!(said.contains("line 1 is `pub fn a() -> i32 {`"), "{said}");
    assert!(said.contains("Nothing was written"), "{said}");

    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);
}

#[tokio::test]
async fn emptying_a_file_is_not_breaking_it() {
    // The gate's one plausible false refusal, and the parsers all disagree
    // with it: empty content is valid in every language the tree knows.
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();

    let out = run(
        &tools::write::Write,
        json!({ "path": "a.rs", "content": "" }),
        &c,
    )
    .await;
    assert!(out.contains("wrote 0 lines"), "{out}");
}

#[tokio::test]
async fn a_new_file_is_never_gated() {
    // A stub, a scaffold, half a file about to be finished: nothing behind it
    // to lose, so the parser has no standing to refuse it.
    let (_d, c) = ctx();
    let out = run(
        &tools::write::Write,
        json!({ "path": "stub.rs", "content": "pub fn a() -> i32 {\n" }),
        &c,
    )
    .await;
    assert!(out.contains("wrote 1 line"), "{out}");
}

#[tokio::test]
async fn a_file_that_was_already_broken_stays_writable() {
    // Same rule as edit's: a file the write found broken is not the write's
    // doing, and refusing there is how the model ends up with no way back.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "pub fn a() -> i32 {\n    1\n").unwrap();

    let out = run(
        &tools::write::Write,
        json!({ "path": "a.rs", "content": "pub fn a() -> i32 {\n    2\n" }),
        &c,
    )
    .await;
    assert!(out.contains("wrote 2 lines"), "{out}");
    assert!(std::fs::read_to_string(&path).unwrap().contains("2"));
}

#[tokio::test]
async fn a_write_in_a_language_the_parser_does_not_know_is_not_gated() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.toml"), "[a]\nb = 1\n").unwrap();

    let out = run(
        &tools::write::Write,
        json!({ "path": "a.toml", "content": "[a\n" }),
        &c,
    )
    .await;
    assert!(out.contains("wrote 1 line"), "{out}");
}

#[tokio::test]
async fn two_edits_in_a_row_need_no_re_read() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let first = read_then_edit(&c, "a.rs", "PUT 1:\n+a\n+b\n")
        .await
        .unwrap();
    let tag = first.split('#').nth(1).unwrap().split(']').next().unwrap();
    let second = format!("[a.rs#{tag}]\nPUT 4:\n+THREE\n");
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
        .execute(json!({ "patch": "[a.rs#0000]\nPUT 1:\n+X\n" }), &c)
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
        "[a.rs#{}]\nPUT 1:\n+A\n[b.rs#0000]\nPUT 1:\n+B\n",
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

    let report = read_then_edit(&c, "a.rs", "PUT 1:\n+ONE\nMV lib/a.rs\n")
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
        .execute(json!({ "patch": "[new.rs#0000]\nPUT 1:\n+x\n" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("use write to create one"), "{err}");
}

#[tokio::test]
async fn edit_cannot_reach_outside_the_workspace() {
    let (_d, c) = ctx();
    let r = tools::edit::Edit
        .execute(json!({ "patch": "[../escape.rs#0000]\nRM\n" }), &c)
        .await;
    assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
}

#[tokio::test]
async fn bash_previews_the_command_that_ran() {
    let (_d, c) = ctx();
    let out = tools::bash::Bash
        .execute(json!({ "command": "echo first; echo second" }), &c)
        .await
        .unwrap();
    // A progress line says what ran, not what it printed — the output is the
    // result body, which opens with the `<stdout>` marker.
    assert!(out.flatten().starts_with("<stdout>"));
    assert_eq!(out.preview(), "echo first; echo second");
}

#[tokio::test]
async fn a_multiline_command_previews_only_its_first_line() {
    let (_d, c) = ctx();
    let out = tools::bash::Bash
        .execute(json!({ "command": "echo one\necho two" }), &c)
        .await
        .unwrap();
    // The preview feeds a one-line progress row; a newline would leak the
    // rest into the diff-row renderer as fake structure.
    assert_eq!(out.preview(), "echo one");
}

#[tokio::test]
async fn a_failing_command_previews_the_command_too() {
    let (_d, c) = ctx();
    let out = tools::bash::Bash
        .execute(json!({ "command": "echo boom >&2; exit 2" }), &c)
        .await
        .unwrap();
    assert_eq!(out.preview(), "echo boom >&2; exit 2");
}

#[tokio::test]
async fn tools_whose_result_opens_with_content_need_no_explicit_preview() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\n").unwrap();
    let out = tools::read::Read
        .execute(json!({ "path": "a.rs" }), &c)
        .await
        .unwrap();
    assert_eq!(out.preview(), format!("[a.rs#{}]", hashline::tag("one\n")));
}

#[tokio::test]
async fn a_ranged_read_previews_the_rows_that_came_back() {
    // The progress line carries the real window, not the ask: the limit may
    // reach past the end of the file, and the preview should say 2-3, not
    // 2-12.
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    let out = tools::read::Read
        .execute(json!({ "path": "a.rs", "offset": 2, "limit": 10 }), &c)
        .await
        .unwrap();
    assert_eq!(
        out.preview(),
        format!("[a.rs#{} 2-3]", hashline::tag(src))
    );
}

#[tokio::test]
async fn two_edits_to_one_file_in_the_same_turn_do_not_clobber_each_other() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    let src = "one\ntwo\nthree\n";
    std::fs::write(&path, src).unwrap();
    let tag = hashline::tag(src);

    // Both patches are valid against the same read, and the loop runs shared
    // tools concurrently.
    let first = tools::edit::Edit.execute(
        json!({ "patch": format!("[a.rs#{tag}]\nPUT 1:\n+ONE\n") }),
        &c,
    );
    let second = tools::edit::Edit.execute(
        json!({ "patch": format!("[a.rs#{tag}]\nPUT 3:\n+THREE\n") }),
        &c,
    );
    let (a, b) = tokio::join!(first, second);

    // Serialized, so the second one sees a file that moved under it and says
    // so. A patch format cannot merge here; what it can do is refuse quietly
    // instead of losing a change silently.
    let (ok, failed) = match (&a, &b) {
        (Ok(_), Err(e)) => (a.as_ref().unwrap(), e),
        (Err(e), Ok(_)) => (b.as_ref().unwrap(), e),
        _ => panic!("exactly one must land: {a:?} / {b:?}"),
    };
    assert!(ok.flatten().starts_with("[a.rs#"));
    assert!(failed.to_string().contains("Re-read it"), "{failed}");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(
        after == "ONE\ntwo\nthree\n" || after == "one\ntwo\nTHREE\n",
        "{after:?}"
    );
}

#[tokio::test]
async fn write_strips_display_prefixes_the_model_copied_from_read() {
    let (_d, c) = ctx();
    std::fs::write(
        c.workspace.root().join("a.rs"),
        "fn main() {}\nlet x = 1;\n",
    )
    .unwrap();
    let view = run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    // The whole read output, pasted straight back — a habit the numbered format
    // invites and one nothing else catches.
    tools::write::Write
        .execute(json!({ "path": "b.rs", "content": view }), &c)
        .await
        .unwrap();

    let written = std::fs::read_to_string(c.workspace.root().join("b.rs")).unwrap();
    assert_eq!(written, "fn main() {}\nlet x = 1;\n", "got: {written:?}");
}

#[tokio::test]
async fn a_zero_timeout_is_not_an_instant_kill() {
    let (_d, c) = ctx();
    // A command that takes real time: with zero read literally, this dies
    // before it starts.
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "sleep 0.3; echo hi", "timeout_ms": 0 }),
        &c,
    )
    .await;
    assert!(out.contains("hi"), "{out}");
}

#[tokio::test]
async fn an_over_long_output_is_kept_somewhere_the_model_can_reach() {
    let (_d, c) = ctx();
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "printf 'z%.0s' $(seq 1 40000); echo; echo MIDDLE_MARKER; printf 'z%.0s' $(seq 1 40000)" }),
        &c,
    )
    .await;

    assert!(
        out.contains("bytes omitted"),
        "{}",
        &out[..90.min(out.len())]
    );
    let locator = out
        .lines()
        .find_map(|l| l.strip_prefix("full output: ").and_then(|l| l.split(' ').next()))
        .expect("the omitted middle must be recoverable");
    let whole = std::fs::read_to_string(c.spill_path(locator).unwrap()).unwrap();
    assert!(
        whole.contains("MIDDLE_MARKER"),
        "the spill must hold what the result dropped"
    );
    let _ = std::fs::remove_file(c.spill_path(locator).unwrap());
}

#[tokio::test]
async fn read_spills_an_over_long_view_and_reads_it_back_by_locator() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let c = Ctx::new(ws).with_spill_root(dir.path().join("spill"));
    let big: String = (1..=30_000).map(|i| format!("line {i}\n")).collect();
    std::fs::write(c.workspace.root().join("big.txt"), &big).unwrap();

    let out = tools::read::Read
        .execute(
            json!({ "path": "big.txt", "limit": 30_000, "outline": false }),
            &c,
        )
        .await
        .unwrap()
        .flatten();
    assert!(out.contains("bytes omitted"), "{out}");
    let locator = out
        .lines()
        .find_map(|l| l.strip_prefix("full output: ").and_then(|l| l.split(' ').next()))
        .expect("the read view must be recoverable");

    // Reading the spill back re-numbers its lines: spill line 2 is `1:line 1`,
    // so it comes back as row `2:1:line 1`.
    let again = tools::read::Read
        .execute(json!({ "path": locator, "limit": 5 }), &c)
        .await
        .unwrap()
        .flatten();
    assert!(again.contains("2:1:line 1\n3:2:line 2"), "{again}");
    assert!(!again.contains("full output:"), "{again}");
}

#[tokio::test]
async fn a_spill_that_cannot_be_written_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let c = Ctx::new(ws).with_spill_root(dir.path().join("spill"));
    // A file where the session directory would go: create_dir_all cannot.
    std::fs::write(dir.path().join("spill"), "in the way").unwrap();
    let err = tools::bash::Bash
        .execute(
            json!({ "command": "printf 'z%.0s' $(seq 1 40000)" }),
            &c,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("SPILL_FAILED"));
    assert!(err.to_string().contains("could not spill"), "{err}");
}

#[tokio::test]
async fn a_short_output_leaves_no_file_behind() {
    let (_d, c) = ctx();
    let out = run(&tools::bash::Bash, json!({ "command": "echo hi" }), &c).await;
    assert!(!out.contains("full output:"), "{out}");
}

#[tokio::test]
async fn an_edit_that_changes_nothing_says_so() {
    let (_d, c) = ctx();
    let src = "one\ntwo\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();

    // A patch whose body already matches. It "succeeds", and the model has no
    // way to tell its fix did not land.
    let patch = format!("[a.rs#{}]\nPUT 1:\n+one\n", hashline::tag(src));
    let out = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap()
        .flatten();
    assert!(out.contains("unchanged"), "{out}");
}

#[tokio::test]
async fn a_deletion_is_reported_by_what_it_deleted() {
    let (_d, c) = ctx();
    let before = "one\ntwo\nthree\nfour\n";
    std::fs::write(c.workspace.root().join("a.rs"), before).unwrap();
    let tag = hashline::tag(before);

    let out = run(
        &tools::edit::Edit,
        json!({ "patch": format!("[a.rs#{tag}]\nCUT 2-3") }),
        &c,
    )
    .await;

    // "no lines added" is true of a pure CUT and reads as nothing happening.
    assert!(out.contains("removed 2 lines"), "{out}");
    assert!(!out.contains("no lines added"), "{out}");
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "one\nfour\n"
    );
}

#[tokio::test]
async fn every_form_the_table_lists_is_one_the_parser_takes() {
    // The FORMAT string is prose sent to the model on every request, and the
    // parser is what reads back what the model writes from it. They were kept
    // in step by hand through the last grammar change, and a stale hint two
    // lines from the rewrite is what that costs.
    let (_d, c) = ctx();
    let before = "one\ntwo\nthree\nfour\n";
    std::fs::write(c.workspace.root().join("a.rs"), before).unwrap();
    let tag = hashline::tag(before);

    let described = tools::edit::Edit.description();
    for form in hashline::FORMS {
        let named = format!("N{}", form.suffix);
        assert!(
            described.contains(&named),
            "the description omits `{named}`"
        );
        assert!(
            described.contains(form.means.split(". ").next().unwrap_or(form.means)),
            "the description omits what `{named}` means"
        );
        // Concrete numbers on one line, so the patch stays applicable.
        let spec = named.replace('N', "2").replace('M', "3");
        let patch = format!("[a.rs#{tag}]\nPUT {spec}:\n+x\n");
        let err = hashline::parse(&patch).err();
        assert!(
            err.is_none(),
            "the table lists `{named}`, parser says {err:?}"
        );
    }
    assert!(hashline::FORMS.len() >= 2, "the table emptied out");
}

#[tokio::test]
async fn a_range_that_is_not_one_is_refused_by_the_op_that_wrote_it() {
    let (_d, c) = ctx();
    let before = "one\ntwo\nthree\n";
    std::fs::write(c.workspace.root().join("a.rs"), before).unwrap();
    let tag = hashline::tag(before);

    let complaint = async |patch: String| {
        tools::edit::Edit
            .execute(json!({ "patch": patch }), &c)
            .await
            .unwrap_err()
            .to_string()
    };

    // The same mistake under two verbs must not draw the same words: a model
    // that fixes the PUT and sees the message again cannot tell it made
    // progress.
    let put = complaint(format!("[a.rs#{tag}]\nPUT two:\n+x")).await;
    let cut = complaint(format!("[a.rs#{tag}]\nCUT two")).await;
    assert!(put.contains("an address is"), "{put}");
    assert!(cut.contains("an address is"), "{cut}");
    assert_ne!(put, cut);
}

#[tokio::test]
async fn text_that_is_not_utf8_is_refused_rather_than_mangled() {
    let (_d, c) = ctx();
    // Latin-1 prose: no NUL byte, so a NUL sniff calls it text, and a lossy
    // decode turns every accent into U+FFFD.
    let mut bytes = b"caf\xe9 na\xefve r\xe9sum\xe9\n".to_vec();
    bytes.extend_from_slice(&b"pr\xe9cis \xe0 la carte\n".repeat(40));
    std::fs::write(c.workspace.root().join("latin.txt"), &bytes).unwrap();

    let out = tools::read::Read
        .execute(json!({ "path": "latin.txt" }), &c)
        .await
        .unwrap();
    assert!(
        !out.flatten().contains('\u{FFFD}'),
        "mojibake reached the model: {}",
        out.flatten()
    );
}
