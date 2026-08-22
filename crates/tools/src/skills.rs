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

/// Where skills come from, nearest first.
///
/// Project directories outrank personal ones, and `.claude/skills` is read as
/// it stands: a skill already written for another tool is a skill.
pub fn sources(workspace: &Path) -> Vec<PathBuf> {
    let mut out = vec![
        workspace.join(".pir/skills"),
        workspace.join(".claude/skills"),
    ];
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config")));
    if let Some(c) = config {
        out.push(c.join("pir/skills"));
    }
    if let Some(h) = home() {
        out.push(h.join(".claude/skills"));
    }
    out
}

fn read_one(dir: &Path) -> Option<Skill> {
    let text = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let (declared, description) = frontmatter(&text);
    let name = declared
        .filter(|n| usable(n))
        .or_else(|| dir.file_name()?.to_str().map(str::to_string))
        .filter(|n| usable(n))?;
    Some(Skill {
        name,
        description: description.unwrap_or_else(|| "(no description)".into()),
        dir: dir.to_path_buf(),
    })
}

/// Every skill reachable from `workspace`, sorted by name.
pub fn discover(workspace: &Path) -> Vec<Skill> {
    discover_from(&sources(workspace))
}

/// The same, over explicit directories. Taking them as an argument keeps the
/// environment out of the call, which is what lets tests run in parallel.
///
/// A nearer source wins a name collision, so a project can shadow a personal
/// skill.
pub fn discover_from(sources: &[PathBuf]) -> Vec<Skill> {
    let mut found: Vec<Skill> = Vec::new();
    for source in sources {
        let Ok(entries) = std::fs::read_dir(source) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Some(skill) = read_one(&entry.path()) else {
                continue;
            };
            if !found.iter().any(|s| s.name == skill.name) {
                found.push(skill);
            }
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
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
        let names: Vec<&str> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["only-personal", "shared"]);
        assert_eq!(
            found[1].description, "project version",
            "a project skill shadows a personal one"
        );
    }
}
