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

// From ~/.pi/logs/1787817702-887066: `PUT 136*:` was suggested by the failure
// message, followed, and failed again — line 136 was a one-line `let`.
#[tokio::test]
async fn a_one_row_construct_is_never_offered_as_a_star() {
    let (_d, c) = ctx();
    let src = "\
fn f() -> bool {
    let enabled = count();
    if enabled {
        true
    } else {
        false
    }
}
";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    // The hunk opens on a one-row `let` and its body drops a brace, which is
    // where the message reaches for `N*` — and where `N*` cannot help.
    let err = edit(
        &c,
        format!(
            "[a.rs#{}]\nPUT 2-6:\n+    let enabled = count();\n+    enabled\n",
            hashline::tag(src)
        ),
    )
    .await
    .unwrap_err();
    assert!(err.contains("the imbalance sits at line 2"), "{err}");
    assert!(!err.contains('*'), "no unfollowable `N*` advice:\n{err}");
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

// From ~/.pi/logs/1787817380-856220 turn 20: a 43-line body that carried the
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

    // Fifty narrow rows: over the row count that used to elide, nowhere near
    // the bytes that do.
    std::fs::write(&path, &src).unwrap();
    let body: String = (1..=50)
        .map(|i| format!("+    let y{i} = {i};\n"))
        .collect();
    let out = edit(
        &c,
        format!("[a.rs#{}]\nPUT 2-61:\n{body}", hashline::tag(&src)),
    )
    .await
    .unwrap();
    assert!(
        !out.contains("… "),
        "narrow rows are cheap; echo them:\n{out}"
    );
    assert!(out.contains("51:    let y50 = 50;"), "{out}");

    // Forty wide ones: fewer rows than above, three times the bytes. A second file, because the
    // first one's numbering has moved and the guard is right to say so.
    std::fs::write(c.workspace.root().join("b.rs"), &src).unwrap();
    let wide: String = (1..=40)
        .map(|i| format!("+    let y{i} = compute(&state, {i}, \"a rather long argument\");\n"))
        .collect();
    let out = edit(
        &c,
        format!("[b.rs#{}]\nPUT 2-61:\n{wide}", hashline::tag(&src)),
    )
    .await
    .unwrap();
    assert!(
        out.contains("2:    let y1 = compute"),
        "head of the hunk:\n{out}"
    );
    assert!(
        out.contains("41:    let y40 = compute"),
        "tail of the hunk:\n{out}"
    );
    assert!(
        out.contains("… 34 lines"),
        "and what it stood in for:\n{out}"
    );
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
    // Replace `fn f30() {` … `30;` and drop the opening brace's partner.
    let at = 2 + 30 * 3;
    let err = edit(
        &c,
        format!(
            "[a.rs#{}]\nPUT {at}-{}:\n+fn f30() {{\n+    30;\n+}}\n+extra();\n",
            hashline::tag(&src),
            at + 1
        ),
    )
    .await
    .unwrap_err();
    assert!(
        !err.contains("//! Header."),
        "must not point at the file head:\n{err}"
    );
}

// From ~/.pi/logs/1788141625-3348974 turn 4→5: the bare `}` was called invalid,
// the model deleted it, and the next patch failed on the brace it had dropped.
#[tokio::test]
async fn a_kept_line_written_as_context_is_told_to_widen() {
    let (_d, c) = ctx();
    let src = "fn f() {\n    a();\n}\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    let err = edit(
        &c,
        format!("[a.rs#{}]\nPUT 2:\n+    b();\n}}\n", hashline::tag(src)),
    )
    .await
    .unwrap_err();
    assert!(err.contains("widen the address"), "{err}");
}

// `PUT N*:` named at the construct's own row left the doc comment and the
// attribute above it in place, so a body carrying its own wrote both twice —
// and the result parsed, so nothing caught it.
#[tokio::test]
async fn a_star_named_below_the_annotations_still_replaces_them() {
    let (_d, c) = ctx();
    let src = "use std::fmt;\n\n/// Old doc.\n#[inline]\npub fn foo() -> u8 {\n    1\n}\n";
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, src).unwrap();
    edit(
        &c,
        format!(
            "[a.rs#{}]\nPUT 5*:\n+/// New doc.\n+#[inline]\n+pub fn foo() -> u8 {{\n+    2\n+}}\n",
            hashline::tag(src)
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "use std::fmt;\n\n/// New doc.\n#[inline]\npub fn foo() -> u8 {\n    2\n}\n"
    );
}

// The read view names the extent on the row it starts, so the number it prints
// and the number `N*` resolves are the same one.
#[tokio::test]
async fn the_view_and_the_star_name_the_same_rows() {
    let (_d, c) = ctx();
    let src = "/// Doc.\n#[inline]\npub fn foo() {}\n";
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, src).unwrap();
    assert!(show(&c, "a.rs").await.contains("\n1-3:/// Doc."), "{src}");
    edit(&c, format!("[a.rs#{}]\nCUT 1*\n", hashline::tag(src)))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
}

// A file written whole leaves the model knowing its numbering — it sent every
// line. A body that still carries read's header does not: cleaning drops that
// row, and everything below it is one off what was sent.
#[tokio::test]
async fn a_write_re_establishes_numbering_unless_cleaning_moved_it() {
    let (_d, c) = ctx();
    std::fs::write(c.workspace.root().join("a.rs"), "one\ntwo\nthree\n").unwrap();
    let write = |body: &'static str| {
        tools::write::Write.execute(json!({"path": "a.rs", "content": body}), &c)
    };
    let tag_of = |out: &str| {
        out.split('#')
            .nth(1)
            .unwrap()
            .split(']')
            .next()
            .unwrap()
            .to_string()
    };

    // An earlier edit left a shift note; writing the file whole answers it.
    edit(
        &c,
        format!(
            "[a.rs#{}]\nPUT 1:\n+A\n+B\n",
            hashline::tag("one\ntwo\nthree\n")
        ),
    )
    .await
    .unwrap();
    let tag = tag_of(&write("one\ntwo\nTHREE\n").await.unwrap().flatten());
    edit(&c, format!("[a.rs#{tag}]\nPUT 3:\n+DONE\n"))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap(),
        "one\ntwo\nDONE\n"
    );

    // A body with read's own header pasted back: cleaning drops that row, so
    // the rows the model counted are one off the rows on disk.
    let tag = tag_of(&write("[a.rs#0000]\nx\ny\n").await.unwrap().flatten());
    let err = edit(&c, format!("[a.rs#{tag}]\nPUT 2:\n+Y\n"))
        .await
        .unwrap_err();
    assert!(err.contains("renumbered from line 1 on"), "{err}");
}

// A shift note may not outlive the content it was about: the next file at that
// path is one the model wrote itself, and every line of it is its own.
#[tokio::test]
async fn a_removed_file_takes_its_shift_note_with_it() {
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\nfour\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    edit(
        &c,
        format!("[a.rs#{}]\nPUT 1:\n+A\n+B\n", hashline::tag(src)),
    )
    .await
    .unwrap();

    let now = std::fs::read_to_string(c.workspace.root().join("a.rs")).unwrap();
    edit(&c, format!("[a.rs#{}]\nRM\n", hashline::tag(&now)))
        .await
        .unwrap();
    let fresh = "1\n2\n3\n";
    tools::write::Write
        .execute(json!({"path": "a.rs", "content": fresh}), &c)
        .await
        .unwrap();
    // Nothing about this file has ever shifted.
    edit(
        &c,
        format!("[a.rs#{}]\nPUT 3:\n+THREE\n", hashline::tag(fresh)),
    )
    .await
    .unwrap();
}

// One hunk below the shift does not vouch for the ones above it.
#[tokio::test]
async fn a_patch_is_refused_when_any_hunk_reaches_into_moved_numbering() {
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\nfour\nfive\n";
    let path = c.workspace.root().join("a.rs");
    std::fs::write(&path, src).unwrap();
    let out = edit(
        &c,
        format!("[a.rs#{}]\nPUT 2:\n+TWO\n+TWO-B\n", hashline::tag(src)),
    )
    .await
    .unwrap();
    let after = std::fs::read_to_string(&path).unwrap();
    let tag = out.split('#').nth(1).unwrap().split(']').next().unwrap();

    // Starts at 1 — below the shift — but its far end is well inside it.
    let err = edit(&c, format!("[a.rs#{tag}]\nPUT 1-4:\n+A\n+B\n+C\n+D\n"))
        .await
        .unwrap_err();
    assert!(err.contains("renumbered from line 2 on"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        after,
        "nothing written"
    );
}

// The refusal is only worth a turn if it carries the numbering. An edit that
// shortened the file is where an address runs past the end — and where an
// unclamped window would have printed a heading with nothing under it.
#[tokio::test]
async fn the_refusal_shows_rows_even_when_the_address_is_past_the_end() {
    let (_d, c) = ctx();
    let src = "one\ntwo\nthree\nfour\nfive\nsix\n";
    std::fs::write(c.workspace.root().join("a.rs"), src).unwrap();
    let out = edit(&c, format!("[a.rs#{}]\nCUT 1-3\n", hashline::tag(src)))
        .await
        .unwrap();
    let tag = out.split('#').nth(1).unwrap().split(']').next().unwrap();

    // The file is three lines now; line 6 came from the read before the cut.
    let err = edit(&c, format!("[a.rs#{tag}]\nPUT 6:\n+SIX\n"))
        .await
        .unwrap_err();
    assert!(err.contains("renumbered from line 1 on"), "{err}");
    assert!(
        err.contains("3:six"),
        "the tail, not an empty window:\n{err}"
    );
}

// The display and the body are two projections of one decision, so a row named
// by the first is a row present in the second — whatever the budget dropped in
// between. Naming what the caller *meant* to send is how it came to advertise
// `1-2000` for a body holding 1-361 and 1661-2000.
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

// The refusal exists to save a read turn, not to be one: a patch whose hunks
// span most of a file was asking for most of the file back.
#[tokio::test]
async fn the_renumber_refusal_is_budgeted_like_every_other_view() {
    let (_d, c) = ctx();
    let path = c.workspace.root().join("a.txt");
    let src: String = (1..=400)
        .map(|i| format!("row {i} of the file\n"))
        .collect();
    std::fs::write(&path, &src).unwrap();

    let out = edit(
        &c,
        format!("[a.txt#{}]\nPUT 2:\n+A\n+B\n", hashline::tag(&src)),
    )
    .await
    .unwrap();
    let tag = out.split('#').nth(1).unwrap().split(']').next().unwrap();
    // One hunk over nearly the whole file, addressed on the moved numbering.
    let err = edit(&c, format!("[a.txt#{tag}]\nPUT 3-390:\n+x\n"))
        .await
        .unwrap_err();

    assert!(err.contains("renumbered from line 2 on"), "{err}");
    assert!(err.contains("Rebuild the hunks"), "{err}");
    assert!(err.len() < 4_000, "unbudgeted, {} bytes:\n{err}", err.len());
    assert!(err.contains('…'), "and it says where it stopped:\n{err}");
}
