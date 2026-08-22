use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::skills::{Skill, body};
use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

pub const NAME: &str = "skill";

#[derive(Deserialize)]
struct Args {
    name: String,
    /// A file inside the skill's own directory, when the body points at one.
    #[serde(default)]
    file: Option<String>,
}

/// Loads a skill's instructions on demand.
///
/// Descriptions ride in this tool's own description, which every request
/// carries; the bodies do not. That split is the whole point — a dozen skills
/// cost a paragraph of context until one is actually needed.
pub struct SkillTool {
    skills: Vec<Skill>,
    description: String,
}

impl SkillTool {
    pub fn new(skills: Vec<Skill>) -> Self {
        let mut description = String::from(
            "Load a skill's instructions and follow them. Use one whenever its \
             description matches what you are about to do — the description is a \
             summary, not the instructions.\n\nAvailable:\n",
        );
        for s in &skills {
            description.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        Self {
            skills,
            description,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    fn find(&self, name: &str) -> Result<&Skill, ToolError> {
        self.skills.iter().find(|s| s.name == name).ok_or_else(|| {
            ToolError::Invalid(format!(
                "no skill named `{name}`; available: {}",
                self.skills
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
    }
}

/// Paths under `dir`, relative to it, excluding `SKILL.md` itself.
fn sibling_files(dir: &std::path::Path) -> Vec<String> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        if depth > 3 || out.len() > 40 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
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
    out.sort();
    out
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "One of the names listed above." },
                "file": {
                    "type": "string",
                    "description": "A path inside the skill's directory, when its \
                                    instructions point at one.",
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
        let args: Args = serde_json::from_value(args)?;
        let skill = self.find(&args.name)?;

        let Some(rel) = args.file else {
            let text = tokio::fs::read_to_string(skill.dir.join("SKILL.md"))
                .await
                .map_err(|e| ToolError::Invalid(format!("{}: {e}", args.name)))?;
            let mut out = body(&text).to_string();
            // Skills usually live outside the workspace, where `read` cannot
            // reach. Instructions that point at a sibling file are useless
            // unless the way to fetch it arrives with them.
            let siblings = sibling_files(&skill.dir);
            if !siblings.is_empty() {
                out.push_str(&format!(
                    "\n---\nFiles in this skill: {}\nFetch one with                      `skill(name: \"{}\", file: \"<path>\")` — `read` cannot reach them.\n",
                    siblings.join(", "),
                    skill.name
                ));
            }
            return Ok(ToolOutput::text(out).with_preview(format!("skill {}", skill.name)));
        };

        // Skills live outside the workspace, so the workspace gate does not
        // cover them; the skill's own directory is the boundary instead.
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

        let text = tokio::fs::read_to_string(&real).await?;
        Ok(ToolOutput::text(text).with_preview(format!("{}/{rel}", skill.name)))
    }
}
