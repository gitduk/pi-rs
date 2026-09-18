mod common;

use common::{ctx, run, view};
use serde_json::json;
use tools::{Ctx, Registry, Tier, Tool, ToolError, Workspace};

// Failure messages quote a slice of the output; byte slicing would panic
// mid-UTF-8 and hide the real failure, so take characters instead.
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[tokio::test]
async fn read_heads_the_view_with_the_file_path() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\ntwo\nthree\n").unwrap();

    let out = view(&c, "a.rs").await;
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
    let out = view(&c, "sub").await;
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

    let back = view(&c, "a/b/c.rs").await;
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
    assert!(out.contains("bytes omitted"), "{}", head(&out, 80));
    assert!(out.len() < 40_000, "clamped output was {} bytes", out.len());
}

// Read a file the way the model would, then replace `old` with `new` in it.
// Read-before-edit is tracked by the session, so nothing is passed for it.
async fn read_then_edit(c: &Ctx, path: &str, old: &str, new: &str) -> Result<String, ToolError> {
    view(c, path).await;
    tools::edit::Edit
        .execute(
            json!({ "path": path, "edits": [{ "old_string": old, "new_string": new }] }),
            c,
        )
        .await
        .map(|o| o.flatten())
}

#[tokio::test]
async fn edit_replaces_the_text_its_anchor_names() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();

    let report = read_then_edit(&c, "a.rs", "two\n", "TWO\n").await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\nTWO\nthree\n");
    // The report echoes the rows it landed, numbered in the new file.
    assert!(report.starts_with("[a.rs]"), "{report}");
    assert!(report.contains("2:TWO"), "{report}");
}

#[tokio::test]
async fn an_anchor_written_with_lf_lands_in_a_crlf_file() {
    // The model writes `\n`; the file spells its breaks `\r\n`. The anchor is
    // retried in the file's own spelling rather than normalizing the file.
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\r\ntwo\r\nthree\r\n").unwrap();

    let report = read_then_edit(&c, "a.rs", "two\n", "TWO\n").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "one\r\nTWO\r\nthree\r\n"
    );
    assert!(report.contains("2:TWO"), "{report}");
}

#[tokio::test]
async fn edit_writes_new_rows_with_the_files_line_ending() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\r\ntwo\r\nthree\r\n").unwrap();
    view(&c, "a.rs").await;
    let out = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{ "old_string": "two\r\n", "new_string": "TWO\r\n" }] }),
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
async fn a_replacement_that_breaks_the_parse_is_refused_rather_than_applied() {
    // The anchor matches and the call is well formed; only the parse gate can
    // tell that the file as edited would not compile.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, THREE_FNS).unwrap();

    let err = read_then_edit(&c, "a.rs", "    2\n}\n", "}\n}\n")
        .await
        .unwrap_err();
    let said = err.to_string();
    // The row's own text: a bare line number invites a story about the parser.
    assert!(said.contains("of what this one produces is `}`"), "{said}");
    assert!(said.contains("Nothing was written"), "{said}");

    // Refused means refused: the file on disk is untouched.
    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);
}

#[tokio::test]
async fn a_multi_row_anchor_lands_as_written() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, THREE_FNS).unwrap();

    let out = read_then_edit(&c, "a.rs", "    2\n}\n", "    99\n}\n")
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("    99\n}\n")
    );
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

    let out = read_then_edit(&c, "a.rs", "    1\n", "    2\n")
        .await
        .unwrap();
    assert!(out.contains("2"), "{out}");
}

#[tokio::test]
async fn a_language_the_parser_does_not_know_is_not_gated() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.toml"), "[a]\nb = 1\n").unwrap();

    let out = read_then_edit(&c, "a.toml", "b = 1\n", "b = 2\n")
        .await
        .unwrap();
    assert!(out.contains("b = 2"), "{out}");
}

#[tokio::test]
async fn a_no_op_replacement_is_refused() {
    // Nothing moves, so reporting a successful edit would teach the model
    // that a fix landed. The file is left alone and the refusal says why.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\n").unwrap();
    view(&c, "a.rs").await;

    let err = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{ "old_string": "one\n", "new_string": "one\n" }] }),
            &c,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("nothing changed"), "{err}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n");
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
    view(&c, "a.rs").await;

    tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{ "insert_after": "one\n", "new_string": "one!\n" }] }),
            &c,
        )
        .await
        .unwrap();
    tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{ "insert_after": "three\n", "new_string": "three!\n" }] }),
            &c,
        )
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
    view(&c, "a.rs").await;
    // The file changed after the view: the anchor the model copied is no
    // longer there, and nothing may be written.
    std::fs::write(&path, "something else\n").unwrap();

    let err = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [{ "old_string": "two\n", "new_string": "" }] }),
            &c,
        )
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
async fn an_edit_that_misses_leaves_its_sibling_unapplied() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "a\nb\n").unwrap();
    view(&c, "a.rs").await;

    // The second edit's anchor matches nothing: the whole call refuses.
    let out = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [
                { "insert_after": "a\n", "new_string": "A\n" },
                { "old_string": "zzz\n", "new_string": "B\n" },
            ]}),
            &c,
        )
        .await;
    assert!(out.is_err(), "a missed anchor must refuse the call");
    // The first edit was sound, but a half-applied call is worse than a
    // rejected one.
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "a\nb\n"
    );
}

#[tokio::test]
async fn edit_refuses_a_file_it_cannot_read_and_says_to_use_write() {
    let (_d, c) = ctx();
    let err = tools::edit::Edit
        .execute(
            json!({ "path": "new.rs", "edits": [{ "old_string": "x", "new_string": "y" }] }),
            &c,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("use write to create one"), "{err}");
}

#[tokio::test]
async fn edit_cannot_reach_outside_the_workspace() {
    let (_d, c) = ctx();
    let r = tools::edit::Edit
        .execute(
            json!({ "path": "../escape.rs", "edits": [{ "old_string": "x", "new_string": "y" }] }),
            &c,
        )
        .await;
    assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
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
        .execute(
            json!({ "path": "a.rs", "edits": [
                { "insert_after": "changed\n", "new_string": "changed twice\n" }
            ]}),
            &c,
        )
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
    view(&c, "a.rs").await;

    // Both anchors are content, and the per-file lock only orders them: each
    // edit applies where its own anchor matches.
    let first = tools::edit::Edit.execute(
        json!({ "path": "a.rs", "edits": [{ "insert_after": "one\n", "new_string": "ONE\n" }] }),
        &c,
    );
    let second = tools::edit::Edit.execute(
        json!({ "path": "a.rs", "edits": [{ "insert_after": "three\n", "new_string": "THREE\n" }] }),
        &c,
    );
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
    let shown = view(&c, "a.rs").await;

    // The whole read output, pasted straight back — a habit the numbered format
    // invites and one nothing else catches.
    tools::write::Write
        .execute(json!({ "path": "b.rs", "content": shown }), &c)
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

    assert!(out.contains("bytes omitted"), "{}", head(&out, 90));
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
async fn a_deletion_is_reported_by_what_it_deleted() {
    let (_d, c) = ctx();
    let before = "one\ntwo\nthree\nfour\n";
    std::fs::write(c.workspace.root().join("a.rs"), before).unwrap();
    view(&c, "a.rs").await;

    let out = run(
        &tools::edit::Edit,
        json!({ "path": "a.rs", "edits": [{ "old_string": "two\nthree\n", "new_string": "" }] }),
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
async fn a_call_that_adds_and_removes_reports_both() {
    // The rows a deletion took have no row in the new file to be named by,
    // so a call that also added something still has to say they went.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "a\nb\ntail\n").unwrap();
    view(&c, "a.rs").await;

    let out = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [
                { "old_string": "a\nb\n", "new_string": "" },
                { "insert_after": "tail\n", "new_string": "z\n" },
            ]}),
            &c,
        )
        .await
        .unwrap()
        .flatten();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "tail\nz\n");
    assert!(out.contains("removed 2 lines"), "{out}");
    assert!(out.contains("   1 - a"), "{out}");
}

#[tokio::test]
async fn every_field_the_description_names_is_one_the_schema_takes() {
    // The description is prose sent on every request and the schema is what
    // the call is parsed against. A name the prose teaches but the schema
    // refuses is exactly what a change to either one costs, so both are read
    // here against the same list.
    let described = tools::edit::Edit.description();
    let schema = tools::edit::Edit.schema();
    let fields = schema["properties"]["edits"]["items"]["properties"]
        .as_object()
        .expect("the schema names the entry's fields")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        fields,
        vec![
            "insert_after",
            "insert_before",
            "new_string",
            "old_string",
            "replace_all",
            "whole_block",
        ]
    );
    for field in &fields {
        assert!(
            described.contains(field.as_str()),
            "the description omits `{field}`"
        );
    }
    // The shape the entries are given in, named in both places.
    for name in ["`path`", "`edits`"] {
        assert!(described.contains(name), "the description omits {name}");
        assert!(
            schema["properties"].get(name.trim_matches('`')).is_some(),
            "the schema omits {name}"
        );
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
async fn an_edit_without_its_arguments_says_what_they_are() {
    let (_d, c) = ctx();
    let err = tools::edit::Edit
        .execute(json!({}), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing field `path`"), "{err}");
    assert!(err.contains("takes `path` and `edits`"), "{err}");

    let err = tools::edit::Edit
        .execute(json!({ "path": "a.rs" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("missing field `edits`"), "{err}");
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
    view(&c, "a.rs").await;
    run(
        &tools::edit::Edit,
        json!({ "path": "a.rs", "edits": [{ "old_string": "    1\n", "new_string": "    2\n" }] }),
        &c,
    )
    .await;
    // A read changes nothing, so it leaves no mark.
    view(&c, "a.rs").await;

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

#[tokio::test]
async fn a_command_rtk_knows_runs_through_rtk() {
    let (_d, c) = ctx();
    // Asked of rtk through the same call pi makes, from the same directory, so
    // what to expect is rtk's answer rather than a literal here. No rtk on this
    // machine, or one told to stand down, leaves nothing to check and this
    // returns.
    let Some(rewritten) = tools::rtk::rewrite("git status", c.workspace.root()).await else {
        return;
    };
    let out = tools::bash::Bash
        .execute(json!({ "command": "git status" }), &c)
        .await
        .unwrap();
    // The workspace is not a repository, so the rewritten command fails; what
    // this checks is the row naming the command that ran.
    assert_eq!(out.preview(), rewritten);
}

#[tokio::test]
async fn an_anchor_wrapped_in_a_string_or_a_lone_object_is_still_taken() {
    // The shapes models send: `edits` stringified, and a lone entry where a
    // list of one belongs. Both are accepted rather than costing a turn.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    view(&c, "a.rs").await;

    let edits = serde_json::to_string(&json!([{ "old_string": "one\n", "new_string": "ONE\n" }]))
        .expect("json");
    tools::edit::Edit
        .execute(json!({ "path": "a.rs", "edits": edits }), &c)
        .await
        .expect("a stringified list is taken");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\ntwo\n");

    tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": { "old_string": "two\n", "new_string": "TWO\n" } }),
            &c,
        )
        .await
        .expect("a lone entry is taken");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ONE\nTWO\n");
}

#[tokio::test]
async fn a_byte_order_mark_is_stripped_from_the_view_and_kept_on_disk() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "\u{FEFF}fn f() {}\n").unwrap();

    let out = view(&c, "a.rs").await;
    assert!(out.contains("1:fn f() {}"), "{out}");
    assert!(
        !out.contains('\u{FEFF}'),
        "the mark is not part of the line"
    );

    // So an anchor copied from the view matches, and the mark survives.
    read_then_edit(&c, "a.rs", "fn f() {}", "fn g() {}")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "\u{FEFF}fn g() {}\n"
    );
}

#[tokio::test]
async fn emptying_a_line_without_its_break_is_said_out_loud() {
    // `new_string: ""` on an anchor that stops at the end of a line leaves a
    // blank row. The edit still lands, and the report says what it left.
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    view(&c, "a.rs").await;

    let out = read_then_edit(&c, "a.rs", "two", "").await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\n\nthree\n");
    assert!(out.contains("left the line break"), "{out}");
    assert!(out.contains("edits[0]"), "{out}");
}

// One call, both edits matched against the file as it was: the second's
// anchor survives even where the first replaced the text it names.
#[tokio::test]
async fn two_edits_in_one_call_match_against_the_file_as_it_was() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    view(&c, "a.rs").await;

    let out = tools::edit::Edit
        .execute(
            json!({ "path": "a.rs", "edits": [
                { "old_string": "two\n", "new_string": "TWO\n" },
                { "insert_after": "two\n", "new_string": "after two\n" },
            ]}),
            &c,
        )
        .await
        .unwrap()
        .flatten();

    assert!(out.contains("2:TWO"), "{out}");
    assert!(out.contains("3:after two"), "{out}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "one\nTWO\nafter two\nthree\n"
    );
}
