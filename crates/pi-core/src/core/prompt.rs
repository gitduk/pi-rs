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
        self.pi_home(&mut out);
        self.instructions(&mut out);
        self.memory(&mut out);
        self.env(&mut out);
        out
    }

    fn workspace(&self, out: &mut String) {
        out.push_str(&format!(
            "\n\n<workspace path=\"{}\"/>\n\nThe workspace is the directory you work in. \
Every path you name is relative to it, and commands start in it. What you make along the way \
— scripts, downloads, state files — goes in the workspace too, even when it feels throwaway: \
the user looks for it there. Reading may go further — an absolute path reaches the rest of \
this machine, a URL the rest of the world.",
            escaped(self.workspace.display())
        ));
        if !tool::Tier::Write.under(self.tier) {
            return;
        }
        out.push_str(match self.fenced {
            true => {
                " write and edit stay inside the workspace, and pi's home when a `<pi_home>` \
block names it."
            }
            false => {
                " write and edit reach anywhere, but a file outside the workspace is changed \
only when the user asks for it."
            }
        });
    }

    fn pi_home(&self, out: &mut String) {
        let Some(home) = &self.pi_home else {
            return;
        };
        out.push_str(&format!(
            "\n\n<pi_home path=\"{}\">\npi's own home, read live: what you add here is offered \
from your next turn, with no restart. The `pi-extend` skill says what can be added and \
how.\n</pi_home>",
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
            "\n\nThe `<instructions>` blocks below are the user's standing instructions, for \
this machine and this project, most general first. Follow them; where two disagree, the later \
one, nearer the workspace, wins. They say how to work here; the user's message says what to do \
now.",
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
            "\n\n<memory>\nWhat you have come to know in earlier sessions. Let it shape what \
you do the way a colleague's experience does: act on it without announcing it or saying you \
remember. Say where something came from only if asked. It can be out of date; what the user \
says now wins.\n{}\n</memory>",
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
            "<workspace",
            "<pi_home",
            "<instructions path",
            "<memory>",
            "<env",
        ];
        assert!(order.windows(2).all(|w| at(w[0]) < at(w[1])), "{text}");
        assert!(text.trim_end().ends_with("tier=\"exec\"/>"), "{text}");
    }
}
