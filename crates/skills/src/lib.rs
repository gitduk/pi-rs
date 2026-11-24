mod load;
mod paths;

pub use load::{Load, delivered, delivery, included, source, source_name};
pub use paths::Paths;

use llm::slice::head_bytes;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tool::limit::{MAX_BYTES, over_limit};
use tool::stamp::{Stamp, Stamped};

// One line in the tool catalog: past this, the description costs more than
// the decision it buys.
const DESCRIPTION_LIMIT: usize = 1_000;

/// A directory holding `SKILL.md` and whatever files it references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub paths: Paths,
    /// The whole `SKILL.md` when it is compiled in rather than kept in `dir`.
    pub builtin: Option<&'static str>,
    /// Referenced files compiled into a built-in skill, addressed by relative name.
    pub builtin_files: Vec<(String, &'static str)>,
}

impl Skill {
    /// A skill compiled into the binary, from its whole `SKILL.md`, which
    /// must name it; `None` when the header gives no usable name or description.
    pub fn builtin(text: &'static str) -> Option<Self> {
        let Header {
            name: Some(name),
            description: Some(description),
            paths,
        } = header(text).ok()?
        else {
            return None;
        };
        usable(&name).then(|| Self {
            name,
            description,
            dir: PathBuf::new(),
            paths,
            builtin: Some(text),
            builtin_files: Vec::new(),
        })
    }

    pub fn with_files(mut self, files: Vec<(String, &'static str)>) -> Self {
        self.builtin_files = files;
        self
    }

    /// Its `SKILL.md` as it stands.
    pub fn text(&self) -> std::io::Result<String> {
        match self.builtin {
            Some(text) => Ok(text.to_string()),
            None => std::fs::read_to_string(self.dir.join("SKILL.md")),
        }
    }

    /// Load the complete instructions and count the delivery.
    ///
    /// # Errors
    /// Returns an error if the instructions cannot be read or delivered whole.
    pub async fn instructions(&self) -> Result<String, tool::ToolError> {
        let text = self.read_instructions().await?;
        self.record_delivery(text.len()).await;
        Ok(text)
    }

    /// Prepare complete instructions without counting a delivery.
    ///
    /// # Errors
    /// Returns an error if the instructions cannot be read or delivered whole.
    pub async fn read_instructions(&self) -> Result<String, tool::ToolError> {
        let skill = self.clone();
        tokio::task::spawn_blocking(move || skill.compose_instructions())
            .await
            .map_err(|error| tool::ToolError::Invalid(format!("{}: {error}", self.name)))?
    }

    /// Count instructions accepted for delivery.
    pub async fn record_delivery(&self, bytes: usize) {
        let name = self.name.clone();
        let _ = tokio::task::spawn_blocking(move || load::count(&name, bytes)).await;
    }

    fn compose_instructions(&self) -> Result<String, tool::ToolError> {
        let text;
        let raw = match self.builtin {
            Some(raw) => raw,
            None => {
                let path = self.dir.join("SKILL.md");
                let failed =
                    |error| tool::ToolError::Invalid(format!("{}: {error}", path.display()));
                let meta = std::fs::metadata(&path).map_err(failed)?;
                if !meta.is_file() {
                    return Err(tool::ToolError::Invalid(format!(
                        "{} is not a regular file",
                        path.display()
                    )));
                }
                if meta.len() > MAX_BYTES {
                    return Err(tool::ToolError::Invalid(over_limit(
                        &path.display().to_string(),
                        meta.len(),
                    )));
                }
                text = std::fs::read_to_string(&path).map_err(failed)?;
                &text
            }
        };
        load::compose_fit(self, raw).map_err(tool::ToolError::Invalid)
    }
}

// A name that cannot leave the skills directory it was found in.
fn usable(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(tool::name_char)
}

#[derive(Default, Deserialize)]
struct Header {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    paths: Paths,
}

/// Read `name` and `description` out of a `---` fenced header.
///
/// The header goes through serde_yaml_ng, so quoting, comments and block
/// scalars come out the way real YAML resolves them. Text with no header at
/// all is `Ok((None, None))`; a header that will not parse is an `Err`,
/// reported rather than silently misread.
pub fn frontmatter(text: &str) -> Result<(Option<String>, Option<String>), String> {
    let Header {
        name, description, ..
    } = header(text)?;
    Ok((name, description))
}

fn header(text: &str) -> Result<Header, String> {
    let Some((header, _)) = split(text).filter(|(header, _)| !header.is_empty()) else {
        return Ok(Header::default());
    };
    serde_yaml_ng::from_str(header).map_err(|e| format!("not valid skill frontmatter: {e}"))
}

/// Everything below the frontmatter.
pub fn body(text: &str) -> &str {
    split(text).map_or(text, |(_, body)| body)
}

fn split(text: &str) -> Option<(&str, &str)> {
    let (header, body) = text.strip_prefix("---")?.split_once("\n---")?;
    Some((header.trim(), body.trim_start_matches(['\n', '\r'])))
}

// What a skill directory turned out to be.
enum Read {
    Skill(Box<Skill>),
    // Present but unusable, and worth saying so: a skill that silently fails
    // to appear is one the user goes looking for in the wrong place.
    Problem(String),
    // No SKILL.md here; keep descending.
    None,
}

fn read_one(dir: &Path) -> Read {
    // Discovery runs before the first turn, and a giant SKILL.md is a problem to
    // report, not a file to hold.
    let path = dir.join("SKILL.md");
    if let Ok(meta) = std::fs::metadata(&path)
        && meta.len() > MAX_BYTES
    {
        return Read::Problem(over_limit(&path.display().to_string(), meta.len()));
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return Read::None;
    };
    let Header {
        name: declared,
        description,
        paths,
    } = match header(&text) {
        Ok(header) => header,
        Err(why) => return Read::Problem(format!("{}: {why}", dir.display())),
    };
    let shown = dir.display();

    // The description is what the model decides on; without it the entry costs
    // context and can never be chosen. Pi refuses these too.
    let Some(description) = description.filter(|d| !d.trim().is_empty()) else {
        return Read::Problem(format!("{shown}: SKILL.md has no description"));
    };

    // One line in the catalog and help list; capped since the catalog rides
    // every request, so one poisoned description can't tax every turn.
    let mut folded = String::new();
    for word in description.split_whitespace() {
        if !folded.is_empty() {
            if folded.len() + 1 + word.len() > DESCRIPTION_LIMIT {
                break;
            }
            folded.push(' ');
        } else if word.len() > DESCRIPTION_LIMIT {
            folded.push_str(head_bytes(word, DESCRIPTION_LIMIT));
            break;
        }
        folded.push_str(word);
    }
    let description = folded;

    let fallback = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let name = declared.unwrap_or_else(|| fallback.to_string());
    if !usable(&name) {
        return Read::Problem(format!(
            "{shown}: `{name}` is not a usable skill name (a-z, 0-9, - and _, up to 64)"
        ));
    }
    let skill = Skill {
        name,
        description,
        dir: dir.to_path_buf(),
        paths,
        builtin: None,
        builtin_files: Vec::new(),
    };
    if let Err(why) = load::compose_fit(&skill, &text) {
        return Read::Problem(why);
    }
    Read::Skill(Box::new(skill))
}

/// Skills, and what could not be read.
#[derive(Debug, Default)]
pub struct Found {
    pub skills: Vec<Skill>,
    pub problems: Vec<String>,
}

// How far below the skills directory a skill may sit — collections group by
// category, so a bound also keeps a stray symlink from a full filesystem walk.
const MAX_DEPTH: usize = 3;

/// The skills directories as a `tool::Source`: the `skill` tool as they now
/// stand, rebuilt only when a `SKILL.md` under one changed.
///
/// Directories are read in order and the first to claim a name keeps it, so
/// whoever passes them decides which of two skills with one name is the one
/// offered.
pub struct Shelf {
    dirs: Vec<PathBuf>,
    builtins: Vec<Skill>,
    seen: Stamped<Seen>,
    builds: AtomicU64,
}

struct Seen {
    found: Arc<Found>,
    tool: Option<Arc<dyn tool::Tool>>,
    generation: u64,
}

impl Shelf {
    pub fn new(dirs: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
        Self {
            dirs: dirs.into_iter().map(Into::into).collect(),
            builtins: Vec::new(),
            seen: Stamped::default(),
            builds: AtomicU64::new(0),
        }
    }

    /// Offer `skill` too, unless one on disk has its name.
    pub fn with_builtin(mut self, skill: Skill) -> Self {
        self.builtins.push(skill);
        self
    }

    /// The skills as they stand, and a number that moves when they change.
    pub fn now(&self) -> (Arc<Found>, u64) {
        self.fresh(|seen| (seen.found.clone(), seen.generation))
    }

    /// Load named instructions in order from the current catalog, including their sources.
    ///
    /// # Errors
    /// Returns an error if any name is missing, unreadable, or too large for one result.
    pub async fn preload(&self, names: &[String]) -> Result<Vec<String>, tool::ToolError> {
        let (found, _) = self.now();
        let selected = names
            .iter()
            .map(|name| {
                found
                    .skills
                    .iter()
                    .find(|skill| skill.name == *name)
                    .ok_or_else(|| {
                        tool::ToolError::Invalid(format!("no skill named `{name}` to preload"))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        // Every body is read and sized before any is counted: a call that fails
        // must not leave a count for instructions that never reached the model.
        let mut composed = Vec::with_capacity(selected.len());
        for skill in &selected {
            composed.push(skill.read_instructions().await?);
        }
        for (skill, out) in selected.iter().zip(&composed) {
            load::count(&skill.name, out.len());
        }
        Ok(composed)
    }

    fn fresh<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
        let mut stamps = Vec::new();
        for dir in &self.dirs {
            stamp(dir, MAX_DEPTH, &mut stamps);
        }
        self.seen.read(stamps, || self.build(), read)
    }

    fn build(&self) -> Seen {
        let mut found = discover(&self.dirs);
        for builtin in &self.builtins {
            if !found.skills.iter().any(|s| s.name == builtin.name) {
                found.skills.push(builtin.clone());
            }
        }
        found.skills.sort_by(|a, b| a.name.cmp(&b.name));
        Seen {
            tool: (!found.skills.is_empty())
                .then(|| Arc::new(Load::new(found.skills.clone())) as Arc<dyn tool::Tool>),
            found: Arc::new(found),
            generation: self.builds.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }
}

impl tool::Source for Shelf {
    fn tools(&self) -> Vec<Arc<dyn tool::Tool>> {
        self.fresh(|seen| seen.tool.clone().into_iter().collect())
    }
}

// Every `SKILL.md` `walk` would read, by path, time and size: what says a
// skill changed without parsing one.
fn stamp(dir: &Path, depth: usize, out: &mut Vec<Stamp>) {
    if let Some(found) = tool::stamp::of(dir.join("SKILL.md")) {
        out.push(found);
        return;
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut kids: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    kids.sort();
    for kid in kids {
        stamp(&kid, depth - 1, out);
    }
}

/// Every skill under `dirs`, read in order and sorted by name. A name an
/// earlier directory claimed keeps its skill; the later one is reported.
pub fn discover(dirs: &[PathBuf]) -> Found {
    let mut found = Found::default();
    for dir in dirs {
        walk(dir, MAX_DEPTH, &mut found);
    }
    found.skills.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Found) {
    match read_one(dir) {
        // A skill's own subdirectories are its scripts and references, not more
        // skills; descending into them would find its own fragments.
        Read::Skill(skill) => {
            if let Some(first) = found.skills.iter().find(|s| s.name == skill.name) {
                found.problems.push(format!(
                    "{}: `{}` is already defined by {}",
                    skill.dir.display(),
                    skill.name,
                    first.dir.display()
                ));
            } else {
                found.skills.push(*skill);
            }
            return;
        }
        Read::Problem(why) => {
            found.problems.push(why);
            return;
        }
        Read::None => {}
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // `Path::is_dir` follows links, so a skill kept elsewhere joins by symlink.
    let mut kids: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    // Stable order, so a collision resolves the same way on every machine.
    kids.sort();
    for kid in kids {
        walk(&kid, depth - 1, found);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Casing and hyphen style are left alone on purpose: skills are often
    // copied in from other tools, written to their rules.
    #[test]
    fn a_name_that_could_leave_the_directory_is_refused() {
        assert!(!usable("../escape"));
        assert!(!usable("a/b"));
        assert!(!usable(""));
        assert!(usable("first-principles"));
        assert!(usable("cc_clean2"));
    }
}
