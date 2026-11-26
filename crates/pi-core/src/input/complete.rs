//! Path candidates: recognizing `@` and `~/` tokens, listing files or
//! directories, and ranking matches. No terminal here, so tests run against
//! a scratch tree.

use std::path::Path;

// Where a token ends and a new one may start. The newline keeps the scan
// inside the line the caret is on, as the per-line parse upstream does.
const DELIMS: [char; 6] = [' ', '\t', '"', '\'', '=', '\n'];
// File suggestions are bounded; directory navigation keeps every entry.
const WALK_CAP: usize = 2000;
// What file suggestions offer at most.
const TOP: usize = 20;
const GIT_DIR: &str = ".git";

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
    let start = token_start(line, cursor);
    let query = line[start..cursor].strip_prefix('@')?;
    Some((start, cursor, query))
}

/// The `~/token` under the caret, read as `at_prefix` reads an `@` one; the
/// query is what follows `~/`.
pub fn home_prefix(line: &str, cursor: usize) -> Option<(usize, usize, &str)> {
    let start = token_start(line, cursor);
    let query = line[start..cursor].strip_prefix("~/")?;
    Some((start, cursor, query))
}

fn token_start(line: &str, cursor: usize) -> usize {
    line[..cursor]
        .char_indices()
        .rev()
        .find(|(_, c)| DELIMS.contains(c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Ranked candidates for the text after the `@`, best first, at most `TOP`.
///
/// Only one level is listed, the folder the query names (`src/`, `src/fo`)
/// or the root; absolute, `~` and `..` scopes are refused. Gitignored
/// entries are offered like any other, `.git` alone stays out.
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
    if query
        .rsplit_once('/')
        .is_some_and(|(folder, _)| folder.split('/').any(|part| part == GIT_DIR))
    {
        return Vec::new();
    }
    let (scope, display_base, name) = split(&query, root, "");
    let entries = read_folder(&scope)
        .filter(|(rel, _)| rel != GIT_DIR)
        .take(WALK_CAP);
    rank(entries, name, &display_base, false)
}

/// Candidates under `home` for the text after `~/`, each spelled from `~/`.
///
/// One level, as `candidates`; `..` is refused, as there.
pub fn home_candidates(query: &str, home: &Path) -> Vec<FileEntry> {
    if query.split('/').any(|s| s == "..") {
        return Vec::new();
    }
    listed(query, home, "~/", false)
}

/// Directories spelled as typed: from `~/` under `home`,
/// from `/` absolute, otherwise against `root` — `..` included.
pub fn directories(query: &str, root: &Path, home: Option<&Path>) -> Vec<FileEntry> {
    if let Some(rest) = query.strip_prefix("~/") {
        return home.map_or_else(Vec::new, |home| listed(rest, home, "~/", true));
    }
    if let Some(rest) = query.strip_prefix('/') {
        return listed(rest, Path::new("/"), "/", true);
    }
    listed(query, root, "", true)
}

fn read_folder(dir: &Path) -> impl Iterator<Item = (String, bool)> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let e = e.ok()?;
            let rel = e.file_name().into_string().ok()?;
            // Follow symlinks to navigable directories.
            let dir = match e.file_type() {
                Ok(kind) if !kind.is_symlink() => kind.is_dir(),
                _ => e.path().is_dir(),
            };
            Some((rel, dir))
        })
}

// One folder read as it is. Dot entries wait for a typed `.`, as in a shell.
fn listed(query: &str, base: &Path, lead: &str, dirs_only: bool) -> Vec<FileEntry> {
    let (scope, display_base, name) = split(query, base, lead);
    let hidden = name.starts_with('.');
    let entries = read_folder(&scope)
        .filter(|(rel, dir)| (hidden || !rel.starts_with('.')) && (*dir || !dirs_only))
        .take(if dirs_only { usize::MAX } else { WALK_CAP });
    rank(entries, name, &display_base, dirs_only)
}

// The folder `query` names under `root`, how its entries are spelled from
// `lead`, and the name being typed in it.
fn split<'a>(query: &'a str, root: &Path, lead: &str) -> (std::path::PathBuf, String, &'a str) {
    match query.rsplit_once('/') {
        Some((folder, name)) => (root.join(folder), format!("{lead}{folder}/"), name),
        None => (root.to_path_buf(), lead.to_string(), query),
    }
}

fn rank(
    entries: impl Iterator<Item = (String, bool)>,
    name: &str,
    display_base: &str,
    dirs_only: bool,
) -> Vec<FileEntry> {
    let query_lower = name.to_lowercase();
    let browse = dirs_only && name.is_empty();
    let mut scored: Vec<(i32, FileEntry)> = entries
        .filter_map(|(rel, dir)| {
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
            (score > 0).then(|| {
                let path = format!("{display_base}{rel}");
                (score, FileEntry { path, dir })
            })
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| match browse {
                true => std::cmp::Ordering::Equal,
                false => a.1.path.len().cmp(&b.1.path.len()),
            })
            .then_with(|| a.1.path.cmp(&b.1.path))
    });
    if !dirs_only {
        scored.truncate(TOP);
    }
    scored.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // src/lib/mod.rs, src/main.rs, README.md, target/miss.rs (gitignored,
    // still offered), .git/config (excluded).
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
        // `.git` stays out; other dot entries are plain candidates.
        assert_eq!(
            names(&got),
            vec!["src", "target", "README.md", ".gitignore"]
        );
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
    fn gitignored_entries_are_offered_and_git_metadata_is_excluded() {
        let (_dir, root) = tree();
        assert_eq!(names(&candidates("ta", &root)), vec!["target"]);
        assert_eq!(names(&candidates(".gi", &root)), vec![".gitignore"]);
        assert_eq!(names(&candidates(".git", &root)), vec![".gitignore"]);
        assert!(candidates(".git/", &root).is_empty());
    }

    #[test]
    fn home_paths_spell_from_the_tilde_and_hide_dot_entries() {
        let (_dir, root) = tree();
        assert_eq!(home_prefix("/mv ~/src/m", 11), Some((4, 11, "src/m")));
        assert_eq!(home_prefix("~5 min", 2), None);
        assert_eq!(
            names(&home_candidates("", &root)),
            vec!["~/src", "~/target", "~/README.md"]
        );
        assert_eq!(
            names(&home_candidates(".gi", &root)),
            vec!["~/.git", "~/.gitignore"]
        );
        assert_eq!(
            names(&home_candidates("src/m", &root)),
            vec!["~/src/main.rs"]
        );
        assert!(home_candidates("../out", &root).is_empty());
    }

    #[test]
    fn directories_are_offered_against_the_root_or_as_spelled() {
        let (_dir, root) = tree();
        assert_eq!(names(&directories("", &root, None)), vec!["src", "target"]);
        assert_eq!(names(&directories("src/", &root, None)), vec!["src/lib"]);
        assert_eq!(
            names(&directories("src/../s", &root, None)),
            vec!["src/../src"]
        );
        assert_eq!(names(&directories("~/", &root, Some(&root)))[0], "~/src");
        let abs = format!("{}/s", root.display());
        assert_eq!(
            names(&directories(&abs, &root, None)),
            vec![format!("{}/src", root.display())]
        );
    }

    #[test]
    fn directory_navigation_keeps_all_siblings_in_name_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        let mut expected = vec![
            "../a_long_directory".to_string(),
            "../work".into(),
            "../z".into(),
        ];
        for i in 0..30 {
            expected.push(format!("../project{i:02}"));
        }
        for path in &expected {
            fs::create_dir(dir.path().join(path.strip_prefix("../").unwrap())).unwrap();
        }
        fs::create_dir(dir.path().join(".hidden")).unwrap();
        fs::write(dir.path().join("a_file"), "").unwrap();
        expected.sort();
        assert_eq!(names(&directories("../", &root, None)), expected);
        assert_eq!(directories("../project", &root, None).len(), 30);

        // File suggestions still have their own relevance ranking and bound.
        let files = candidates("project", dir.path());
        assert_eq!(files.len(), TOP);
        assert_eq!(files[0].path, "project00");
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
