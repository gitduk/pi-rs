//! What `/content` shows: the request's standing part exactly as it is sent —
//! the system prompt with everything appended to it, then every tool.

use super::Core;
use pi_store::listing::Listing;

impl Core {
    /// The front lane's system prompt and tool definitions, verbatim, each
    /// headed by what it costs.
    pub(super) fn content(&self) -> Listing {
        let brief = &self.lane().agent().brief;
        let tools = brief.registry.defs();
        let mut lines = vec![format!(
            "── system · ~{} tokens",
            llm::estimate::text(&brief.system)
        )];
        lines.extend(brief.system.lines().map(str::to_string));
        lines.push(String::new());
        lines.push(format!(
            "── tools · {} · ~{} tokens",
            tools.len(),
            llm::estimate::tool_defs(&tools)
        ));
        for tool in &tools {
            lines.push(String::new());
            lines.push(format!("{}:", tool.name));
            lines.extend(tool.description.lines().map(str::to_string));
            lines.push(format!("parameters: {}", tool.input_schema));
        }
        Listing::say(lines)
    }
}
