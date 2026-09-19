mod common;

use common::{ctx, run, view};
use serde_json::json;
use tools::{Ctx, Registry, Tier, Tool, ToolError};

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

const THREE_FNS: &str = "\
pub fn keep() -> i32 {
    1
}

pub fn target() -> i32 {
    2
}
";

// The workspace is the write boundary: read may leave it, and no tool that
// acts — write, bash, edit — may follow a path out.
#[tokio::test]
async fn read_leaves_the_workspace_but_write_bash_and_edit_do_not() {
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

    let e = tools::edit::Edit
        .execute(
            json!({ "path": o.join("z.rs").to_str().unwrap(), "edits": [{ "old_string": "x", "new_string": "y" }] }),
            &c,
        )
        .await;
    assert!(matches!(e, Err(ToolError::Escape(_))), "{e:?}");
}

// `useless` marks a result that carries nothing for a later turn — the flag
// compaction reads, not the prose the model reads. A silent success is
// nothing; a silent failure is exactly what a later turn needs.
#[tokio::test]
async fn results_that_carry_nothing_are_marked_useless() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.txt"), "one\n").unwrap();

    let past = tools::read::Read
        .execute(json!({ "path": "a.txt", "offset": 99 }), &c)
        .await
        .unwrap();
    assert!(past.useless);

    let silent = tools::bash::Bash
        .execute(json!({ "command": "true" }), &c)
        .await
        .unwrap();
    assert!(silent.useless);

    let quiet = tools::bash::Bash
        .execute(json!({ "command": "grep nothing /dev/null | head" }), &c)
        .await
        .unwrap();
    assert!(quiet.useless);

    let failed = tools::bash::Bash
        .execute(json!({ "command": "false" }), &c)
        .await
        .unwrap();
    assert!(!failed.useless, "a silent failure is not a silent success");
}

// The window arithmetic saturates: a limit past the end of the file reads to
// the end rather than wrapping into a panic.
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

// The tier is what each tool may reach — the gate the loop checks before a
// call is made.
#[test]
fn registry_tiers_name_what_each_tool_may_reach() {
    let r = Registry::builtin();
    assert_eq!(r.get("edit").unwrap().tier(), Tier::Write);
    assert_eq!(r.get("bash").unwrap().tier(), Tier::Exec);
    assert_eq!(r.get("read").unwrap().tier(), Tier::Read);
    assert_eq!(r.get("fetch").unwrap().tier(), Tier::Net);
}

// The size check runs before the read: a huge file is refused without ever
// being read into memory.
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
}

// The output budget is spent in bytes, not characters: a char-counted clamp
// on CJK output would blow past the cap.
#[tokio::test]
async fn multibyte_output_respects_the_byte_budget_and_stays_valid_utf8() {
    let (_d, c) = ctx();
    // 60k CJK chars = 180k bytes.
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "printf '中%.0s' $(seq 1 60000)" }),
        &c,
    )
    .await;
    assert!(out.len() < 40_000, "clamped output was {} bytes", out.len());
}

// The parse gate refuses before anything is written: a replacement that
// breaks the file, an overwrite that runs out mid-file, a replacement that
// moves nothing — each is refused, and the file on disk is untouched.
#[tokio::test]
async fn an_edit_that_would_break_the_file_is_refused_and_writes_nothing() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, THREE_FNS).unwrap();

    // The anchor matches and the call is well formed; only the parse gate can
    // tell that the file as edited would not compile.
    read_then_edit(&c, "a.rs", "    2\n}\n", "}\n}\n")
        .await
        .unwrap_err();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);

    // The whole-file counterpart: content that ran out mid-file, losing the
    // tail of a working file.
    tools::write::Write
        .execute(
            json!({ "path": "a.rs", "content": "pub fn a() -> i32 {\n    1\n" }),
            &c,
        )
        .await
        .unwrap_err();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);

    // Nothing moves, so reporting a successful edit would teach the model
    // that a fix landed.
    read_then_edit(&c, "a.rs", "    2\n}\n", "    2\n}\n")
        .await
        .unwrap_err();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), THREE_FNS);
}

// The gate's edges, where it has no standing to refuse: a file that never
// parsed is usually the reason an edit is happening, a language the parser
// does not know cannot be judged, a new file has nothing behind it to lose,
// and empty content parses in every language the tree knows.
#[tokio::test]
async fn the_parse_gate_leaves_files_it_has_no_standing_to_refuse() {
    let (_d, c) = ctx();

    std::fs::write(
        c.workspace.root().join("broken.rs"),
        "pub fn a() -> i32 {\n    1\n",
    )
    .unwrap();
    read_then_edit(&c, "broken.rs", "    1\n", "    2\n")
        .await
        .unwrap();

    std::fs::write(c.workspace.root().join("a.toml"), "[a]\nb = 1\n").unwrap();
    read_then_edit(&c, "a.toml", "b = 1\n", "b = 2\n")
        .await
        .unwrap();

    std::fs::write(
        c.workspace.root().join("w.rs"),
        "pub fn a() -> i32 {\n    1\n",
    )
    .unwrap();
    run(
        &tools::write::Write,
        json!({ "path": "w.rs", "content": "pub fn a() -> i32 {\n    2\n" }),
        &c,
    )
    .await;

    run(
        &tools::write::Write,
        json!({ "path": "b.toml", "content": "[a\n" }),
        &c,
    )
    .await;

    run(
        &tools::write::Write,
        json!({ "path": "stub.rs", "content": "pub fn a() -> i32 {\n" }),
        &c,
    )
    .await;

    std::fs::write(c.workspace.root().join("e.rs"), THREE_FNS).unwrap();
    run(
        &tools::write::Write,
        json!({ "path": "e.rs", "content": "" }),
        &c,
    )
    .await;
}

// The drift-immunity payoff: two edits back to back with no read between
// them, the second anchored on content the first one put there. Content
// anchors cannot go stale, so this just works — no false staleness note, no
// demand for a reread.
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

// The view was recorded, so the gate can answer for the file moving: an
// anchor that still matches applies, with the staleness note beside its
// report; one whose text is gone refuses, and nothing may be written.
#[tokio::test]
async fn an_edit_answers_for_a_file_that_changed_since_its_view() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "changed\n").unwrap();
    view(&c, "a.rs").await;
    std::fs::write(c.workspace.root().join("a.rs"), "changed\nmore\n").unwrap();

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

    std::fs::write(c.workspace.root().join("b.rs"), "one\ntwo\n").unwrap();
    view(&c, "b.rs").await;
    std::fs::write(c.workspace.root().join("b.rs"), "something else\n").unwrap();
    let refused = tools::edit::Edit
        .execute(
            json!({ "path": "b.rs", "edits": [{ "old_string": "two\n", "new_string": "" }] }),
            &c,
        )
        .await;
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("b.rs")).unwrap(),
        "something else\n",
        "nothing may be written"
    );
}

// The second edit's anchor matches nothing: the whole call refuses, and the
// first edit — sound on its own — is not applied either. A half-applied call
// is worse than a rejected one.
#[tokio::test]
async fn an_edit_that_misses_leaves_its_sibling_unapplied() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "a\nb\n").unwrap();
    view(&c, "a.rs").await;

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
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
}

// Both anchors are content, and the per-file lock only orders them: each edit
// applies where its own anchor matches, and neither clobbers the other.
#[tokio::test]
async fn two_edits_to_one_file_in_the_same_turn_do_not_clobber_each_other() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
    view(&c, "a.rs").await;

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

// Over the cap the view is cut to a locator, and the spill file holds what
// the result dropped — nothing is lost. A short output takes no spill at all:
// no locator in the view, no file left behind.
#[tokio::test]
async fn an_over_long_output_is_kept_somewhere_the_model_can_reach() {
    let (dir, c) = common::spilling();
    let out = run(
        &tools::bash::Bash,
        json!({ "command": "printf 'z%.0s' $(seq 1 40000); echo; echo MIDDLE_MARKER; printf 'z%.0s' $(seq 1 40000)" }),
        &c,
    )
    .await;
    let locator = common::locator_in(&out);
    let whole = std::fs::read_to_string(c.spill_path(locator).unwrap()).unwrap();
    assert!(
        whole.contains("MIDDLE_MARKER"),
        "the spill must hold what the result dropped"
    );
    let _ = std::fs::remove_file(c.spill_path(locator).unwrap());

    let logs = |root: &std::path::Path| -> usize {
        std::fs::read_dir(root)
            .map(|sessions| {
                sessions
                    .filter_map(|s| s.unwrap().path().read_dir().ok())
                    .flat_map(|files| files)
                    .count()
            })
            .unwrap_or(0)
    };
    assert_eq!(logs(&dir.path().join("spill")), 0, "the spill was removed");

    let out = run(&tools::bash::Bash, json!({ "command": "echo hi" }), &c).await;
    assert!(!out.contains("full output:"), "{out}");
    assert_eq!(
        logs(&dir.path().join("spill")),
        0,
        "no file for a short output"
    );
}

// The view's cut lands between rows, not at a byte offset: every row that
// survives carries the whole of the line it is numbered with, so an anchor
// copied from any of them still matches.
#[tokio::test]
async fn read_spills_an_over_long_view_without_splitting_a_row() {
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
    assert!(out.contains("\n…\n"), "{out}");
    for row in out.lines().filter(|l| l.starts_with(char::is_numeric)) {
        let (n, text) = row.split_once(':').expect("every row is addressed");
        assert_eq!(text, format!("line {n}"), "half a row survived: {row}");
    }
    let whole = std::fs::read_to_string(c.spill_path(common::locator_in(&out)).unwrap()).unwrap();
    assert!(whole.contains("line 30000"), "the spill holds what went");
}

// A spill that cannot be written is a loud failure, not a view whose rows
// quietly went missing.
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
}

// No NUL byte, so a NUL sniff calls it text, and a lossy decode would turn
// every accent into U+FFFD: non-UTF-8 prose is refused rather than mangled.
#[tokio::test]
async fn text_that_is_not_utf8_is_refused_rather_than_mangled() {
    let (_d, c) = ctx();
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

// What a run wrote is taken as it writes, never from anything it says
// afterwards — the account of a subagent's work is the one part of its result
// nothing else checks. Reads leave no mark.
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
    view(&c, "a.rs").await;

    let wrote: Vec<String> = c.writes().iter().map(|p| c.workspace.display(p)).collect();
    assert_eq!(
        wrote,
        ["a.rs", "b.rs"],
        "both writers, sorted, the read left out"
    );
}

// The whole point of the split record: asking what the child wrote is asking
// about the child, and a shared record answers with both and names neither.
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

// The mark is stripped from the view so an anchor copied from it matches, and
// it survives on disk: the edit must not quietly destroy it.
#[tokio::test]
async fn a_byte_order_mark_survives_an_edit() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, "\u{FEFF}fn f() {}\n").unwrap();

    read_then_edit(&c, "a.rs", "fn f() {}", "fn g() {}")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "\u{FEFF}fn g() {}\n"
    );
}
