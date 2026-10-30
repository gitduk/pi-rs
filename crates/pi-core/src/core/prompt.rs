//! What the run stands on, as the system prompt says it: every block pi
//! appends after the assistant's own prompt, in one order, from one place.
//!
//! The assistant's prompt may be replaced or emptied, so everything the run
//! needs to know about pi is said here. Blocks refer to each other ("below"),
//! which is why their order lives in one function: `Standing::render`.

use std::path::PathBuf;

/// The assistant's own prompt, when no file replaces it.
pub const DEFAULT_SYSTEM: &str = include_str!("../../prompts/system.md");

/// The built-in prompts a file of the same name under pi's `prompts/`
/// replaces whole, each with its built-in text.
pub const REPLACEABLE: [(&str, &str); 2] = [
    ("summarize", agent::compaction::SUMMARIZE),
    ("subagent", subagent::PROMPT),
];

/// The facts a run's blocks are drawn from. Empty fields leave their block out.
#[derive(Debug, Clone)]
pub struct Standing {
    /// The directory the run works in.
    pub workspace: PathBuf,
    /// The project tool directory read by the tool registry, even before it
    /// exists; set only when the run may write tools to it.
    pub project_tools: Option<PathBuf>,
    /// Whether write and edit are held to the workspace and pi's home.
    pub fenced: bool,
    /// pi's home, set only when the run may add tools and skills to it.
    pub pi_home: Option<PathBuf>,
    /// Instructions files by path, most general first.
    pub instructions: Vec<(PathBuf, String)>,
    /// Memory files by name.
    pub memory: Vec<(String, String)>,
    /// The day the run started, `YYYY-MM-DD`.
    pub day: String,
    pub tier: tool::Tier,
}

impl Standing {
    /// The blocks, each opened by a blank line, steadiest first: a provider
    /// caches the prompt by prefix, so what changes daily goes last.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.workspace(&mut out);
        self.project_tools(&mut out);
        self.pi_home(&mut out);
        self.instructions(&mut out);
        self.memory(&mut out);
        self.env(&mut out);
        out
    }

    fn workspace(&self, out: &mut String) {
        out.push_str(&format!(
            "\n\n<workspace path=\"{}\"/>\n\nWork in this directory. \
Relative paths resolve from the workspace. Commands start in the workspace. \
Keep scripts, downloads, and state files in the workspace, including temporary files. \
The user expects to find them there.\n\n\
A script is a code file that an interpreter or runtime executes. \
A tool is a named interface that the model can call directly. \
A tool can use a script as its implementation. Writing a script does not require registering a tool. \
When a registered tool supports the operation, call it directly through its tool interface. \
Use bash for ordinary scripts or operations that other available tool interfaces cannot express. \
Keep scripts written for the current task in the workspace. Run those task scripts through bash. \
Running a script repeatedly does not require a new tool.\n\n\
Reading may access files outside the workspace through absolute paths. URLs can access sources outside this machine.",
            escaped(self.workspace.display())
        ));
        if !tool::Tier::Write.under(self.tier) {
            return;
        }
        out.push_str(match self.fenced {
            true => {
                "\n\nThe tools write and edit stay inside the workspace. \
They can also access pi's home when a `<pi_home>` block specifies it."
            }
            false => {
                "\n\nThe tools write and edit reach anywhere. \
Change a file outside the workspace only when the user requests it."
            }
        });
    }

    fn project_tools(&self, out: &mut String) {
        let Some(dir) = &self.project_tools else {
            return;
        };
        out.push_str(&format!(
            "\n\n<project_tools path=\"{}\">\nUse this directory for project tools by default. \
This includes tools that depend on this project's code, data, paths, or configuration. \
Create the directory if it does not exist. The run's write boundary still applies. \
If that boundary blocks this directory, report the limit. Do not use pi's home as a fallback.\n</project_tools>",
            escaped(dir.display())
        ));
    }

    fn pi_home(&self, out: &mut String) {
        let Some(home) = &self.pi_home else {
            return;
        };
        out.push_str(&format!(
            "\n\n<pi_home path=\"{}\">\nThis is pi's home. \
Tools here are available across projects. \
Add a tool here only when the user requests a tool shared across projects. \
A tool that depends on this project belongs with the project, not here.\n\n\
Tools and skills added here become available on your next turn. No restart is needed. \
pi registers tools from scripts in `tools/`. Do not store the current task's scripts or files here. \
Read `tool-creator` to create tools, `skill-creator` to create skills, and `mcp-setup` to connect MCP servers.\n</pi_home>",
            escaped(home.display())
        ));
    }

    // Tagged rather than headed: the content is arbitrary markdown with
    // headings of its own, so a `#` delimiter would not delimit anything.
    fn instructions(&self, out: &mut String) {
        if self.instructions.is_empty() {
            return;
        }
        out.push_str(
            "\n\nThe `<instructions>` blocks below contain the user's standing instructions for this machine and project. \
The most general instructions appear first. Follow these instructions. \
If two instructions conflict, the later instruction, nearer the workspace, takes precedence. \
These instructions specify how to work here. The user's message specifies the current task.",
        );
        for (path, body) in &self.instructions {
            out.push_str(&format!(
                "\n\n<instructions path=\"{}\">\n{}\n</instructions>",
                escaped(path.display()),
                body.trim_end()
            ));
        }
    }

    // After the instructions: what was learned is read in light of what the
    // user wrote down.
    fn memory(&self, out: &mut String) {
        if self.memory.is_empty() {
            return;
        }
        out.push_str(&format!(
            "\n\n<memory>\nThese facts come from earlier sessions. \
Use them without announcing that you remember them. State their source only if asked. \
Memory may be outdated. The user's current instructions take precedence.\n{}\n</memory>",
            files(&self.memory)
        ));
    }

    // `sh`, not `$SHELL`: the bash tool runs `Command::new("sh")` whatever the
    // login shell is. Only the day, so runs an hour apart share a cache entry.
    fn env(&self, out: &mut String) {
        let tier = format!("{:?}", self.tier).to_lowercase();
        out.push_str(&format!(
            "\n\n<env date=\"{}\" platform=\"{}\" shell=\"sh\" pi=\"{}\" tier=\"{tier}\"/>",
            self.day,
            std::env::consts::OS,
            env!("CARGO_PKG_VERSION"),
        ));
    }
}

/// Named files as `<file name="…">` blocks, one after another.
pub fn files(files: &[(String, String)]) -> String {
    files
        .iter()
        .map(|(name, body)| format!("<file name=\"{name}\">\n{}\n</file>", body.trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(tier: tool::Tier) -> Standing {
        Standing {
            workspace: "/w".into(),
            project_tools: Some("/w/.pi/tools".into()),
            fenced: true,
            pi_home: Some("/h/.pi".into()),
            instructions: vec![("/w/AGENTS.md".into(), "be terse\n".into())],
            memory: vec![("user.md".into(), "- likes rust\n".into())],
            day: "2026-10-07".into(),
            tier,
        }
    }

    #[test]
    fn the_write_boundary_claims_only_what_the_ceiling_allows() {
        for tier in [tool::Tier::Read, tool::Tier::Net] {
            assert!(!standing(tier).render().contains("write and edit"));
        }
        let fenced = standing(tool::Tier::Write).render();
        let open = Standing {
            fenced: false,
            ..standing(tool::Tier::Exec)
        }
        .render();
        assert!(fenced.contains("write and edit stay inside"), "{fenced}");
        assert!(open.contains("write and edit reach anywhere"), "{open}");
    }

    // Blocks point at each other ("below"), and the prompt is cached by
    // prefix: an order nobody sees changing is an order nobody checks.
    #[test]
    fn blocks_come_in_one_order_with_the_date_last() {
        let text = standing(tool::Tier::Exec).render();
        let at = |tag: &str| text.find(tag).unwrap_or_else(|| panic!("{tag}: {text}"));
        let order = [
            "<workspace path=",
            "<project_tools path=",
            "<pi_home path=",
            "<instructions path",
            "<memory>",
            "<env",
        ];
        assert!(order.windows(2).all(|w| at(w[0]) < at(w[1])), "{text}");
        assert!(text.trim_end().ends_with("tier=\"exec\"/>"), "{text}");
    }

    #[test]
    fn the_project_tool_path_is_escaped_and_does_not_require_a_global_home() {
        let mut standing = standing(tool::Tier::Exec);
        standing.project_tools = Some("/repo/a&b/\"tools\"".into());
        standing.pi_home = None;
        let text = standing.render();
        assert!(text.contains("<project_tools path=\"/repo/a&amp;b/&quot;tools&quot;\">"));
        assert!(!text.contains("<pi_home path="));

        standing.project_tools = None;
        assert!(!standing.render().contains("<project_tools path="));
    }
}
