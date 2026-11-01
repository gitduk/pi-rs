//! What `/content` shows: the request's standing part exactly as it is sent —
//! the system prompt with everything appended to it, then every tool.

use super::Core;
use super::prompt::Block;
use pi_store::listing::Listing;

impl Core {
    /// The front lane's system prompt and tool definitions, verbatim, each
    /// headed by its size.
    pub(super) fn content(&self) -> Listing {
        listing(
            &self.lane().agent().brief,
            &self.lane().resolved().prompt_blocks,
        )
    }
}

fn listing(brief: &agent::Briefing, blocks: &[Block]) -> Listing {
    let tools = brief.registry.defs();
    let size = |n: usize| pi_store::text::size(n as u64);
    let mut lines = vec![format!("── system · {}", size(brief.system.len()))];
    lines.push(
        "composition · bytes include separators · versions are diagnostic fingerprints".to_string(),
    );
    for block in blocks {
        lines.push(format!(
            "{} · {} · {:016x} · {}",
            block.name,
            size(block.bytes),
            block.version,
            block.source
        ));
    }
    lines.push(String::new());
    lines.extend(brief.system.lines().map(str::to_string));
    lines.push(String::new());
    lines.push(format!(
        "── tools · {} · {}",
        tools.len(),
        size(llm::request::defs_bytes(&tools))
    ));
    for tool in &tools {
        lines.push(String::new());
        lines.push(format!(
            "{} · {}",
            tool.name,
            size(llm::request::defs_bytes(std::slice::from_ref(tool)))
        ));
        lines.extend(tool.description.lines().map(str::to_string));
        lines.push(format!("parameters: {}", tool.input_schema));
    }
    Listing::say(lines)
}

#[cfg(test)]
mod tests {
    #[test]
    fn inspection_preserves_the_rendered_prompt_and_live_tool_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tool::Workspace::new(dir.path()).unwrap();
        let pinned = crate::args::Pinned {
            no_skills: true,
            no_context_files: true,
            tier: Some(pi_store::args::TierArg::Read),
            ..Default::default()
        };
        let settings = pi_store::settings::Settings::new(toml::Table::new().into(), None);
        let resolved = crate::core::resolve::resolve(
            &pinned,
            &workspace,
            &Default::default(),
            &settings,
            None,
            &[],
        )
        .unwrap();
        let listing = super::listing(&resolved.brief, &resolved.prompt_blocks);
        let text = listing
            .rows
            .iter()
            .map(|r| r.cells[0].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(resolved.brief.system.trim_end()));
        for block in &resolved.prompt_blocks {
            assert!(text.contains(&format!("{:016x} · {}", block.version, block.source)));
        }
        for tool in resolved.brief.registry.defs() {
            assert!(text.contains(&format!("{} · ", tool.name)));
            assert!(text.contains(&tool.description));
            assert!(text.contains(&format!("parameters: {}", tool.input_schema)));
        }
        assert!(resolved.brief.registry.get("write").is_none());
        assert!(!text.contains("can change files anywhere"));
    }
}
