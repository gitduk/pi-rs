mod common;

use serde_json::json;
use tools::{Ctx, Tool, Workspace};

fn tree() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    std::fs::create_dir_all(r.join("src/deep")).unwrap();
    std::fs::create_dir_all(r.join("target")).unwrap();
    std::fs::write(r.join(".gitignore"), "target/\n*.log\n").unwrap();
    std::fs::write(
        r.join("src/main.rs"),
        "fn main() {\n    let x = 1;\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    std::fs::write(
        r.join("src/deep/util.rs"),
        "pub fn helper() {}\n// TODO: rename\n",
    )
    .unwrap();
    std::fs::write(r.join("README.md"), "# Title\nTODO: write docs\n").unwrap();
    std::fs::write(r.join("target/gen.rs"), "TODO: ignored\n").unwrap();
    std::fs::write(r.join("debug.log"), "TODO: ignored\n").unwrap();
    std::fs::write(r.join("blob.bin"), [0u8, b'T', b'O', b'D', b'O', 0]).unwrap();
    let ws = Workspace::new(r).unwrap();
    (dir, Ctx::new(ws))
}

async fn run(tool: &dyn Tool, args: serde_json::Value, ctx: &Ctx) -> String {
    tool.execute(args, ctx).await.unwrap().flatten()
}

#[tokio::test]
async fn glob_matches_at_any_depth_without_a_slash() {
    let (_d, c) = tree();
    let out = run(&tools::glob::Glob, json!({ "pattern": "*.rs" }), &c).await;
    assert!(out.contains("src/main.rs"), "{out}");
    assert!(out.contains("src/deep/util.rs"), "{out}");
}

#[tokio::test]
async fn glob_and_grep_both_honor_gitignore() {
    let (_d, c) = tree();
    let globbed = run(&tools::glob::Glob, json!({ "pattern": "*.rs" }), &c).await;
    assert!(!globbed.contains("target/"), "{globbed}");

    let grepped = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    assert!(!grepped.contains("target/gen.rs"), "{grepped}");
    assert!(!grepped.contains("debug.log"), "{grepped}");
}

#[tokio::test]
async fn glob_returns_the_newest_file_first() {
    let (_d, c) = tree();
    let newest = c.workspace.root().join("src/newest.rs");
    std::fs::write(&newest, "fn later() {}\n").unwrap();
    // mtime resolution is coarse enough that a fresh write can tie; force it.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    filetime::set_file_mtime(&newest, filetime::FileTime::from_system_time(later)).unwrap();

    let out = run(&tools::glob::Glob, json!({ "pattern": "*.rs" }), &c).await;
    assert_eq!(out.lines().next().unwrap(), "src/newest.rs", "{out}");
}

#[tokio::test]
async fn glob_scopes_to_a_subdirectory_and_reports_no_match() {
    let (_d, c) = tree();
    let out = run(
        &tools::glob::Glob,
        json!({ "pattern": "*.rs", "path": "src/deep" }),
        &c,
    )
    .await;
    assert_eq!(out.trim(), "src/deep/util.rs");

    let empty = tools::glob::Glob
        .execute(json!({ "pattern": "*.zig" }), &c)
        .await
        .unwrap();
    assert!(empty.useless && empty.flatten().contains("no file matches"));
}

/// The row a finished search leaves has to name what was searched for. A
/// tally alone is the same row for every search that found that many.
#[tokio::test]
async fn a_finished_search_names_what_it_looked_for() {
    let (_d, c) = tree();
    let found = tools::glob::Glob
        .execute(json!({ "pattern": "*.rs" }), &c)
        .await
        .unwrap();
    assert_eq!(found.preview(), "*.rs [2 files]");

    let hits = tools::grep::Grep
        .execute(json!({ "pattern": "TODO" }), &c)
        .await
        .unwrap();
    assert_eq!(hits.preview(), "TODO [2 files · 2 matches]");

    // The files-only view is the same search and says the same thing.
    let names = tools::grep::Grep
        .execute(json!({ "pattern": "TODO", "files_only": true }), &c)
        .await
        .unwrap();
    assert_eq!(names.preview(), "TODO [2 files · 2 matches]");
}

#[tokio::test]
async fn glob_reports_what_the_limit_dropped() {
    let (_d, c) = tree();
    let out = run(
        &tools::glob::Glob,
        json!({ "pattern": "*.rs", "limit": 1 }),
        &c,
    )
    .await;
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(out.contains("more; narrow the pattern"), "{out}");
}

#[tokio::test]
async fn grep_returns_sections_an_edit_can_anchor_on() {
    let (_d, c) = tree();
    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "glob": ["*.rs"] }),
        &c,
    )
    .await;

    let expected = hashline::tag(
        &std::fs::read_to_string(c.workspace.root().join("src/deep/util.rs")).unwrap(),
    );
    assert!(
        out.starts_with(&format!("[src/deep/util.rs#{expected}]")),
        "{out}"
    );
    // The whole prefix: `2:…` is what a single line reads back as now.
    assert!(out.contains("\n2:// TODO: rename"), "{out}");
    let body = std::fs::read_to_string(c.workspace.root().join("src/deep/util.rs")).unwrap();
    common::every_address_parses(&out, "src/deep/util.rs", &body, 1);

    // The whole point: the tag grep hands back is good enough to edit with.
    let patch = format!("[src/deep/util.rs#{expected}]\nPUT 2:\n+// renamed\n");
    tools::edit::Edit
        .execute(json!({ "patch": patch }), &c)
        .await
        .unwrap();
    assert!(
        std::fs::read_to_string(c.workspace.root().join("src/deep/util.rs"))
            .unwrap()
            .contains("// renamed")
    );
}

#[tokio::test]
async fn grep_skips_binaries_rather_than_emitting_noise() {
    let (_d, c) = tree();
    let out = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    assert!(!out.contains("blob.bin"), "{out}");
    assert!(out.contains("README.md"), "{out}");
}

#[tokio::test]
async fn grep_filters_by_glob_and_by_case() {
    let (_d, c) = tree();
    let scoped = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "glob": ["*.md"] }),
        &c,
    )
    .await;
    assert!(
        scoped.contains("README.md") && !scoped.contains("util.rs"),
        "{scoped}"
    );

    let exact = tools::grep::Grep
        .execute(json!({ "pattern": "todo" }), &c)
        .await
        .unwrap();
    assert!(exact.useless, "{}", exact.flatten());
    let loose = run(
        &tools::grep::Grep,
        json!({ "pattern": "todo", "insensitive": true }),
        &c,
    )
    .await;
    assert!(loose.contains("TODO"), "{loose}");
}

#[tokio::test]
async fn files_only_lists_paths_with_their_counts() {
    let (_d, c) = tree();
    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "files_only": true }),
        &c,
    )
    .await;
    assert!(out.contains("README.md (1 matches)"), "{out}");
    assert!(!out.contains(':'), "files_only must not emit lines: {out}");
}

#[tokio::test]
async fn grep_order_does_not_change_between_identical_searches() {
    let (_d, c) = tree();
    // The parallel walker finishes files in whatever order it likes.
    let a = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    let b = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    assert_eq!(a, b);
}

#[tokio::test]
async fn a_bad_pattern_says_so_instead_of_returning_nothing() {
    let (_d, c) = tree();
    let err = tools::grep::Grep
        .execute(json!({ "pattern": "fn (" }), &c)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("bad pattern `fn (`"), "{err}");
}

#[tokio::test]
async fn search_reaches_outside_the_workspace() {
    let (_d, c) = tree();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("out.txt"), "needle here\n").unwrap();

    let g = tools::grep::Grep
        .execute(
            json!({ "pattern": "needle", "path": outside.path().to_str().unwrap() }),
            &c,
        )
        .await
        .unwrap();
    assert!(g.flatten().contains("needle"), "{g:?}");

    let l = tools::glob::Glob
        .execute(
            json!({ "pattern": "*.txt", "path": outside.path().to_str().unwrap() }),
            &c,
        )
        .await
        .unwrap();
    assert!(l.flatten().contains("out.txt"), "{l:?}");
}

#[tokio::test]
async fn the_git_directory_is_never_swept() {
    let (_d, c) = tree();
    let r = c.workspace.root();
    std::fs::create_dir_all(r.join(".git/objects")).unwrap();
    std::fs::write(r.join(".git/config"), "TODO: internal\n").unwrap();
    std::fs::create_dir_all(r.join(".github/workflows")).unwrap();
    std::fs::write(r.join(".github/workflows/ci.yml"), "TODO: ci\n").unwrap();

    let out = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    // An object store is megabytes of noise the model can do nothing with.
    assert!(!out.contains(".git/"), "{out}");
    // Other dotted directories are ordinary project files.
    assert!(out.contains(".github/workflows/ci.yml"), "{out}");
}

#[tokio::test]
async fn a_file_with_invalid_utf8_still_reports_its_matches() {
    let (_d, c) = tree();
    let mut bytes = b"fn a() {}\n// TODO: fix \xF0\x28 here\n".to_vec();
    bytes.extend_from_slice(b"fn b() {}\n");
    std::fs::write(c.workspace.root().join("src/odd.rs"), &bytes).unwrap();

    // Searching raw bytes through a UTF-8 sink drops the whole file silently.
    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "glob": ["odd.rs"] }),
        &c,
    )
    .await;
    assert!(out.contains("src/odd.rs"), "{out}");
    assert!(out.contains("2:"), "{out}");
}

// `limit` is what the model asked for; the byte cap is what the window holds.
// Only the second needs a locator — only the second the model never chose.

#[tokio::test]
async fn an_over_long_grep_drops_whole_sections_and_spills_the_rest() {
    let (_d, c) = common::spilling();
    let r = c.workspace.root();
    // Each file is a section of its own, and no one section is over budget: the
    // cut has to land between them.
    for i in 0..40 {
        std::fs::write(
            r.join(format!("f{i:02}.txt")),
            format!("NEEDLE {}\n", "z".repeat(2_000)),
        )
        .unwrap();
    }

    let out = run(&tools::grep::Grep, json!({ "pattern": "NEEDLE" }), &c).await;
    assert!(out.len() <= tools::spill::MAX_OUTPUT, "{} bytes", out.len());

    // Every row that survived carries its whole line, and every row sits under
    // a header — a row without its `[path#TAG]` has no tag for an edit to name.
    let mut headed = false;
    for line in out.lines() {
        if line.starts_with('[') {
            headed = true;
        } else if let Some((_, text)) = line.split_once(':')
            && line.starts_with(char::is_numeric)
        {
            assert!(headed, "a row before any header: {line}");
            assert_eq!(text.len(), "NEEDLE ".len() + 2_000, "half a row: {line}");
        }
    }

    // The count has to answer for the byte cut too: a body saying only what the
    // line limit dropped reads as near-complete when most sections went.
    assert!(out.contains("of 40 files did not fit the window"), "{out}");
    let whole = common::spilled_body(&c, &out);
    assert!(whole.contains("f39.txt"), "the spill must hold what went");
    assert!(!out.contains("f39.txt"), "nothing was actually dropped\n{out}");
}

#[tokio::test]
async fn a_grep_within_budget_carries_no_locator() {
    let (_d, c) = tree();
    let out = run(&tools::grep::Grep, json!({ "pattern": "TODO" }), &c).await;
    assert!(!out.contains("full output:"), "{out}");
    assert!(out.contains("README.md"), "{out}");
}

#[tokio::test]
async fn one_section_over_budget_leaves_only_the_locator() {
    let (_d, c) = common::spilling();
    std::fs::write(
        c.workspace.root().join("min.js"),
        format!("NEEDLE {}\n", "z".repeat(tools::spill::MAX_OUTPUT)),
    )
    .unwrap();

    let out = run(&tools::grep::Grep, json!({ "pattern": "NEEDLE" }), &c).await;
    // Better an empty view with a locator than half a row under an address.
    assert!(!out.contains("NEEDLE"), "a section was split\n{out}");
    assert!(out.contains("… 1 of 1 files did not fit"), "{out}");
    let whole = common::spilled_body(&c, &out);
    assert!(whole.contains("NEEDLE"), "the spill must hold what went");
}

#[tokio::test]
async fn files_only_answers_to_the_same_limit_as_the_line_view() {
    let (_d, c) = tree();
    let r = c.workspace.root();
    for i in 0..10 {
        std::fs::write(r.join(format!("m{i}.md")), "TODO: x\n").unwrap();
    }

    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "files_only": true, "limit": 3 }),
        &c,
    )
    .await;
    assert_eq!(
        out.lines().filter(|l| l.contains("matches)")).count(),
        3,
        "{out}"
    );
    assert!(out.contains("more files; narrow the pattern"), "{out}");
}

#[tokio::test]
async fn files_only_reports_the_files_it_could_not_search() {
    let (_d, c) = tree();
    let big = c.workspace.root().join("huge.txt");
    std::fs::write(&big, "TODO\n").unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&big).unwrap();
    f.set_len(11 << 20).unwrap();

    let out = run(
        &tools::grep::Grep,
        json!({ "pattern": "TODO", "files_only": true }),
        &c,
    )
    .await;
    assert!(out.contains("over the size limit"), "{out}");
}

#[tokio::test]
async fn an_over_long_glob_spills_its_tail() {
    let (_d, c) = common::spilling();
    let r = c.workspace.root();
    let long = "n".repeat(120);
    for i in 0..400 {
        std::fs::write(r.join(format!("{long}{i:03}.txt")), "x").unwrap();
    }

    let out = run(
        &tools::glob::Glob,
        json!({ "pattern": "*.txt", "limit": 400 }),
        &c,
    )
    .await;
    assert!(out.len() <= tools::spill::MAX_OUTPUT, "{} bytes", out.len());
    let whole = common::spilled_body(&c, &out);
    assert_eq!(
        whole.lines().filter(|l| l.ends_with(".txt")).count(),
        400,
        "the spill must hold every path"
    );
}
