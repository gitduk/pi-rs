use serde_json::json;
use tools::{Ctx, Tool, Workspace};

fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

async fn show(c: &Ctx, path: &str) -> String {
    tools::read::Read
        .execute(json!({"path": path}), c)
        .await
        .unwrap()
        .flatten()
}

async fn edit(c: &Ctx, patch: String) -> Result<String, String> {
    tools::edit::Edit
        .execute(json!({"patch": patch}), c)
        .await
        .map(|o| o.flatten())
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn an_unbalanced_replacement_says_where_the_imbalance_sits() {
    let (_d, c) = ctx();
    let src = "fn f() -> bool {\n    let enabled = count();\n    if enabled {\n        true\n    } else {\n        false\n    }\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    // The replacement deletes the rows that close the fn: the produced file
    // is unterminated, which the parser always flags - unlike a stray
    // top-level `}`, which its error recovery absorbs.
    run_read(&c, "a.rs").await;
    let err = edit(
        &c,
        "[a.rs]\n-    let enabled = count();\n-    if enabled {\n-        true\n-    } else {\n-        false\n-    }\n-}\n".into(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("would not parse"), "{err}");
    assert!(err.contains("Brace balance"), "{err}");
    assert!(err.contains("Nothing was written"), "{err}");
}

// From ~/.pi/logs/1788141625-3348974: every failing range ended one line off a
// `match`, a struct literal or a wrapped call — none of them declarations, so
// the read view said nothing about where they close.
#[tokio::test]
async fn the_read_view_says_where_a_non_declaration_closes() {
    let (_d, c) = ctx();
    let src = "\
pub fn pick(word: &str) -> Vec<u8> {
    match word {
        \"a\" => one()
            .two(),
        _ => Vec::new(),
    }
}
";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    let out = show(&c, "a.rs").await;
    assert!(out.contains("\n2-6:    match word {"), "{out}");
    assert!(out.contains("\n3-4:"), "{out}");
}

// From ~/.pi/logs/1788141625-3348974 turn 20: a 43-line body that carried the
// struct it was replacing twice. The echo said only how many rows arrived, so
// nothing showed that the body had doubled a block — and the file still parsed.
//
// The budget behind that cut-off is bytes, not rows: forty rows of `}` and
// forty rows of a wrapped call are the same row count and an order of magnitude
// apart, and what a transcript pays for is the bytes.
#[tokio::test]
async fn the_echo_budget_is_spent_in_bytes_not_rows() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    let src = format!("fn f() {{\n{}}}\n", "    let x = 1;\n".repeat(60));
    std::fs::write(&path, &src).unwrap();
    run_read(&c, "a.rs").await;

    // Fifty narrow rows replacing sixty: over the row count that used to
    // elide, nowhere near the bytes that do.
    let body: String = (1..=50)
        .map(|i| format!("+    let y{i} = {i};\n"))
        .collect();
    let patch = format!(
        "[a.rs]\n=fn f() {{\n{}=}}\n{body}",
        "-    let x = 1;\n".repeat(60)
    );
    let out = edit(&c, patch).await.unwrap();
    assert!(
        !out.contains("… "),
        "narrow rows are cheap; echo them:\n{out}"
    );
    assert!(out.contains("52:    let y50 = 50;"), "{out}");

    // Forty wide ones: fewer rows than above, three times the bytes. A second
    // file, because the first one's content has moved and the anchor is right
    // to say so.
    std::fs::write(c.workspace.root().join("b.rs"), &src).unwrap();
    run_read(&c, "b.rs").await;
    let wide: String = (1..=40)
        .map(|i| format!("+    let y{i} = compute(&state, {i}, \"a rather long argument\");\n"))
        .collect();
    let patch = format!(
        "[b.rs]\n=fn f() {{\n{}=}}\n{wide}",
        "-    let x = 1;\n".repeat(60)
    );
    let out = edit(&c, patch).await.unwrap();
    assert!(
        out.contains("3:    let y1 = compute"),
        "head of the hunk:\n{out}"
    );
    assert!(
        out.contains("42:    let y40 = compute"),
        "tail of the hunk:\n{out}"
    );
    assert!(
        out.contains("… 36 lines"),
        "and what it stood in for:\n{out}"
    );
}

async fn run_read(c: &Ctx, path: &str) {
    tools::read::Read
        .execute(json!({"path": path}), c)
        .await
        .unwrap();
}

// From ~/.pi/logs/1788141625-3348974 turn 103: the break was reported at line 1
// (`//! Provider list panel.`) while the hunk sat 170 rows down.
#[tokio::test]
async fn the_break_reported_is_the_one_near_the_hunk() {
    let (_d, c) = ctx();
    let filler: String = (0..40)
        .map(|i| format!("fn f{i}() {{\n    {i};\n}}\n"))
        .collect();
    let src = format!("//! Header.\n{filler}");
    std::fs::write(c.workspace.root().join("a.rs"), &src).unwrap();
    run_read(&c, "a.rs").await;
    // Replace `fn f30()`'s body and drop the opening brace's partner.
    let err = edit(
        &c,
        "[a.rs]\n=fn f30() {\n-    30\n+    30;\n+extra();\n".into(),
    )
    .await
    .unwrap_err();
    assert!(
        !err.contains("//! Header."),
        "must not point at the file head:\n{err}"
    );
}

// The general form of a refusal that teaches: a keep row that matches nothing
// is named as the thing to widen.
#[tokio::test]
async fn a_context_that_matches_nothing_is_told_to_widen() {
    let (_d, c) = ctx();
    let src = "fn f() {\n    a();\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    run_read(&c, "a.rs").await;
    let err = edit(&c, "[a.rs]\n=a();\n=nothing\n-a();\n".into())
        .await
        .unwrap_err();
    assert!(err.contains("Widen the `=` context"), "{err}");
}

// The `*` row names the opening line; the extent the parser returns includes
// the doc comment and attribute above it, so a body carrying its own does not
// write them twice — and the result still parses.
#[tokio::test]
async fn a_star_named_below_the_annotations_still_replaces_them() {
    let (_d, c) = ctx();
    let src = "use std::fmt;\n\n/// Old doc.\n#[inline]\npub fn foo() -> u8 {\n    1\n}\n";
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, src).unwrap();

    run_read(&c, "a.rs").await;
    let _out = edit(
        &c,
        "[a.rs]\n@/// Old doc.\n-/// Old doc.\n-#[inline]\n-pub fn foo() -> u8 {\n-    1\n-}\n+/// New doc.\n+#[inline]\n+pub fn foo() -> u8 {\n+    2\n+}\n".into(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "use std::fmt;\n\n/// New doc.\n#[inline]\npub fn foo() -> u8 {\n    2\n}\n"
    );
}

// The read view names the extent on the row it starts, so the number it prints
// and the row the `@` scope matches are the same one.
#[tokio::test]
async fn the_view_and_the_scope_name_the_same_rows() {
    let (_d, c) = ctx();
    let src = "/// Doc.\n#[inline]\npub fn foo() {}\n";
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, src).unwrap();
    assert!(show(&c, "a.rs").await.contains("\n1-3:/// Doc."), "{src}");
    edit(
        &c,
        "[a.rs]\n@/// Doc.\n-/// Doc.\n-#[inline]\n-pub fn foo() {}\n".into(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
}

#[tokio::test]
async fn the_display_names_exactly_the_rows_the_body_holds() {
    let (_d, c) = ctx();
    let spill = tempfile::tempdir().unwrap();
    let c = c.with_spill_root(spill.path());
    let src: String = (1..=5000)
        .map(|i| format!("line {i} of a file with ordinary rows\n"))
        .collect();
    std::fs::write(c.workspace.root().join("big.txt"), &src).unwrap();
    std::fs::write(c.workspace.root().join("a.rs"), "one\ntwo\nthree\n").unwrap();

    for args in [
        json!({ "path": "big.txt" }),
        json!({ "path": "big.txt", "offset": 100, "limit": 4000 }),
        json!({ "path": "a.rs" }),
        json!({ "path": "a.rs", "offset": 2, "limit": 1 }),
    ] {
        let out = tools::read::Read.execute(args.clone(), &c).await.unwrap();
        let shown = out.preview();
        let body = out.flatten();
        // Every row the body carries, by the number it carries.
        let held: Vec<usize> = body
            .lines()
            .filter_map(|l| l.split_once(':').and_then(|(n, _)| n.parse().ok()))
            .collect();
        let named = shown
            .split_once(':')
            .map_or(String::new(), |(_, r)| r.trim_end_matches(']').to_string());
        if named.is_empty() {
            // No window named means the file arrived whole and uncut.
            assert_eq!(held.len(), src_lines(&c, &args), "{args}: {shown}");
            continue;
        }
        for span in named.split('…') {
            let (a, b) = match span.split_once('-') {
                Some((a, b)) => (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()),
                None => (span.parse().unwrap(), span.parse().unwrap()),
            };
            for n in a..=b {
                assert!(held.contains(&n), "{args}: {shown} names {n}, body has not");
            }
        }
        // And nothing outside what it named.
        let widest: Vec<usize> = named
            .split('…')
            .flat_map(|s| {
                let (a, b) = s.split_once('-').unwrap_or((s, s));
                a.parse::<usize>().unwrap()..=b.parse::<usize>().unwrap()
            })
            .collect();
        for n in &held {
            assert!(
                widest.contains(n),
                "{args}: body has {n}, {shown} does not name it"
            );
        }
    }
}

fn src_lines(c: &Ctx, args: &serde_json::Value) -> usize {
    let path = args["path"].as_str().unwrap();
    std::fs::read_to_string(c.workspace.root().join(path))
        .unwrap()
        .lines()
        .count()
}

// A row elided from the transcript is only recoverable through the spill file,
// so "something was elided" and "the whole of it was kept on disk" have to be
// one decision. They were two — a transcript budget and a spill threshold, each
// read off a differently assembled string — and between the two thresholds sat
// three bytes where rows vanished behind `…` with no locator to fetch them.
//
// Three bytes is why the sweep is wide and walks one byte at a time: aiming at
// the window means re-deriving the arithmetic that got it wrong, and any change
// to the header, the gap mark or the row format moves it.
#[tokio::test]
async fn nothing_is_elided_without_somewhere_to_recover_it_from() {
    let (_d, c) = ctx();
    let spill = tempfile::tempdir().unwrap();
    let c = c.with_spill_root(spill.path());
    let path = c.workspace.root().join("a.txt");

    // Rows of a fixed width, then one whose width walks the whole boundary.
    let bulk: String = (1..=880)
        .map(|i| format!("{i:04} xxxxxxxxxxxxxxxxxxxxxxxx\n"))
        .collect();
    let (mut elided, mut whole) = (false, false);
    for pad in 0..900 {
        std::fs::write(&path, format!("{bulk}{}\n", "y".repeat(pad))).unwrap();
        let out = tools::read::Read
            .execute(json!({ "path": "a.txt", "limit": 2000 }), &c)
            .await
            .unwrap()
            .flatten();
        if out.contains("\n…\n") {
            elided = true;
            assert!(
                out.contains("full output: spill:"),
                "pad {pad}: rows elided with no locator to recover them"
            );
        } else {
            whole = true;
        }
    }
    assert!(
        elided && whole,
        "the sweep never crossed the budget; widen it"
    );
}

// A refusal is only worth a turn if it stays inside the view budget every
// other refusal obeys, and names the closest real row so the retry costs no
// read.
#[tokio::test]
async fn the_no_match_refusal_is_budgeted_like_every_other_view() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.txt");
    let src: String = (1..=400)
        .map(|i| format!("row {i} of the file\n"))
        .collect();
    std::fs::write(&path, &src).unwrap();
    run_read(&c, "a.txt").await;

    let err = edit(
        &c,
        "[a.txt]\n=row 400 of the file\n-row 401 of the file\n-x\n".into(),
    )
    .await
    .unwrap_err();
    assert!(err.contains("no match"), "{err}");
    assert!(err.contains("closest real line"), "{err}");
    assert!(err.len() < 4_000, "unbudgeted, {} bytes:\n{err}", err.len());
}

// A `*` scope whose operation covers the construct says so: the echo names it,
// so opening the wrong one is visible instead of a clean apply.
#[tokio::test]
async fn the_echo_names_the_construct_a_scope_covered() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    let src = "fn first() {\n    1\n}\n\nfn second() {\n    2\n}\n";
    std::fs::write(&path, src).unwrap();

    run_read(&c, "a.rs").await;
    let out = edit(
        &c,
        "[a.rs]\n@fn second()\n=fn second() {\n-    2\n-}\n+    22\n+}\n".into(),
    )
    .await
    .unwrap();

    assert!(out.contains("covered the construct at lines 5-7"), "{out}");
    assert!(out.contains("lines 5-7: `fn second() {`"), "{out}");
}

// The same visibility for a deletion: an operation that takes a whole
// construct names it, since a wrong-target delete is as silent as a
// wrong-target rewrite.
#[tokio::test]
async fn the_echo_names_the_construct_a_pure_delete_covered() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.rs");
    let src = "fn first() {\n    1\n}\n\nfn second() {\n    2\n}\n";
    std::fs::write(&path, src).unwrap();
    run_read(&c, "a.rs").await;

    let out = edit(
        &c,
        "[a.rs]\n@fn second()\n-fn second() {\n-    2\n-}\n".into(),
    )
    .await
    .unwrap();

    assert!(out.contains("covered the construct at lines 5-7"), "{out}");
    assert!(out.contains("removed 3 lines"), "{out}");
}
