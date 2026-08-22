use std::path::{Path, PathBuf};

/// A directory holding `SKILL.md` and whatever files it references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

/// A name that cannot leave the skills directory it was found in.
fn usable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Strip one layer of matching quotes, as a YAML scalar would carry.
fn unquote(value: &str) -> &str {
    let v = value.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return &v[1..v.len() - 1];
        }
    }
    v
}

/// Read `name` and `description` out of a `---` fenced header.
///
/// A deliberate subset of YAML: single-line `key: value`, optionally quoted.
/// Real skill files use nothing more, and a YAML dependency to parse two
/// strings is a poor trade.
pub fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = text.strip_prefix("---") else {
        return (None, None);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, None);
    };

    let (mut name, mut description) = (None, None);
    for line in rest[..end].lines() {
        // Top level only: `metadata:` carries a nested mapping, and a `name:`
        // indented under it describes the metadata, not the skill.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "name" => name = Some(unquote(value).to_string()),
            "description" => description = Some(unquote(value).to_string()),
            _ => {}
        }
    }
    (name, description)
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

/// `.agents/skills` here and in every ancestor up to the repository root.
///
/// A monorepo keeps shared skills at the top while the work happens several
/// directories below, so stopping at the workspace would hide them. The walk
/// ends at the repository root and never reaches `$HOME`, whose `.agents` is
/// the personal one and is added separately.
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
/// Two names only: `.pi/skills`, which is ours, and `.agents/skills`, which is
/// the shared standard. Another harness's private directory is deliberately not
/// on the list — supporting the vendor-neutral location is what makes a shared
/// skill shared, and reading each vendor's own folder as well only spreads the
/// question of where a skill belongs. Anyone wanting one of those can point
/// `.agents/skills` at it with a symlink.
pub fn sources(workspace: &Path) -> Vec<PathBuf> {
    let mut out = vec![workspace.join(".pi/skills")];
    out.extend(ancestral_agents(workspace));
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")));
    if let Some(c) = config {
        out.push(c.join("pi/skills"));
    }
    if let Some(h) = home() {
        out.push(h.join(".agents/skills"));
    }
    out
}

/// What a skill directory turned out to be.
enum Read {
    Skill(Box<Skill>),
    /// Present but unusable, and worth saying so: a skill that silently fails
    /// to appear is one the user goes looking for in the wrong place.
    Problem(String),
    /// No SKILL.md here; keep descending.
    None,
}

fn read_one(dir: &Path) -> Read {
    let Ok(text) = std::fs::read_to_string(dir.join("SKILL.md")) else {
        return Read::None;
    };
    let (declared, description) = frontmatter(&text);
    let shown = dir.display();

    // The description is what the model decides on; without it the entry costs
    // context and can never be chosen. Pi refuses these too.
    let Some(description) = description.filter(|d| !d.trim().is_empty()) else {
        return Read::Problem(format!("{shown}: SKILL.md has no description"));
    };
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

/// How far below a source directory a skill may sit.
///
/// Skill collections group by category, so the top level is not always where
/// they are. A bound keeps a stray symlink or a `node_modules` from turning
/// discovery into a full filesystem walk.
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

    const SKILL: &str = "---\nname: thinking\ndescription: \"Five models, one router.\"\n---\n\n# Thinking\n\nBody here.\n";

    #[test]
    fn a_header_gives_up_its_name_and_description() {
        let (name, description) = frontmatter(SKILL);
        assert_eq!(name.as_deref(), Some("thinking"));
        assert_eq!(description.as_deref(), Some("Five models, one router."));
        assert_eq!(body(SKILL), "# Thinking\n\nBody here.\n");
    }

    #[test]
    fn a_description_may_carry_the_other_quote_inside_it() {
        let text = "---\ndescription: \"Use when the user says 'go'.\"\n---\nx\n";
        assert_eq!(
            frontmatter(text).1.as_deref(),
            Some("Use when the user says 'go'.")
        );
    }

    #[test]
    fn a_file_with_no_header_is_all_body() {
        assert_eq!(frontmatter("# Plain\n"), (None, None));
        assert_eq!(body("# Plain\n"), "# Plain\n");
    }

    #[test]
    fn a_name_that_could_leave_the_directory_is_refused() {
        assert!(!usable("../escape"));
        assert!(!usable("a/b"));
        assert!(!usable(""));
        assert!(usable("first-principles"));
        assert!(usable("cc_clean2"));
    }

    #[test]
    fn discovery_prefers_the_nearer_source_and_sorts_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let near = tmp.path().join("near");
        let far = tmp.path().join("far");

        for (root, body) in [
            (near.join("shared"), "project version"),
            (far.join("shared"), "personal version"),
            (far.join("only-personal"), "z"),
        ] {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("SKILL.md"),
                format!("---\ndescription: {body}\n---\nx\n"),
            )
            .unwrap();
        }

        let found = discover_from(&[near, far]);
        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["only-personal", "shared"]);
        assert_eq!(
            found.skills[1].description, "project version",
            "a project skill shadows a personal one"
        );
    }

    fn skill_at(root: &Path, rel: &str, front: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("---\n{front}\n---\nbody\n")).unwrap();
    }

    #[test]
    fn a_skill_is_found_below_the_top_of_a_collection() {
        // Skill repositories group by category, so the top level is not always
        // where they are.
        let tmp = tempfile::tempdir().unwrap();
        skill_at(tmp.path(), "writing/commit", "description: Write a commit.");
        let found = discover_from(&[tmp.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].name, "commit");
    }

    #[test]
    fn a_skills_own_subdirectories_are_not_more_skills() {
        // scripts/ and references/ belong to the skill; descending into them
        // would rediscover its own fragments.
        let tmp = tempfile::tempdir().unwrap();
        skill_at(tmp.path(), "archify", "description: Draw diagrams.");
        skill_at(tmp.path(), "archify/recipes", "description: Not a skill.");
        let found = discover_from(&[tmp.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 1, "{:?}", found.skills);
    }

    #[test]
    fn what_cannot_be_loaded_is_reported_rather_than_dropped() {
        // Silently vanishing sends the user looking in the wrong place.
        let tmp = tempfile::tempdir().unwrap();
        skill_at(tmp.path(), "nameless", "license: MIT");
        // Unsafe rather than merely non-standard: a name is used to address the
        // skill, so one that can leave its directory must not load. Casing and
        // hyphen style are left alone on purpose — a shared `.agents/skills`
        // holds skills written to other tools' rules, and rejecting those makes
        // them invisible for no gain.
        skill_at(tmp.path(), "escapee", "name: ../../etc\ndescription: x");
        let found = discover_from(&[tmp.path().to_path_buf()]);
        assert!(found.skills.is_empty());
        assert_eq!(found.problems.len(), 2, "{:?}", found.problems);
        assert!(found.problems.iter().any(|p| p.contains("no description")));
        assert!(
            found
                .problems
                .iter()
                .any(|p| p.contains("usable skill name"))
        );
    }

    #[test]
    fn a_shadowed_skill_says_which_one_won() {
        let tmp = tempfile::tempdir().unwrap();
        let (near, far) = (tmp.path().join("a"), tmp.path().join("b"));
        skill_at(&near, "commit", "description: near");
        skill_at(&far, "commit", "description: far");
        let found = discover_from(&[near, far]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].description, "near");
        assert!(found.problems[0].contains("already defined by"));
    }

    #[test]
    fn a_nested_metadata_key_does_not_rename_the_skill() {
        // `metadata:` carries its own mapping, and a `name:` indented under it
        // describes the metadata rather than the skill.
        let text = "---\nname: archify\nmetadata:\n  name: not-this\n                      description: nor this\ndescription: Draw diagrams.\n---\nbody\n";
        let (name, description) = frontmatter(text);
        assert_eq!(name.as_deref(), Some("archify"));
        assert_eq!(description.as_deref(), Some("Draw diagrams."));
    }

    #[test]
    fn the_agents_walk_climbs_to_the_repository_root_and_no_further() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("packages/web");
        std::fs::create_dir_all(&deep).unwrap();
        let dirs = ancestral_agents(&deep);
        assert_eq!(
            dirs,
            vec![
                deep.join(".agents/skills"),
                repo.join("packages/.agents/skills"),
                repo.join(".agents/skills"),
            ],
            "the walk must stop at the repository root"
        );
    }
}
