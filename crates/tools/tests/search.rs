mod common;

use serde_json::json;
use tools::{Ctx, Tool, ToolError, Workspace};

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
    // The whole prefix: `2-2:…` contains `2:…`, so the looser form passed by
    // accident through a grammar change that moved every address.
    assert!(out.contains("\n2-2:// TODO: rename"), "{out}");
    let body = std::fs::read_to_string(c.workspace.root().join("src/deep/util.rs")).unwrap();
    common::every_address_parses(&out, "src/deep/util.rs", &body, 1);

    // The whole point: the tag grep hands back is good enough to edit with.
    let patch = format!("[src/deep/util.rs#{expected}]\nPUT 2-2:\n+// renamed\n");
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
async fn search_cannot_reach_outside_the_workspace() {
    let (_d, c) = tree();
    for r in [
        tools::grep::Grep
            .execute(json!({ "pattern": "x", "path": "../" }), &c)
            .await,
        tools::glob::Glob
            .execute(json!({ "pattern": "*", "path": "/etc" }), &c)
            .await,
    ] {
        assert!(matches!(r, Err(ToolError::Escape(_))), "{r:?}");
    }
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
