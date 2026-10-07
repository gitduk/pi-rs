//! Standing instructions: what to do here, as opposed to what to do now.
//!
//! Both are `AGENTS.md`, the vendor-neutral name every harness reads — yours
//! at the pi root, a project's in the project. Other harnesses' own files are
//! deliberately not read, so one file serves every tool instead of two.

use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Loaded {
    /// Each file that applies and is not blank, with its text.
    pub files: Vec<(PathBuf, String)>,
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// What both the personal file and a project's are called.
const NAME: &str = "AGENTS.md";

// The one file a directory contributes, or none.
fn in_dir(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(NAME);
    path.is_file().then_some(path)
}

/// A path as a person would name it, shortest form first: relative to the
/// workspace, then `~`, then absolute. A file inherited from a directory above
/// the workspace lands in the middle case, which is the point — a bare
/// `AGENTS.md` would not say it came from somewhere else.
pub fn short(path: &Path, root: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.display().to_string();
    }
    if let Some(h) = home()
        && let Ok(rel) = path.strip_prefix(&h)
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}
/// Every instructions file that applies, most general first.
///
/// The nearest directory speaks last, so where files disagree the more
/// specific one was read most recently. The walk ends at the repository
/// root and never reaches `$HOME`, whose file is already first in the list.
pub fn paths(workspace: &Path, home: Option<&Path>, root: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(r) = root
        && let personal = r.join(NAME)
        && personal.is_file()
    {
        out.push(personal);
    }
    let mut project = Vec::new();
    for dir in workspace.ancestors() {
        if home == Some(dir) {
            break;
        }
        project.extend(in_dir(dir));
        if dir.join(".git").exists() {
            break;
        }
    }
    project.reverse();
    // Both files share a name, so a workspace inside the pi root reaches the
    // personal one on the way up and would send it twice.
    for path in project {
        if !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// `root` is the host's own directory, whose file is the personal one.
pub fn load(workspace: &Path, root: Option<&Path>) -> Loaded {
    from(workspace, home().as_deref(), root)
}

// The same, against a stated home and pi root, so a test does not pass or
// fail on whether the machine running it happens to have a real `$HOME`.
fn from(workspace: &Path, home: Option<&Path>, root: Option<&Path>) -> Loaded {
    let mut loaded = Loaded::default();
    for path in paths(workspace, home, root) {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        loaded.files.push((path, body));
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::paths;
    use std::path::Path;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn the_nearest_directory_speaks_last() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join(".pi");
        write(&root.join("AGENTS.md"), "personal");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("AGENTS.md"), "repo");
        let deep = repo.join("crates/pi");
        write(&deep.join("AGENTS.md"), "crate");

        let got = paths(&deep, Some(&home), Some(&root));
        assert_eq!(
            got,
            vec![
                root.join("AGENTS.md"),
                repo.join("AGENTS.md"),
                deep.join("AGENTS.md"),
            ],
            "general to specific, so the closest one is read last"
        );
    }

    #[test]
    fn the_personal_file_is_not_sent_twice_when_it_is_also_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = home.join(".pi");
        write(&root.join("AGENTS.md"), "personal");

        assert_eq!(
            paths(&root, Some(&home), Some(&root)),
            vec![root.join("AGENTS.md")],
            "one file, read once"
        );
    }

    #[test]
    fn the_walk_stops_at_the_repository_root() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("AGENTS.md"), "outside");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(
            paths(&deep, None, None).is_empty(),
            "leaked past the repo root"
        );
    }

    #[test]
    fn home_is_not_walked_as_a_project() {
        // Otherwise every directory under $HOME outside a repository inherits
        // whatever AGENTS.md happens to sit there.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write(&home.join("AGENTS.md"), "stray");
        let notes = home.join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        assert!(paths(&notes, Some(&home), None).is_empty());
    }
}
