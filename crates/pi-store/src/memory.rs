//! What pi has come to know across sessions, as plain markdown under
//! `~/.pi/memory/`: `<key>.md`, its name the project's path with `-` for
//! `/`, is one project's; every other `*.md` there is global. Distillation writes it; anyone may read or edit it by hand.
//! What the prompt makes of it is `pi-core`'s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::session::key_of;

/// The name an edit uses for the project's own file.
pub const PROJECT: &str = "project";

// Where distillation records how far it has read each session.
const MARKS: &str = ".distilled.json";
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
    /// distillation is shown and what the prompt carries. Empty ones are left out.
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

    /// Apply `edits` for the project rooted at `project`, returning how many
    /// changed something; one naming no usable file is skipped.
    pub fn apply(&self, project: &Path, edits: &[Edit]) -> std::io::Result<usize> {
        let mut changed: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        let mut dirty = std::collections::BTreeSet::new();
        let mut count = 0;
        for edit in edits {
            // `project.md` is how a model may well spell it; never a global file.
            let for_project = matches!(edit.file.as_str(), PROJECT | "project.md");
            let path = if for_project {
                self.project_path(project)
            } else if global_name(&edit.file) {
                self.dir.join(&edit.file)
            } else {
                tracing::warn!(target: "pi::memory", file = %edit.file, "not a memory file name, skipped");
                continue;
            };
            let lines = changed.entry(path.clone()).or_insert_with(|| {
                read(&path)
                    .map(|b| b.lines().map(str::to_string).collect())
                    .unwrap_or_else(|| header(for_project, project))
            });
            let mut did = false;
            if let Some(old) = edit.remove.as_deref().map(fact).filter(|o| !o.is_empty())
                && let Some(at) = lines.iter().position(|l| fact(l) == old)
            {
                lines.remove(at);
                did = true;
            }
            if let Some(new) = edit.add.as_deref().map(fact).filter(|n| !n.is_empty())
                && !lines.iter().any(|l| fact(l) == new)
            {
                lines.push(format!("- {new}"));
                did = true;
            }
            if did {
                count += 1;
                dirty.insert(path);
            }
        }
        for (path, lines) in changed.into_iter().filter(|(p, _)| dirty.contains(p)) {
            // Down to its headings, a file remembers nothing; it goes.
            if lines
                .iter()
                .all(|l| l.trim().is_empty() || l.starts_with('#'))
            {
                let _ = std::fs::remove_file(&path);
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            tool::state::write_private(&path, format!("{}\n", lines.join("\n")).as_bytes())?;
        }
        Ok(count)
    }

    /// How far each session has been read, by session id: the last entry id
    /// distilled. `None` before the first distillation, or when unreadable:
    /// either way the archive behind is not read again.
    pub fn marks(&self) -> Option<BTreeMap<String, u64>> {
        let body = std::fs::read(self.dir.join(MARKS)).ok()?;
        serde_json::from_slice(&body).ok()
    }

    pub fn save_marks(&self, marks: &BTreeMap<String, u64>) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let body = serde_json::to_vec_pretty(marks).unwrap_or_default();
        tool::state::write_private(&self.dir.join(MARKS), &body)
    }

    /// Hold memory for one distillation, so two runs started together don't
    /// both read the same sessions. `None` while another holds it.
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

/// Edits out of a model's reply: the JSON array in it, whatever surrounds it.
pub fn parse(reply: &str) -> Result<Vec<Edit>, String> {
    let (Some(start), Some(end)) = (reply.find('['), reply.rfind(']')) else {
        return Err("no JSON array in the reply".into());
    };
    if end < start {
        return Err("no JSON array in the reply".into());
    }
    serde_json::from_str(&reply[start..=end]).map_err(|e| e.to_string())
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

    // Applied silently, read by nobody until the prompt carries it: a wrong
    // line here is one the user never sees go in.
    #[test]
    fn edits_add_once_replace_in_place_and_stay_inside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        let project = Path::new("/work/pi");

        let n = memory
            .apply(
                project,
                &[
                    edit("user.md", None, Some("prefers Rust")),
                    edit("user.md", None, Some("- prefers Rust")),
                    edit(PROJECT, None, Some("ships with cargo fmt")),
                    edit("../escape.md", None, Some("x")),
                    edit("Notes.md", None, Some("x")),
                    edit("project.md", None, Some("keeps a CHANGELOG")),
                ],
            )
            .unwrap();
        assert_eq!(n, 3);
        assert!(
            !dir.path().join("project.md").exists(),
            "a project line went global"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("user.md")).unwrap(),
            "- prefers Rust\n"
        );
        assert!(!dir.path().join("Notes.md").exists());
        assert!(!dir.path().parent().unwrap().join("escape.md").exists());

        memory
            .apply(
                project,
                &[edit(
                    "user.md",
                    Some("prefers Rust"),
                    Some("prefers Rust, then Python"),
                )],
            )
            .unwrap();
        let files = memory.files(project);
        assert_eq!(files[0].name, PROJECT);
        assert_eq!(files[1].body, "- prefers Rust, then Python\n");
        assert!(
            files[0].body.starts_with("# /work/pi\n"),
            "{}",
            files[0].body
        );

        memory
            .apply(
                project,
                &[
                    edit(PROJECT, Some("ships with cargo fmt"), None),
                    edit(PROJECT, Some("keeps a CHANGELOG"), None),
                ],
            )
            .unwrap();
        assert!(
            !memory.project_path(project).exists(),
            "a heading alone remembers nothing"
        );
    }

    #[test]
    fn the_edits_are_read_out_of_whatever_wraps_them() {
        let reply = "Here you go:\n```json\n[{\"file\": \"user.md\", \"add\": \"a\"}]\n```";
        assert_eq!(
            parse(reply).unwrap(),
            vec![edit("user.md", None, Some("a"))]
        );
        assert_eq!(parse("[]").unwrap(), vec![]);
        assert!(parse("nothing to change").is_err());
    }

    #[test]
    fn a_dash_and_an_underscore_are_two_projects_and_garbled_marks_read_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::new(dir.path());
        assert_ne!(
            memory.project_path(Path::new("/w/my-project")),
            memory.project_path(Path::new("/w/my_project"))
        );
        std::fs::write(dir.path().join(MARKS), "{\"s1\": 3").unwrap();
        assert_eq!(memory.marks(), None);
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

        let skipped = memory
            .apply(here, &[edit("-w-there.md", None, Some("x"))])
            .unwrap();
        assert_eq!(skipped, 0, "another project's file is not a global name");
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
