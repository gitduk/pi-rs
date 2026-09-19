use crate::read::{MAX_BYTES, over_limit};
use brain::slice::head_bytes;
use serde::Deserialize;
use std::path::{Path, PathBuf};

// One line in the tool catalog: past this, the description costs more than
// the decision it buys.
const DESCRIPTION_LIMIT: usize = 1_000;

/// A directory holding `SKILL.md` and whatever files it references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

// A name that cannot leave the skills directory it was found in.
fn usable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[derive(Deserialize)]
struct Header {
    name: Option<String>,
    description: Option<String>,
}

/// Read `name` and `description` out of a `---` fenced header.
///
/// The header goes through serde_yaml_ng, so quoting, comments and block
/// scalars come out the way real YAML resolves them. Text with no header at
/// all is `Ok((None, None))`; a header that will not parse is an `Err`,
/// reported rather than silently misread.
pub fn frontmatter(text: &str) -> Result<(Option<String>, Option<String>), String> {
    let Some(rest) = text.strip_prefix("---") else {
        return Ok((None, None));
    };
    let Some(end) = rest.find("\n---") else {
        return Ok((None, None));
    };
    let header = rest[..end].trim();
    if header.is_empty() {
        return Ok((None, None));
    }
    let Header { name, description } =
        serde_yaml_ng::from_str(header).map_err(|e| format!("not valid skill frontmatter: {e}"))?;
    Ok((name, description))
}

/// Everything below the frontmatter.
pub fn body(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---") else {
        return text;
    };
    match rest.find("\n---") {
        Some(end) => rest[end + 4..].trim_start_matches(['\n', '\r']),
        None => text,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// `.agents/skills` here and in every ancestor up to the repository root.
//
// A monorepo keeps shared skills at the top while the work happens several
// directories below, so stopping at the workspace would hide them. The walk
// ends at the repository root and never reaches `$HOME`, whose `.agents` is
// the personal one and is added separately.
fn ancestral_agents(workspace: &Path) -> Vec<PathBuf> {
    let home = home();
    let mut out = Vec::new();
    for dir in workspace.ancestors() {
        if home.as_deref() == Some(dir) {
            break;
        }
        out.push(dir.join(".agents/skills"));
        if dir.join(".git").exists() {
            break;
        }
    }
    out
}

/// Where skills come from, nearest first.
///
/// One name, `.agents/skills`, at both levels, and it is the shared standard
/// rather than ours. Supporting the vendor-neutral location is what makes a
/// shared skill shared; carrying a private name beside it — anyone's, ours
/// included — only leaves the question of where a skill belongs permanently
/// open. A directory under some other name reaches the list by being symlinked
/// into this one, which is the same mechanism said out loud.
pub fn sources(workspace: &Path) -> Vec<PathBuf> {
    let mut out = ancestral_agents(workspace);
    out.extend(home().map(|h| h.join(".agents/skills")));
    out
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
    // Discovery runs before the first turn; a giant SKILL.md is a problem to
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
    let (declared, description) = match frontmatter(&text) {
        Ok(pair) => pair,
        Err(why) => return Read::Problem(format!("{}: {why}", dir.display())),
    };
    let shown = dir.display();

    // The description is what the model decides on; without it the entry costs
    // context and can never be chosen. Pi refuses these too.
    let Some(description) = description.filter(|d| !d.trim().is_empty()) else {
        return Read::Problem(format!("{shown}: SKILL.md has no description"));
    };

    // One skill per line in the tool catalog and the help list.
    // Capped: the catalog rides every request, so one poisoned description
    // must not tax every turn until the skill is deleted.
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
    Read::Skill(Box::new(Skill {
        name,
        description,
        dir: dir.to_path_buf(),
    }))
}

/// Skills, and what could not be read.
#[derive(Debug, Default)]
pub struct Found {
    pub skills: Vec<Skill>,
    pub problems: Vec<String>,
}

/// Every skill reachable from `workspace`, sorted by name.
pub fn discover(workspace: &Path) -> Found {
    discover_from(&sources(workspace))
}

// How far below a source directory a skill may sit.
//
// Skill collections group by category, so the top level is not always where
// they are. A bound keeps a stray symlink or a `node_modules` from turning
// discovery into a full filesystem walk.
const MAX_DEPTH: usize = 3;

/// The same, over explicit directories. Taking them as an argument keeps the
/// environment out of the call, which is what lets tests run in parallel.
///
/// A nearer source wins a name collision, so a project can shadow a personal
/// skill.
pub fn discover_from(sources: &[PathBuf]) -> Found {
    let mut found = Found::default();
    for source in sources {
        walk(source, MAX_DEPTH, &mut found);
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
    let mut kids: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
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

    // A name addresses the skill's directory, so one that could leave it must
    // never load. Casing and hyphen style are left alone on purpose — a shared
    // `.agents/skills` holds skills written to other tools' rules, and
    // rejecting those makes them invisible for no gain.
    #[test]
    fn a_name_that_could_leave_the_directory_is_refused() {
        assert!(!usable("../escape"));
        assert!(!usable("a/b"));
        assert!(!usable(""));
        assert!(usable("first-principles"));
        assert!(usable("cc_clean2"));
    }
}
