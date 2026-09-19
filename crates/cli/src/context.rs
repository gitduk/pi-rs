//! Standing instructions: what to do here, as opposed to what to do now.
//!
//! Both are `AGENTS.md`, the vendor-neutral name every harness reads: yours at
//! the pi root beside `settings.toml`, a project's in the project. Another
//! harness's own file is deliberately not read in its place: the shared name
//! exists so that one file serves every tool, and reading the alternatives too
//! would reward keeping them apart. One name rather than two for the same job,
//! for the same reason.

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

/// The anchor for "every path is relative to it": the model needs to know
/// which directory that is before the rest of the system prompt makes sense.
pub fn workspace(root: &Path) -> String {
    format!("\n\n<workspace path=\"{}\"/>", escaped(root.display()))
}

/// What the model may change, and where — the workspace root plus every
/// configured write root, as far as this run's ceiling reaches. The write and
/// exec tools enforce exactly this set; the model needs it spelled out before
/// it picks a tool for a path outside the workspace, so the escape refusal is
/// not the first it hears of the boundary.
pub fn boundary(ws: &tools::Workspace, tier: tools::Tier) -> String {
    let extras = ws.write_roots();
    // Nothing to say when the run may not write at all, or when the workspace
    // is the whole boundary — the `<workspace>` tag already names that.
    if !tools::Tier::Write.under(tier) || extras.is_empty() {
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
    if tools::Tier::Exec.under(tier) {
        // Said only where it holds: a run capped below `exec` may not run `sh`.
        out.push_str(" bash can still write anywhere its redirections name.");
    }
    out
}

/// What this run is, as against what it is working on.
///
/// Everything here holds still for the whole run, because it rides the system
/// prompt — the part a provider caches. A field that moved mid-run would cost
/// that cache every turn, which is why the model and the window are not here:
/// the window is the denominator of a number the turn already carries.
///
/// The date is a day, not an instant, so that two runs an hour apart still
/// share one cached prefix.
pub fn env(tier: tools::Tier) -> String {
    // `sh`, not `$SHELL`: the bash tool runs `Command::new("sh")` whatever the
    // login shell is, and the tool's own name is what misleads about it.
    let day = &crate::journal::rfc3339(std::time::SystemTime::now())[..10];
    let tier = format!("{tier:?}").to_lowercase();
    format!(
        "\n\n<env date=\"{day}\" platform=\"{}\" shell=\"sh\" pi=\"{}\" tier=\"{tier}\"/>",
        std::env::consts::OS,
        env!("CARGO_PKG_VERSION"),
    )
}

/// Every instructions file that applies, most general first.
///
/// Order is the whole point: the nearest directory speaks last, so where two
/// files disagree the more specific one is the one the model read most
/// recently. The walk ends at the repository root and never reaches `$HOME`,
/// whose file is the personal one and is already first in the list.
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

pub fn load(workspace: &Path) -> Loaded {
    from(workspace, home().as_deref(), tools::state::dir().as_deref())
}

// The same, against a stated home and pi root rather than this process's.
//
// A test that reads the real `$HOME` passes or fails on whether whoever runs
// it happens to keep one — which is a property of the machine, not of
// the code under test.
fn from(workspace: &Path, home: Option<&Path>, root: Option<&Path>) -> Loaded {
    let mut loaded = Loaded::default();
    for path in paths(workspace, home, root) {
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        if body.trim().is_empty() {
            continue;
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

    // The block is advice the model acts on, so it may claim only what the
    // ceiling makes true: a run that cannot write hears nothing about
    // writable paths, and one that cannot run a shell is not told a shell is
    // the way around the boundary.
    #[test]
    fn the_write_block_claims_only_what_the_ceiling_allows() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = tools::Workspace::new(dir.path())
            .unwrap()
            .with_write_roots(&[outside.path()])
            .unwrap();

        assert!(boundary(&ws, tools::Tier::Read).is_empty());
        assert!(boundary(&ws, tools::Tier::Net).is_empty());

        // Printed as `resolve` admits it, which an existing tempdir may spell
        // differently once its links are gone.
        let shown = outside.path().canonicalize().unwrap();
        let shown = shown.to_str().unwrap();
        let write = boundary(&ws, tools::Tier::Write);
        assert!(write.contains(shown), "{write}");
        assert!(!write.contains("bash"), "{write}");
        assert!(boundary(&ws, tools::Tier::Exec).contains("bash"));

        // No write root beyond the workspace: `<workspace>` already names the
        // whole boundary, so there is nothing to add.
        let bare = tools::Workspace::new(dir.path()).unwrap();
        assert!(boundary(&bare, tools::Tier::Exec).is_empty());
    }

    // The block rides the cached prompt prefix, so its one moving part, the
    // date, has to move once a day — not on every run.
    #[test]
    fn the_env_date_moves_once_a_day_not_every_run() {
        let got = env(tools::Tier::Read);
        let date = got
            .split("date=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(date.len(), 10, "a day, not an instant: {date}");
        assert_eq!(date.matches('-').count(), 2, "{date}");
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
        let deep = repo.join("crates/cli");
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

    // Editing your own config is an ordinary thing to do — `pi -C ~/.pi` —
    // and both files now answer to one name, so the walk up reaches the very
    // file the personal slot already took.
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
