mod common;

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
async fn read_heads_the_view_with_the_file_path() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\ntwo\nthree\n").unwrap();

    let out = run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    assert_eq!(out.lines().next().unwrap(), "[a.rs]");
    assert!(out.contains("\n1:one\n2:two\n3:three\n"), "{out}");
}

#[test]
fn the_content_hash_moves_when_the_file_does() {
    let before = hashline::view_hash("one\n");
    let after = hashline::view_hash("one\ntwo\n");
    assert_ne!(before, after, "a stale view must be detectable");
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
    // write and read must agree on the view hash, or the staleness note fires
    // on the very first edit.
    assert!(back.starts_with("[a/b/c.rs]"), "{back}");
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
async fn bash_marks_whitespace_only_output_useless() {
    let (_d, c) = ctx();
    let out = run(&tools::bash::Bash, json!({ "command": "echo" }), &c).await;
    assert!(out.contains("exit 0, no output"), "{out}");
    assert!(!out.contains("<stdout>"), "{out}");
}

#[tokio::test]
async fn read_huge_limit_stays_inside_the_file() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let out = run(
        &tools::read::Read,
        json!({ "path": "a.txt", "offset": 2, "limit": u64::MAX }),
        &c,
    )
    .await;
    assert!(out.contains("2:two\n3:three"), "{out}");
}

#[tokio::test]
async fn bash_spills_a_runaway_output_instead_of_holding_it() {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    let c = Ctx::new(ws).with_spill_root(dir.path().join("spill"));
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "head -c 40000 /dev/zero | tr '\\0' 'x'" }),
        &c,
    )
    .await;
    assert!(out.contains("<stdout>\nxxx"), "{out}");
    assert!(out.contains("bytes omitted"), "{out}");
    assert!(out.contains("full output: spill:"), "{out}");
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

// A removed worktree takes the shell's ground with it, and spawn answers
// with a bare ENOENT that reads exactly like a missing command. Saying which
// directory went is the difference between the model moving on and the model
// running the same call again — which is what one session did, three times.
#[tokio::test]
async fn a_working_directory_that_went_says_which_one() {
    let (d, c) = ctx();
    let root = d.path().to_path_buf();
    std::fs::remove_dir_all(&root).unwrap();

    let err = tools::bash::Bash
        .execute(json!({ "command": "true" }), &c)
        .await
        .unwrap_err();
    let said = err.to_string();
    assert!(said.contains("working directory is gone"), "{said}");
    assert!(said.contains(&root.display().to_string()), "{said}");
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
    let expected = vec!["bash", "edit", "fetch", "glob", "grep", "read", "write"];
    assert_eq!(r.names(), expected);
    let names: Vec<String> = r.defs().iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, expected);
    assert_eq!(r.get("edit").unwrap().tier(), Tier::Write);
    assert_eq!(r.get("bash").unwrap().tier(), Tier::Exec);
    assert_eq!(r.get("read").unwrap().tier(), Tier::Read);
    assert_eq!(r.get("fetch").unwrap().tier(), Tier::Net);
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

// Read a file the way the model would, then edit it. Read-before-edit is
// tracked by the session, so no tag is passed any more.
async fn read_then_edit(c: &Ctx, path: &str, ops: &str) -> Result<String, ToolError> {
    run(&tools::read::Read, json!({ "path": path }), c).await;
    let patch = format!("[{path}]\n{ops}");
    tools::edit::Edit
        .execute(json!({ "patch": patch }), c)
        .await
        .map(|o| o.flatten())
}

#[tokio::test]
async fn edit_applies_a_patch_built_from_the_last_view() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let report = read_then_edit(&c, "a.rs", "-two\n+TWO\n").await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
    // The report echoes the rows it landed, numbered in the new file.
    assert!(report.starts_with("[a.rs]"), "{report}");
    assert!(report.contains("2:TWO"), "{report}");
}

#[tokio::test]
async fn edit_writes_new_rows_with_the_files_line_ending() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\r\ntwo\r\nthree\r\n").unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    let out = tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n-two\n+TWO\n" }), &c)
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
async fn a_replacement_that_breaks_the_parse_is_refused_rather_than_applied() {
    // The anchor matches and the patch is well formed; only the parse gate
    // can tell that the file as patched would not compile.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, THREE_FNS).unwrap();

    let err = read_then_edit(
        &c,
        "a.rs",
        "=pub fn target() -> i32 {\n-    2\n-}\n+}\n+}\n",
    )
    .await
    .unwrap_err();
    let said = err.to_string();
    // The row's own text: a bare line number invites a story about the parser.
    assert!(
        said.contains("line 7 of what this one produces is `}`"),
        "{said}"
    );
    assert!(said.contains("Nothing was written"), "{said}");

    // Refused means refused: the file on disk is untouched.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);
}

#[tokio::test]
async fn the_same_edit_anchored_by_content_still_applies() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();

    let out = read_then_edit(
        &c,
        "a.rs",
        "=pub fn target() -> i32 {\n-    2\n-}\n+    99\n+}\n",
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

    let out = read_then_edit(&c, "a.rs", "-    1\n+    2\n")
        .await
        .unwrap();
    assert!(out.contains("2"), "{out}");
}

#[tokio::test]
async fn a_language_the_parser_does_not_know_is_not_gated() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.toml"), "[a]\nb = 1\n").unwrap();

    let out = read_then_edit(&c, "a.toml", "=b = 1\n+b = 2\n")
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
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    let out = tools::edit::Edit
        .execute(
            json!({ "patch": "[a.rs]\n=pub fn target() -> i32 {\n-    2\n-}\n+    99\n+}\n" }),
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
}

#[tokio::test]
async fn each_side_of_a_hunk_is_numbered_in_the_file_it_belongs_to() {
    // The first hunk swaps one line for two, so the removed `b` was line 2
    // before the patch but sits at line 3 after it. Removed rows must show
    // the old number, added rows the new one.
    let (_d, c) = ctx();
    let src = "a\nb\nc\nd\ne\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    let out = tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n=a\n+AA\n+BB\n-b\n" }), &c)
        .await
        .unwrap();
    let sketch = out.preview.unwrap();
    assert!(sketch.contains("2 - b"), "{sketch}");
    assert!(sketch.contains("3 + BB"), "{sketch}");
}

#[tokio::test]
async fn a_delete_reports_the_lines_it_took() {
    // A hunk that gives nothing has no row in the new file to name, and is
    // exactly the one a reader most wants shown.
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), THREE_FNS).unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    let out = tools::edit::Edit
        .execute(
            json!({ "patch": "[a.rs]\n-pub fn target() -> i32 {\n-    2\n-}\n" }),
            &c,
        )
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
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    run(&tools::read::Read, json!({ "path": "b.rs" }), &c).await;

    let patch = "[a.rs]\n=pub fn target() -> i32 {\n-    2\n-}\n+    99\n+}\n\n[b.rs]\n-fn b() {}\n+fn b() {}\n";
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
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    run(&tools::read::Read, json!({ "path": "b.rs" }), &c).await;

    let patch = "[a.rs]\n=pub fn keep() -> i32 {\n-    1\n+    11\n\n[b.rs]\n-fn b() {}\n+fn b() -> i32 { 2 }\n";
    let out = tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap();

    let sketch = out.preview.unwrap();
    assert!(sketch.starts_with("2 files +3 -3"), "{sketch}");
    // Diff rows lead with their row number, so a name row is anything whose
    // second word is not the `+`/`-` mark.
    let named: Vec<&str> = sketch
        .lines()
        .filter(|l| !matches!(l.split_whitespace().nth(1), Some("+") | Some("-")))
        .collect();
    assert_eq!(named, vec!["2 files +3 -3", "a.rs", "b.rs"], "{sketch}");
}

#[tokio::test]
async fn an_overwrite_that_would_not_parse_is_refused() {
    // The whole-file counterpart of a replacement that stops one line short:
    // content that ran out mid-file. The tool cannot tell a short answer from
    // a short file, but the parser can, and the tail of a working file is
    // what is lost.
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

// The drift-immunity payoff: two edits back to back with no read between
// them, the second anchored on content the first one put there. Content
// anchors cannot go stale, so this just works.
#[tokio::test]
async fn a_second_edit_right_after_the_first_applies_without_a_reread() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n=one\n+one!\n" }), &c)
        .await
        .unwrap();
    tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n=three\n+three!\n" }), &c)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\none!\ntwo\nthree\nthree!\n",
    );
}

#[tokio::test]
async fn an_edit_whose_anchor_is_gone_leaves_the_file_untouched() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    // The file changed after the view: the anchor the patch carries is no
    // longer there, and nothing may be written.
    std::fs::write(&path, "something else\n").unwrap();

    let err = tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n-two\n" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("no match"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "something else\n",
        "nothing may be written"
    );
}

#[tokio::test]
async fn a_multi_file_patch_is_all_or_nothing_on_disk() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "a\n").unwrap();
    std::fs::write(c.workspace.root().join("b.rs"), "b\n").unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    run(&tools::read::Read, json!({ "path": "b.rs" }), &c).await;

    // b.rs carries an anchor that matches nothing: the whole patch refuses.
    let patch = "[a.rs]\n=a\n+A\n[b.rs]\n=zzz\n+B\n";
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
async fn edit_refuses_a_file_it_cannot_read_and_says_to_use_write() {
    let (_d, c) = ctx();
    let err = tools::edit::Edit
        .execute(json!({ "patch": "[new.rs]\n-x\n" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("use write to create one"), "{err}");
}

#[tokio::test]
async fn edit_cannot_reach_outside_the_workspace() {
    let (_d, c) = ctx();
    let r = tools::edit::Edit
        .execute(json!({ "patch": "[../escape.rs]\n-x\n" }), &c)
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
    std::fs::create_dir(c.workspace.root().join("d")).unwrap();
    std::fs::write(c.workspace.root().join("d/a.rs"), "one\n").unwrap();
    let out = tools::read::Read
        .execute(json!({ "path": "d" }), &c)
        .await
        .unwrap();
    assert_eq!(out.preview(), "d/");
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
    assert_eq!(out.preview(), "a.rs:2-3");
}

// The view header names the file and nothing else: the staleness note and
// the read-before-edit rule took over the tag's old jobs.
#[tokio::test]
async fn a_view_carries_no_tag_but_still_feeds_the_staleness_note() {
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    let long: String = (0..200)
        .map(|i| format!("fn f{i}() {{\n    {i};\n}}\n"))
        .collect();
    std::fs::write(c.workspace.root().join("big.rs"), &long).unwrap();

    for args in [
        json!({ "path": "a.rs" }),
        json!({ "path": "a.rs", "offset": 2, "limit": 1 }),
        json!({ "path": "big.rs" }),
    ] {
        let out = tools::read::Read.execute(args.clone(), &c).await.unwrap();
        assert!(!out.preview().contains('#'), "{args}: {}", out.preview());
        let head = out.flatten().lines().next().unwrap().to_string();
        assert!(!head.contains('#'), "{args}: {head}");
    }

    // The view was still recorded: an edit after the file changed carries the
    // staleness note beside its report.
    std::fs::write(c.workspace.root().join("a.rs"), "changed\n").unwrap();
    let out = tools::edit::Edit
        .execute(json!({ "patch": "[a.rs]\n=changed\n+changed twice\n" }), &c)
        .await
        .unwrap()
        .flatten();
    assert!(out.contains("file changed since your last view"), "{out}");
}

#[tokio::test]
async fn two_edits_to_one_file_in_the_same_turn_do_not_clobber_each_other() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    // Both anchors are content, and the per-file lock only orders them: each
    // operation applies where its own anchor matches.
    let first = tools::edit::Edit.execute(json!({ "patch": "[a.rs]\n=one\n+ONE\n" }), &c);
    let second = tools::edit::Edit.execute(json!({ "patch": "[a.rs]\n=three\n+THREE\n" }), &c);
    let (a, b) = tokio::join!(first, second);
    assert!(a.is_ok() && b.is_ok(), "{a:?} / {b:?}");

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("ONE"), "{after:?}");
    assert!(after.contains("THREE"), "{after:?}");
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
    let locator = common::locator_in(&out);
    let whole = std::fs::read_to_string(c.spill_path(locator).unwrap()).unwrap();
    assert!(
        whole.contains("MIDDLE_MARKER"),
        "the spill must hold what the result dropped"
    );
    let _ = std::fs::remove_file(c.spill_path(locator).unwrap());
}

#[tokio::test]
async fn read_spills_an_over_long_view_and_reads_it_back_by_locator() {
    let (_dir, c) = common::spilling();
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
    // Cut between rows, not at a byte offset: every line still carries the
    // whole of the line it is numbered with.
    assert!(out.contains("\n…\n"), "{out}");
    for row in out.lines().filter(|l| l.starts_with(char::is_numeric)) {
        let (n, text) = row.split_once(':').expect("every row is addressed");
        assert_eq!(text, format!("line {n}"), "half a row survived: {row}");
    }
    let locator = common::locator_in(&out);

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
    let (dir, c) = common::spilling();
    // A file where the session directory would go: create_dir_all cannot.
    std::fs::write(dir.path().join("spill"), "in the way").unwrap();
    let err = tools::bash::Bash
        .execute(json!({ "command": "printf 'z%.0s' $(seq 1 40000)" }), &c)
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
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    // A patch whose body already matches. It "succeeds", and the model has no
    // way to tell its fix did not land.
    let patch = "[a.rs]\n-one\n+one\n";
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
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    let out = run(
        &tools::edit::Edit,
        json!({ "patch": "[a.rs]\n-two\n-three\n" }),
        &c,
    )
    .await;

    // "no lines added" is true of a pure delete and reads as nothing
    // happening.
    assert!(out.contains("removed 2 lines"), "{out}");
    assert!(!out.contains("no lines added"), "{out}");
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "one\nfour\n"
    );
}

#[tokio::test]
async fn every_marker_the_description_names_is_one_the_parser_takes() {
    // The FORMAT string is prose sent to the model on every request, and the
    // parser is what reads back what the model writes from it. A marker the
    // description names but the parser rejects is exactly what a grammar
    // change costs, so each one is exercised here.
    let described = tools::edit::Edit.description();
    for marker in ["`-`", "`=`", "`+`", "`*`", "`@`"] {
        assert!(described.contains(marker), "the description omits {marker}");
    }
    for body in [
        "-a\n+b",
        "-a",
        "=a\n+b",
        "+b\n=a",
        "*a\n+b",
        "+b\n*a",
        "@a\n+b",
        "@a\n-b",
        "@a\n-",
        "-a\n=b\n+c",
    ] {
        let patch = format!("[a.txt]\n{body}\n");
        hashline::parse(&patch).expect("a described form the parser rejects");
    }
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

// `undecorate` strips a pasted-back header so a model that echoes read's output
// into `write` does not save it as content.
#[tokio::test]
async fn write_still_recognises_the_header_hashline_prints() {
    let src = "one\ntwo\n";
    let pasted = format!("{}\n{src}", hashline::header("a.rs"));
    assert_eq!(tools::write::undecorate(&pasted), src);

    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    tools::write::Write
        .execute(json!({ "path": "a.rs", "content": pasted }), &c)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), src);
}

#[tokio::test]
async fn an_edit_without_patch_says_what_the_one_argument_is() {
    let (_d, c) = ctx();
    let err = tools::edit::Edit
        .execute(json!({}), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing field `patch`"), "{err}");
    assert!(err.contains("takes a single argument"), "{err}");
    assert!(err.contains("[path]"), "{err}");
}

// What a run wrote is taken as it writes, never from anything it says
// afterwards — the account of a subagent's work is the one part of its result
// nothing else checks.
#[tokio::test]
async fn what_a_run_wrote_is_recorded_as_it_writes() {
    let (_d, c) = ctx();
    let src = "pub fn f() {\n    1\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    assert!(c.writes().is_empty(), "nothing written yet");

    run(
        &tools::write::Write,
        json!({ "path": "b.rs", "content": "pub fn g() {}\n" }),
        &c,
    )
    .await;
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;
    run(
        &tools::edit::Edit,
        json!({ "patch": "[a.rs]\n-    1\n+    2\n" }),
        &c,
    )
    .await;
    // A read changes nothing, so it leaves no mark.
    run(&tools::read::Read, json!({ "path": "a.rs" }), &c).await;

    let wrote: Vec<String> = c.writes().iter().map(|p| c.workspace.display(p)).collect();
    assert_eq!(
        wrote,
        ["a.rs", "b.rs"],
        "both writers, sorted, the read left out"
    );
}

#[tokio::test]
async fn a_run_with_its_own_record_writes_nothing_into_its_parents() {
    let (_d, parent) = ctx();
    let child = parent.clone().with_own_writes();

    run(
        &tools::write::Write,
        json!({ "path": "c.rs", "content": "x\n" }),
        &child,
    )
    .await;
    run(
        &tools::write::Write,
        json!({ "path": "p.rs", "content": "y\n" }),
        &parent,
    )
    .await;

    let named =
        |c: &Ctx| -> Vec<String> { c.writes().iter().map(|p| c.workspace.display(p)).collect() };
    // The whole point of the split: asking what the child wrote is asking about
    // the child, and a shared record answers with both and names neither.
    assert_eq!(named(&child), ["c.rs"]);
    assert_eq!(named(&parent), ["p.rs"]);

    // A plain clone shares it, which is what makes the opt-in necessary.
    let sharing = parent.clone();
    run(
        &tools::write::Write,
        json!({ "path": "s.rs", "content": "z\n" }),
        &sharing,
    )
    .await;
    assert_eq!(
        named(&parent),
        ["p.rs", "s.rs"],
        "a clone writes into the record it came with"
    );
}

#[tokio::test]
async fn a_command_that_printed_nothing_still_reports_how_it_ended() {
    let (_d, c) = ctx();
    let quiet = tools::bash::Bash
        .execute(json!({ "command": "grep nothing /dev/null | head" }), &c)
        .await
        .unwrap();
    assert_eq!(quiet.flatten(), "exit 0, no output");
    assert!(quiet.useless, "nothing for a later turn to read");
    // The row that matters most: a command that printed nothing is the one
    // whose row is read to find out what was asked.
    assert_eq!(quiet.preview(), "grep nothing /dev/null | head");

    let failed = tools::bash::Bash
        .execute(json!({ "command": "false" }), &c)
        .await
        .unwrap();
    assert_eq!(
        failed.flatten().trim(),
        "exit 1",
        "a silent failure is not a silent success"
    );
    assert!(!failed.useless, "which is exactly what a later turn needs");
}
