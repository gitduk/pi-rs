//! What pi has come to know across sessions, as plain markdown under
//! `~/.pi/memory/`: `<key>.md`, its name the project's path with `-` for
//! `/`, is one project's; every other `*.md` there is global. The model writes it
//! through `plan`; anyone may read or edit it by hand.
//! What a prompt makes of it is the reader's.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::session::key_of;

/// The name an edit uses for the project's own file.
pub const PROJECT: &str = "project";

const LOCK: &str = ".lock";
// A lock older than this is a run that died holding it.
const STALE: Duration = Duration::from_secs(600);

/// One memory file: what an edit calls it, and what it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub name: String,
    pub body: String,
}

/// One change to memory. `remove` alone drops a line, `add` alone appends
/// one, both together replace.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Edit {
    pub file: String,
    #[serde(default)]
    pub remove: Option<String>,
    #[serde(default)]
    pub add: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Memory {
    dir: PathBuf,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new(
            crate::dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("memory"),
        )
    }
}

impl Memory {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The project file for the checkout rooted at `project`.
    pub fn project_path(&self, project: &Path) -> PathBuf {
        self.dir.join(format!("{}.md", key_of(project)))
    }

    /// Move project files up out of `projects/`, where they used to live,
    /// unless one of the same name is already there.
    pub fn lift_projects(&self) {
        let old = self.dir.join("projects");
        let Ok(entries) = std::fs::read_dir(&old) else {
            return;
        };
        for entry in entries.flatten() {
            let to = self.dir.join(entry.file_name());
            if !to.exists() {
                let _ = std::fs::rename(entry.path(), to);
            }
        }
        let _ = std::fs::remove_dir(&old);
    }

    /// Where the file `files` calls `name` lives.
    pub fn path_of(&self, name: &str, project: &Path) -> PathBuf {
        if name == PROJECT {
            self.project_path(project)
        } else {
            self.dir.join(name)
        }
    }

    /// Every file `files` would read, whether or not it holds anything.
    pub fn paths(&self, project: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(global_name)
            })
            .collect();
        out.sort();
        out.push(self.project_path(project));
        out
    }

    /// The project's file, then every global one by name: what a
    /// change is weighed against and what the prompt carries. Empty ones are left out.
    pub fn files(&self, project: &Path) -> Vec<File> {
        let mut out = Vec::new();
        if let Some(body) = read(&self.project_path(project)) {
            out.push(File {
                name: PROJECT.into(),
                body,
            });
        }
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(global_name)
                })
                .collect();
            paths.sort();
            for path in paths {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if let Some(body) = read(&path) {
                    out.push(File {
                        name: name.to_string(),
                        body,
                    });
                }
            }
        }
        out
    }

    /// What `edits` would do for the project rooted at `project`, nothing
    /// written yet: each in turn sees the ones before it.
    pub fn plan(&self, project: &Path, edits: &[Edit]) -> Plan {
        let before: usize = self.files(project).iter().map(|f| f.body.len()).sum();
        let mut plan = Plan {
            done: Vec::new(),
            missed: Vec::new(),
            before,
            after: before,
            files: BTreeMap::new(),
        };
        for edit in edits {
            // `project.md` is how a model may well spell it; never a global file.
            let for_project = matches!(edit.file.as_str(), PROJECT | "project.md");
            let path = if for_project {
                self.project_path(project)
            } else if global_name(&edit.file) {
                self.dir.join(&edit.file)
            } else {
                plan.missed.push(format!(
                    "`{}` is not a memory file: `{PROJECT}`, or a lowercase `<topic>.md`",
                    edit.file
                ));
                continue;
            };
            let name = if for_project { PROJECT } else { &edit.file };
            let draft = plan.files.entry(path.clone()).or_insert_with(|| {
                let body = read(&path);
                Draft {
                    changed: false,
                    was: body.as_ref().map_or(0, String::len),
                    lines: body
                        .map(|b| b.lines().map(str::to_string).collect())
                        .unwrap_or_else(|| header(for_project, project)),
                }
            });
            let old = edit.remove.as_deref().map(fact).filter(|o| !o.is_empty());
            let new = edit.add.as_deref().map(fact).filter(|n| !n.is_empty());
            if old.is_none() && new.is_none() {
                plan.missed
                    .push(format!("{name}: nothing to add or remove"));
                continue;
            }
            if let Some(old) = old {
                match draft.lines.iter().position(|l| fact(l) == old) {
                    Some(at) => {
                        draft.lines.remove(at);
                        draft.changed = true;
                        plan.done.push(format!("{name}: - {old}"));
                    }
                    None => {
                        plan.missed.push(format!("{name}: no line reads `{old}`"));
                        continue;
                    }
                }
            }
            if let Some(new) = new {
                if draft.lines.iter().any(|l| fact(l) == new) {
                    plan.missed.push(format!("{name}: already has `{new}`"));
                } else {
                    draft.lines.push(format!("- {new}"));
                    draft.changed = true;
                    plan.done.push(format!("{name}: + {new}"));
                }
            }
        }
        plan.after = plan
            .files
            .values()
            .filter(|d| d.changed)
            .fold(before, |total, d| {
                total - d.was + d.body().map_or(0, |b| b.len())
            });
        plan
    }

    /// Hold memory for one change, so two pi writing at once don't each
    /// write over the other's. `None` while another holds it.
    pub fn lock(&self) -> Option<Lock> {
        std::fs::create_dir_all(&self.dir).ok()?;
        let path = self.dir.join(LOCK);
        if crate::older_than(&path, STALE) {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()
            .map(|_| Lock(path))
    }
}

/// Held memory; let go on drop.
pub struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Edits worked out but not yet written; `commit` writes them.
pub struct Plan {
    /// Each change made, as `<file>: - line` or `<file>: + line`.
    pub done: Vec<String>,
    /// Each edit that changed nothing, and why.
    pub missed: Vec<String>,
    /// Bytes across every file `files` reads, before and after.
    pub before: usize,
    pub after: usize,
    files: BTreeMap<PathBuf, Draft>,
}

struct Draft {
    // Untouched, a file is left as it stands rather than written back.
    changed: bool,
    was: usize,
    lines: Vec<String>,
}

impl Draft {
    // Down to its headings, a file remembers nothing; it goes.
    fn body(&self) -> Option<String> {
        let empty = self
            .lines
            .iter()
            .all(|l| l.trim().is_empty() || l.starts_with('#'));
        (!empty).then(|| format!("{}\n", self.lines.join("\n")))
    }
}

impl Plan {
    pub fn commit(&self) -> std::io::Result<()> {
        for (path, draft) in self.files.iter().filter(|(_, d)| d.changed) {
            match draft.body() {
                Some(body) => {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    tool::state::write_private(path, body.as_bytes())?;
                }
                None if draft.was > 0 => std::fs::remove_file(path)?,
                None => {}
            }
        }
        Ok(())
    }
}

// A global file's name: lowercase words, `.md`. Anything else could leave
// the directory or hide among files that aren't memory.
fn global_name(name: &str) -> bool {
    name.strip_suffix(".md").is_some_and(|stem| {
        // A project's file starts with the `-` its path's leading `/` became.
        !stem.is_empty()
            && !stem.starts_with('-')
            && stem != PROJECT
            && stem
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    })
}

// A line as the fact it states, so `- x` and `x` are the same memory.
fn fact(line: &str) -> String {
    let line = line.trim();
    line.strip_prefix("- ").unwrap_or(line).trim().to_string()
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|b| !b.trim().is_empty())
}

// A new file's first line, so a reader can tell whose it is.
fn header(project_file: bool, project: &Path) -> Vec<String> {
    if project_file {
        vec![format!("# {}", project.display()), String::new()]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(file: &str, remove: Option<&str>, add: Option<&str>) -> Edit {
        Edit {
            file: file.into(),
            remove: remove.map(Into::into),
            add: add.map(Into::into),
        }
    }

    // What the model is told back is the only check it has that a line went
    // where it meant, and the sizes are what a budget is weighed by.
    #[test]
    fn edits_add_once_replace_in_place_and_stay_inside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        let project = Path::new("/work/pi");

        let plan = memory.plan(
            project,
            &[
                edit("user.md", None, Some("prefers Rust")),
                edit("user.md", None, Some("- prefers Rust")),
                edit(PROJECT, None, Some("ships with cargo fmt")),
                edit("../escape.md", None, Some("x")),
                edit("Notes.md", None, Some("x")),
                edit("project.md", None, Some("keeps a CHANGELOG")),
                edit("user.md", Some("never said"), Some("y")),
            ],
        );
        assert_eq!(
            plan.done,
            [
                "user.md: + prefers Rust",
                "project: + ships with cargo fmt",
                "project: + keeps a CHANGELOG",
            ]
        );
        assert_eq!(plan.missed.len(), 4, "{:?}", plan.missed);
        assert!(plan.missed[3].contains("never said"), "{:?}", plan.missed);
        assert_eq!(plan.before, 0);
        assert!(memory.files(project).is_empty(), "a plan writes nothing");
        let after = plan.after;
        plan.commit().unwrap();
        let files = memory.files(project);
        assert_eq!(after, files.iter().map(|f| f.body.len()).sum::<usize>());
        assert!(
            !dir.path().join("project.md").exists(),
            "a project line went global"
        );
        assert!(!dir.path().join("Notes.md").exists());
        assert!(!dir.path().parent().unwrap().join("escape.md").exists());

        let plan = memory.plan(
            project,
            &[edit(
                "user.md",
                Some("prefers Rust"),
                Some("prefers Rust, then Python"),
            )],
        );
        assert_eq!(plan.before, after);
        plan.commit().unwrap();
        let files = memory.files(project);
        assert_eq!(files[0].name, PROJECT);
        assert_eq!(files[1].body, "- prefers Rust, then Python\n");
        assert!(
            files[0].body.starts_with("# /work/pi\n"),
            "{}",
            files[0].body
        );

        let plan = memory.plan(
            project,
            &[
                edit(PROJECT, Some("ships with cargo fmt"), None),
                edit(PROJECT, Some("keeps a CHANGELOG"), None),
            ],
        );
        assert_eq!(plan.after, files[1].body.len());
        plan.commit().unwrap();
        assert!(
            !memory.project_path(project).exists(),
            "a heading alone remembers nothing"
        );
    }

    // Rewritten, a file a change only missed would come back reformatted.
    #[test]
    fn a_file_only_missed_is_left_as_it_stands() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        let project = Path::new("/work/pi");
        std::fs::write(memory.project_path(project), "- kept as typed").unwrap();
        let plan = memory.plan(
            project,
            &[
                edit("user.md", None, Some("prefers Rust")),
                edit(PROJECT, Some("never said"), None),
            ],
        );
        assert_eq!(plan.after, plan.before + "- prefers Rust\n".len());
        plan.commit().unwrap();
        assert_eq!(
            std::fs::read_to_string(memory.project_path(project)).unwrap(),
            "- kept as typed"
        );
    }

    #[test]
    fn a_dash_and_an_underscore_are_two_projects() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        assert_ne!(
            memory.project_path(Path::new("/w/my-project")),
            memory.project_path(Path::new("/w/my_project"))
        );
    }

    // Side by side in one directory, a project's file is never read as
    // global, and one left in the old `projects/` comes up beside them.
    #[test]
    fn project_files_sit_with_the_global_ones_and_never_pass_for_them() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        let here = Path::new("/w/here");
        std::fs::create_dir(dir.path().join("projects")).unwrap();
        std::fs::write(dir.path().join("projects/-w-here.md"), "- here\n").unwrap();
        std::fs::write(dir.path().join("-w-there.md"), "- there\n").unwrap();
        std::fs::write(dir.path().join("user.md"), "- user\n").unwrap();

        memory.lift_projects();
        assert!(!dir.path().join("projects").exists());
        let names: Vec<String> = memory.files(here).into_iter().map(|f| f.name).collect();
        assert_eq!(names, [PROJECT, "user.md"]);
        assert_eq!(memory.files(here)[0].body, "- here\n");

        let plan = memory.plan(here, &[edit("-w-there.md", None, Some("x"))]);
        assert!(
            plan.done.is_empty(),
            "another project's file is not a global name"
        );
    }

    #[test]
    fn a_second_holder_waits_for_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        let held = memory.lock().expect("free");
        assert!(memory.lock().is_none());
        drop(held);
        assert!(memory.lock().is_some());
    }
}
