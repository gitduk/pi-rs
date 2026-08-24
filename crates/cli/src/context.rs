//! Standing instructions: what to do here, as opposed to what to do now.
//!
//! Personal ones live in `~/.pi.md`, beside `~/.pi.toml`. A project's live in
//! its `AGENTS.md`, the vendor-neutral name every harness reads. Another
//! harness's own file is deliberately not read in its place: the shared name
//! exists so that one file serves every tool, and reading the alternatives too
//! would reward keeping them apart.

use std::path::{Path, PathBuf};

/// Enough to notice. These ride in every request, so a repository that dumps
/// its handbook here is paying for it on every turn.
const LOUD: usize = 32 * 1024;

#[derive(Debug, Default)]
pub struct Loaded {
    /// Ready to append to the system prompt, or empty.
    pub text: String,
    pub files: Vec<PathBuf>,
    pub bytes: usize,
}

impl Loaded {
    /// Worth one line when the standing instructions have grown into a cost.
    pub fn oversized(&self) -> bool {
        self.bytes > LOUD
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The one file a directory contributes, or none.
fn in_dir(dir: &Path) -> Option<PathBuf> {
    let path = dir.join("AGENTS.md");
    path.is_file().then_some(path)
}

/// Every instructions file that applies, most general first.
///
/// Order is the whole point: the nearest directory speaks last, so where two
/// files disagree the more specific one is the one the model read most
/// recently. The walk ends at the repository root and never reaches `$HOME`,
/// whose `.pi.md` is the personal one and is already first in the list.
pub fn paths(workspace: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = home
        && let personal = h.join(".pi.md")
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
    out.extend(project);
    out
}

pub fn load(workspace: &Path) -> Loaded {
    from(workspace, home().as_deref())
}

/// The same, against a stated home rather than this process's.
///
/// A test that reads the real `$HOME` passes or fails on whether whoever runs
/// it happens to keep a `.pi.md` — which is a property of the machine, not of
/// the code under test.
fn from(workspace: &Path, home: Option<&Path>) -> Loaded {
    let mut loaded = Loaded::default();
    for path in paths(workspace, home) {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
        }
        loaded.bytes += body.len();
        // Tagged rather than headed: the content is arbitrary markdown with
        // headings of its own, so a `#` delimiter would not delimit anything.
        loaded.text.push_str(&format!(
            "\n\n<instructions path=\"{}\">\n{}\n</instructions>",
            path.display(),
            body.trim_end()
        ));
        loaded.files.push(path);
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::{from, paths};
    use std::path::Path;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn the_nearest_directory_speaks_last() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        write(&home.join(".pi.md"), "personal");
        let repo = home.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("AGENTS.md"), "repo");
        let deep = repo.join("crates/cli");
        write(&deep.join("AGENTS.md"), "crate");

        let got = paths(&deep, Some(&home));
        assert_eq!(
            got,
            vec![
                home.join(".pi.md"),
                repo.join("AGENTS.md"),
                deep.join("AGENTS.md"),
            ],
            "general to specific, so the closest one is read last"
        );
    }

    #[test]
    fn only_the_shared_name_counts() {
        // The point of a vendor-neutral name is that one file serves every
        // tool; reading the alternatives as well would reward keeping them
        // apart.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("CLAUDE.md"), "another harness's file");
        assert!(paths(&repo, None).is_empty());

        write(&repo.join("AGENTS.md"), "agents");
        assert_eq!(paths(&repo, None), vec![repo.join("AGENTS.md")]);
    }

    #[test]
    fn the_walk_stops_at_the_repository_root() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("AGENTS.md"), "outside");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(paths(&deep, None).is_empty(), "leaked past the repo root");
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
        assert!(paths(&notes, Some(&home)).is_empty());
    }

    #[test]
    fn each_file_is_tagged_with_where_it_came_from() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("AGENTS.md"), "  Use tabs.\n\n");
        let got = from(&repo, None);
        assert!(got.text.contains("<instructions path="));
        assert!(got.text.contains("Use tabs."));
        assert!(got.text.ends_with("</instructions>"), "{}", got.text);
        assert_eq!(got.files.len(), 1);
    }

    #[test]
    fn an_empty_file_contributes_nothing() {
        // A placeholder someone created and never filled in should not appear
        // as an empty block the model has to interpret.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(&repo.join("AGENTS.md"), "\n  \n");
        let got = from(&repo, None);
        assert!(got.text.is_empty());
        assert!(got.files.is_empty());
    }
}
