use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Skill, body};
use tool::limit::{MAX_BYTES, over_limit};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

impl Load {
    /// The name the tool answers to.
    pub const NAME: &'static str = "skill";
}

const NAME: &str = Load::NAME;

#[derive(Deserialize)]
struct Args {
    name: String,
    // A file inside the skill's own directory, when the body points at one.
    #[serde(default)]
    file: Option<String>,
}

/// Loads a skill's instructions on demand.
///
/// Descriptions ride in this tool's own description, which every request
/// carries; the bodies do not. That split is the whole point — a dozen skills
/// cost a paragraph of context until one is actually needed.
pub struct Load {
    skills: Vec<Skill>,
    description: String,
}

impl Load {
    pub fn new(skills: Vec<Skill>) -> Self {
        let mut description = String::from(
            "Load a skill's instructions. \
             Follow those instructions after you load them. \
             Apply them only to the work that selected the skill. \
             Use a skill whenever its description matches the work you will do next. \
             The description only summarizes the skill. It does not replace the instructions. \
             If earlier instructions are missing or incomplete, load the skill again before continuing related work.\n\nAvailable:\n",
        );
        for s in &skills {
            description.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        Self {
            skills,
            description,
        }
    }

    fn find(&self, name: &str) -> Result<&Skill, ToolError> {
        self.skills.iter().find(|s| s.name == name).ok_or_else(|| {
            ToolError::Invalid(format!(
                "no skill named `{name}`. Available: {}",
                self.skills
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

// How many of a skill's own files are worth naming. Past this the list stops
// being a way in and starts being the message.
const CAP: usize = 40;

// Paths under `dir`, relative to it, excluding `SKILL.md` itself and hidden
// entries (`.git` and the like), in name order. True when more were left out
// past `CAP`.
fn sibling_files(dir: &std::path::Path) -> (Vec<String>, bool) {
    // Stops once one past the cap is found: enough to know the list is cut.
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        if depth > 3 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        // Sorted before any is taken, so which ones the cap keeps does not
        // hang on the order the filesystem lists them in.
        let mut entries: Vec<_> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            // Inside the loop, not only at each descent: a directory holding a
            // thousand files at one level would otherwise name every one of them.
            if out.len() > CAP {
                return;
            }
            let path = entry.path();
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(&path, base, out, depth + 1);
            } else if path.file_name().is_some_and(|n| n != "SKILL.md")
                && let Ok(rel) = path.strip_prefix(base)
            {
                out.push(rel.display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out, 0);
    let cut = out.len() > CAP;
    out.truncate(CAP);
    out.sort();
    (out, cut)
}

/// A skill's body, with the way to reach the files it points at.
///
/// Shared with a surface that runs a skill at the user's word: the model is
/// given the same instructions either way, and the skill tool is how a skill's
/// own files are reached.
pub fn instructions(skill: &Skill, text: &str) -> String {
    let source = match skill.builtin {
        Some(_) => "built-in".to_string(),
        None => skill.dir.join("SKILL.md").display().to_string(),
    };
    let mut out = format!(
        "Skill: `{}`\nSource: {source}\n\n{}",
        skill.name,
        body(text)
    );
    // Skills usually live outside the workspace; instructions that point at a
    // sibling are useless unless the way to fetch it arrives with them.
    let (siblings, cut) = if skill.builtin.is_some() {
        let names = skill.builtin_files.iter().map(|(name, _)| name.clone());
        (names.collect(), false)
    } else {
        sibling_files(&skill.dir)
    };
    if !siblings.is_empty() {
        let mut listed: Vec<String> = siblings.iter().map(|name| format!("- `{name}`")).collect();
        if cut {
            listed.push(format!("- (only the first {CAP} are listed)"));
        }
        out.push_str(&format!(
            "\n---\nFiles in this skill:\n{}\n\nRead a file with \
             `skill(name: \"{}\", file: \"<path>\")`.\n",
            listed.join("\n"),
            skill.name
        ));
    }
    out
}

#[async_trait]
impl Tool for Load {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        let names: Vec<&str> = self.skills.iter().map(|s| s.name.as_str()).collect();
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Specify a skill name from the list above.",
                    "enum": names,
                },
                "file": {
                    "type": "string",
                    "description": "Read a file referenced by the skill's instructions. Specify its path within the skill.",
                },
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, args: Value, _ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tool::parse_args(args)?;
        let skill = self.find(&args.name)?;

        if let Some(text) = skill.builtin {
            return match args.file {
                Some(rel) => match skill.builtin_files.iter().find(|(name, _)| name == &rel) {
                    Some((_, text)) => {
                        Ok(ToolOutput::text(*text).with_preview(format!("{}/{rel}", args.name)))
                    }
                    None if skill.builtin_files.is_empty() => Err(ToolError::Invalid(format!(
                        "{}/{rel}: a built-in skill has no files",
                        args.name
                    ))),
                    None => Err(ToolError::Invalid(format!(
                        "{}/{rel}: no such built-in file. Available: {}",
                        args.name,
                        skill
                            .builtin_files
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))),
                },
                None => Ok(
                    ToolOutput::text(instructions(skill, text)).with_preview(skill.name.clone())
                ),
            };
        }
        let Some(rel) = args.file else {
            // The instructions go to the transcript whole; a multi-megabyte
            // SKILL.md is refused, not read.
            let path = skill.dir.join("SKILL.md");
            if let Ok(meta) = tokio::fs::metadata(&path).await
                && meta.len() > MAX_BYTES
            {
                return Ok(ToolOutput::text(over_limit(
                    &format!("{}: SKILL.md", args.name),
                    meta.len(),
                )));
            }
            let text = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| ToolError::Invalid(format!("{}: {e}", args.name)))?;
            let out = instructions(skill, &text);
            return Ok(ToolOutput::text(out).with_preview(skill.name.clone()));
        };

        // A file argument stays inside the skill's own directory; that is the
        // boundary, not the workspace.
        let target = skill.dir.join(&rel);
        let real = target
            .canonicalize()
            .map_err(|e| ToolError::Invalid(format!("{}/{rel}: {e}", args.name)))?;
        let root = skill
            .dir
            .canonicalize()
            .map_err(|e| ToolError::Invalid(format!("{}: {e}", args.name)))?;
        if !real.starts_with(&root) {
            return Err(ToolError::Escape(format!("{}/{rel}", args.name)));
        }

        // Same ceiling as SKILL.md: a referenced file rides the transcript too.
        if let Ok(meta) = tokio::fs::metadata(&real).await
            && meta.len() > MAX_BYTES
        {
            return Ok(ToolOutput::text(over_limit(
                &format!("{}/{rel}", args.name),
                meta.len(),
            )));
        }
        let text = tokio::fs::read_to_string(&real).await?;
        Ok(ToolOutput::text(text).with_preview(format!("{}/{rel}", skill.name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hidden entries are not the skill's files, and which files a long list
    // keeps must not depend on the filesystem's order.
    #[test]
    fn hidden_entries_are_skipped_and_a_cut_list_says_so() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("SKILL.md"), "").unwrap();
        std::fs::write(dir.path().join("a.md"), "").unwrap();
        assert_eq!(sibling_files(dir.path()), (vec!["a.md".to_string()], false));

        for n in 0..CAP + 5 {
            std::fs::write(dir.path().join(format!("f{n:03}.txt")), "").unwrap();
        }
        let (files, cut) = sibling_files(dir.path());
        assert!(cut);
        assert_eq!(files.len(), CAP);
        assert_eq!(files[0], "a.md");
        assert_eq!(files[CAP - 1], format!("f{:03}.txt", CAP - 2));
    }

    #[test]
    fn exactly_cap_files_is_not_a_cut_list() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..CAP {
            std::fs::write(dir.path().join(format!("f{n:03}.txt")), "").unwrap();
        }
        let (files, cut) = sibling_files(dir.path());
        assert_eq!((files.len(), cut), (CAP, false));
    }

    // Three levels of subdirectory are listed; a fourth is not.
    #[test]
    fn files_are_listed_three_directories_deep_and_no_deeper() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c/d")).unwrap();
        std::fs::write(dir.path().join("a/b/c/three.md"), "").unwrap();
        std::fs::write(dir.path().join("a/b/c/d/four.md"), "").unwrap();
        assert_eq!(
            sibling_files(dir.path()),
            (vec!["a/b/c/three.md".to_string()], false)
        );
    }
}
