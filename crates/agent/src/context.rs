//! Standing instructions: what to do here, as opposed to what to do now.
//!
//! Both are `AGENTS.md`, the vendor-neutral name every harness reads — yours
//! at the pi root, a project's in the project. Other harnesses' own files are
//! deliberately not read, so one file serves every tool instead of two.

use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Loaded {
    /// Ready to append to the system prompt, or empty.
    pub text: String,
    pub files: Vec<PathBuf>,
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
// An XML reader ends a node where a quote or a `<` says it does, so a path
// riding inside one loses those to the references first.
fn escaped(path: impl std::fmt::Display) -> String {
    path.to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The directory the run works in, and what that means for a path. Said
/// here rather than in the system prompt, which the user may replace.
pub fn workspace(root: &Path) -> String {
    format!(
        "\n\n<workspace path=\"{}\"/>\n\nThe workspace is the directory you work in. Every \
path you name is relative to it, and commands start in it. Writing stays inside it unless a \
`<write_paths>` block names more places; reading may go further — an absolute path reaches \
the rest of this machine, a URL the rest of the world.",
        escaped(root.display())
    )
}

/// pi's home, for a run that may add tools and skills to it.
pub fn pi_home(dir: &Path) -> String {
    format!(
        "\n\n<pi_home path=\"{}\">\npi's own home, read live: a tool or skill written here is \
offered from your next turn, with no restart. The `pi-tool` skill says how to write one.\n</pi_home>",
        escaped(dir.display())
    )
}

/// What the model may change, and where — the workspace root plus every
/// configured write root, as far as this run's ceiling reaches. The write and
/// exec tools enforce exactly this set, spelled out here so the escape
/// refusal is not the model's first hint of the boundary.
pub fn boundary(ws: &tool::Workspace, tier: tool::Tier) -> String {
    let extras = ws.write_roots();
    // Nothing to say when the run may not write at all, or when the workspace
    // is the whole boundary — the `<workspace>` tag already names that.
    if !tool::Tier::Write.under(tier) || extras.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "\n\n<write_paths root=\"{}\">",
        escaped(ws.root().display())
    );
    for root in extras {
        out.push_str(&format!("\n  {}", escaped(root.display())));
    }
    out.push_str(
        "\n</write_paths>\n\nPaths inside these directories are writable; elsewhere write and \
edit refuse.",
    );
    if tool::Tier::Exec.under(tier) {
        // Said only where it holds: a run capped below `exec` may not run `sh`.
        out.push_str(" bash can still write anywhere its redirections name.");
    }
    out
}

/// What this run is, as against what it is working on.
///
/// Fields here hold still for the whole run, since this rides the cached
/// system-prompt prefix; only `stamp`'s day is kept, so runs an hour apart
/// still share one cache entry.
pub fn env(stamp: &str, tier: tool::Tier) -> String {
    let day = stamp.split_once('T').map_or(stamp, |(day, _)| day);
    // `sh`, not `$SHELL`: the bash tool runs `Command::new("sh")` whatever the
    // login shell is, and the tool's own name is what misleads about it.
    let tier = format!("{tier:?}").to_lowercase();
    format!(
        "\n\n<env date=\"{day}\" platform=\"{}\" shell=\"sh\" pi=\"{}\" tier=\"{tier}\"/>",
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION"),
    )
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
        if loaded.files.is_empty() {
            loaded.text.push_str(
                "\n\nThe `<instructions>` blocks below are the user's standing instructions, \
for this machine and this project, most general first. Follow them; where two disagree, the \
later one, nearer the workspace, wins. They say how to work here; the user's message says what \
to do now.",
            );
        }
        // Tagged rather than headed: the content is arbitrary markdown with
        // headings of its own, so a `#` delimiter would not delimit anything.
        loaded.text.push_str(&format!(
            "\n\n<instructions path=\"{}\">\n{}\n</instructions>",
            escaped(path.display()),
            body.trim_end()
        ));
        loaded.files.push(path);
    }
    loaded
}

#[cfg(test)]
mod tests {
    use super::{boundary, env, paths};
    use std::path::Path;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn the_write_block_claims_only_what_the_ceiling_allows() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = tool::Workspace::new(dir.path())
            .unwrap()
            .with_write_roots(&[outside.path()])
            .unwrap();

        assert!(boundary(&ws, tool::Tier::Read).is_empty());
        assert!(boundary(&ws, tool::Tier::Net).is_empty());

        // Printed as `resolve` admits it, which an existing tempdir may spell
        // differently once its links are gone.
        let shown = outside.path().canonicalize().unwrap();
        let shown = shown.to_str().unwrap();
        let write = boundary(&ws, tool::Tier::Write);
        assert!(write.contains(shown), "{write}");
        assert!(!write.contains("bash"), "{write}");
        assert!(boundary(&ws, tool::Tier::Exec).contains("bash"));

        // No write root beyond the workspace: `<workspace>` already names the
        // whole boundary, so there is nothing to add.
        let bare = tool::Workspace::new(dir.path()).unwrap();
        assert!(boundary(&bare, tool::Tier::Exec).is_empty());
    }

    #[test]
    fn the_env_date_moves_once_a_day_not_every_run() {
        let got = env("2026-09-21T12:00:00.000Z", tool::Tier::Read);
        assert!(got.contains("date=\"2026-09-21\""), "{got}");
        assert_eq!(got, env("2026-09-21T13:00:00.000Z", tool::Tier::Read));
        assert_ne!(got, env("2026-09-22T12:00:00.000Z", tool::Tier::Read));
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
