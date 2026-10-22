//! What `/content` shows: the request's standing part exactly as it is sent —
//! the system prompt with everything appended to it, then every tool.

use super::Core;
use pi_store::listing::Listing;

impl Core {
    /// The front lane's system prompt and tool definitions, verbatim, each
    /// headed by its size.
    pub(super) fn content(&self) -> Listing {
        let brief = &self.lane().agent().brief;
        let tools = brief.registry.defs();
        let size = |n: usize| pi_store::text::size(n as u64);
        let mut lines = vec![format!("── system · {}", size(brief.system.len()))];
        lines.extend(brief.system.lines().map(str::to_string));
        lines.push(String::new());
        lines.push(format!(
            "── tools · {} · {}",
            tools.len(),
            size(llm::request::defs_bytes(&tools))
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
