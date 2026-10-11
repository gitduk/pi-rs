//! The `@` file completion's pure logic: pulling the token out of the line,
//! walking the workspace, and ranking what it finds. No terminal here, so
//! the tests run against a scratch tree.

use std::path::Path;

// Where a token ends and a new one may start. The newline keeps the scan
// inside the line the caret is on, as the per-line parse upstream does.
const DELIMS: [char; 6] = [' ', '\t', '"', '\'', '=', '\n'];
// A listing stops here: a folder this full is no menu anyway.
const WALK_CAP: usize = 2000;
// What the menu shows at most.
const TOP: usize = 20;

/// One candidate: the path as the completed `@token` should read,
/// workspace-relative, and whether it names a directory — a directory
/// completes without a trailing space, so the walk can descend.
pub struct FileEntry {
    pub path: String,
    pub dir: bool,
}

/// The `@token` under the caret: where it starts (`@` included), where it
/// ends, and the query after the `@`. `None` when the caret is not in one —
/// the `@` must start the token, so `a@b` stays an email.
pub fn at_prefix(line: &str, cursor: usize) -> Option<(usize, usize, &str)> {
    let head = &line[..cursor];
    let start = head
        .char_indices()
        .rev()
        .find(|(_, c)| DELIMS.contains(c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let token = &line[start..cursor];
    if !token.starts_with('@') {
        return None;
    }
    Some((start, cursor, &token[1..]))
}

/// Ranked candidates for the text after the `@`, best first, at most `TOP`.
///
/// Only one level is listed, the folder the query names (`src/`, `src/fo`)
/// or the root; absolute, `~` and `..` scopes are refused.
pub fn candidates(query: &str, root: &Path) -> Vec<FileEntry> {
    if root.as_os_str().is_empty() {
        return Vec::new();
    }
    let query = query.replace('\\', "/");
    // Workspace-relative names only: an absolute, home or parent scope would
    // label files the prompt cannot mean, or walk out of the workspace.
    if query.starts_with(['/', '~']) || query.split('/').any(|s| s == "..") {
        return Vec::new();
    }
    let (scope, display_base, name) = match query.rfind('/') {
        Some(i) => (
            root.join(&query[..i]),
            query[..=i].to_string(),
            query[i + 1..].to_string(),
        ),
        None => (root.to_path_buf(), String::new(), query),
    };

    // The tools' walk, so `@` offers what grep and glob would find. The scope
    // never leaves the root, so the global gitignore applies.
    let mut raw: Vec<(String, bool)> = Vec::new();
    let walk = toolbox::walk::walker_at(&scope, true, None)
        .max_depth(Some(1))
        .build();
    for entry in walk.flatten() {
        let Ok(rel) = entry.path().strip_prefix(&scope) else {
            continue;
        };
        let Some(rel) = rel.to_str() else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        let dir = entry.file_type().is_some_and(|t| t.is_dir());
        raw.push((rel.replace('\\', "/"), dir));
        if raw.len() >= WALK_CAP {
            break;
        }
    }

    let query_lower = name.to_lowercase();
    let mut scored: Vec<(i32, FileEntry)> = raw
        .into_iter()
        .filter_map(|(rel, dir)| {
            let path = format!("{display_base}{rel}");
            let score = if query_lower.is_empty() {
                // Nothing to match: children of the scope, folders first.
                if dir { 110 } else { 100 }
            } else {
                let lower = rel.to_lowercase();
                let mut s = if lower == query_lower {
                    100
                } else if lower.starts_with(&query_lower) {
                    80
                } else if lower.contains(&query_lower) {
                    50
                } else {
                    0
                };
                if dir && s > 0 {
                    s += 10;
                }
                s
            };
            (score > 0).then_some((score, FileEntry { path, dir }))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.path.len().cmp(&b.1.path.len()))
            .then_with(|| a.1.path.cmp(&b.1.path))
    });
    scored.truncate(TOP);
    scored.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // src/lib/mod.rs, src/main.rs, README.md, target/miss.rs (ignored),
    // .git/config (excluded).
    fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("src/lib")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "").unwrap();
        fs::write(root.join("src/lib/mod.rs"), "").unwrap();
        fs::write(root.join("README.md"), "").unwrap();
        fs::write(root.join("target/miss.rs"), "").unwrap();
        fs::write(root.join(".git/config"), "").unwrap();
        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        (dir, root)
    }

    fn names(v: &[FileEntry]) -> Vec<&str> {
        v.iter().map(|e| e.path.as_str()).collect()
    }

    #[test]
    fn the_at_token_must_start_the_word() {
        assert_eq!(at_prefix("@src/f", 6), Some((0, 6, "src/f")));
        assert_eq!(at_prefix("hey @src/f", 10), Some((4, 10, "src/f")));
        // An email is not a path.
        assert_eq!(at_prefix("a@b", 3), None);
        // The newline ends the token, as any delimiter.
        assert_eq!(at_prefix("@a\n@b", 4), Some((3, 4, "")));
    }

    #[test]
    fn bare_at_lists_the_root_folders_first() {
        let (_dir, root) = tree();
        let got = candidates("", &root);
        // Upstream's fd hides `.git` itself but not other dot-files, so the
        // gitignore file is a plain candidate here too.
        assert_eq!(names(&got), vec!["src", "README.md", ".gitignore"]);
        assert!(got[0].dir);
    }

    #[test]
    fn a_query_matches_one_level_only() {
        let (_dir, root) = tree();
        // src/main.rs is a level down: reached through `src/`, not by name.
        assert!(candidates("main", &root).is_empty());
        assert_eq!(names(&candidates("src/li", &root)), vec!["src/lib"]);
    }

    #[test]
    fn a_scoped_query_lists_the_scope_children_only() {
        let (_dir, root) = tree();
        assert_eq!(
            names(&candidates("src/", &root)),
            vec!["src/lib", "src/main.rs"]
        );
        assert_eq!(names(&candidates("src/ma", &root)), vec!["src/main.rs"]);
    }

    #[test]
    fn ignored_and_git_entries_stay_out() {
        let (_dir, root) = tree();
        assert!(candidates("ta", &root).is_empty());
        assert_eq!(names(&candidates(".gi", &root)), vec![".gitignore"]);
    }

    #[test]
    fn scopes_that_leave_the_workspace_complete_nothing() {
        let (_dir, root) = tree();
        assert!(candidates("/et", &root).is_empty());
        assert!(candidates("~/notes", &root).is_empty());
        assert!(candidates("../out", &root).is_empty());
        assert!(candidates("src/../../out", &root).is_empty());
    }
}
