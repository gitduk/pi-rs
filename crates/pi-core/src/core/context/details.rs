//! Inspect the content behind each context category.

use super::{Core, messages};
use crate::core::prompt::Block;
use crate::input::commands::context::Target;
use llm::{estimate, figures};
use pi_store::listing::{Listing, Row};
use pi_store::text;

pub(super) fn listing(core: &Core, target: &Target) -> Listing {
    let lane = core.lane();
    let brief = &lane.agent().brief;
    let blocks = &lane.resolved().prompt_blocks;
    match target {
        Target::System => system(&brief.system, blocks, ""),
        Target::Prompt(name) => system(&brief.system, blocks, name),
        Target::Tools => tools(&brief.registry, ""),
        Target::Tool(name) => tools(&brief.registry, name),
        Target::Messages => match lane.session() {
            Some(session) => messages::listing(&session.context_view(), lane.agent().spec(), ""),
            None => Listing::say([
                "Message details are unavailable during a run. Try again after it finishes.",
            ]),
        },
        Target::Raw => raw(brief, blocks),
    }
}

fn size(tokens: usize, bytes: usize) -> String {
    format!(
        "~{} tokens · {}",
        figures::short(tokens as u64),
        text::size(bytes as u64)
    )
}

fn system(prompt: &str, blocks: &[Block], name: &str) -> Listing {
    let mut parts = blocks.iter().scan(prompt, |remaining, block| {
        let (body, rest) = remaining.split_at(block.bytes);
        *remaining = rest;
        Some((block, body))
    });
    if !name.is_empty() {
        let Some((block, body)) = parts.find(|(block, _)| block.name == name) else {
            return Listing::say([
                format!("No loaded system block named {name}."),
                "/context · show loaded prompt names".into(),
            ]);
        };
        let mut rows = vec![
            Row::new([block.name.into(), size(estimate::text(body), block.bytes)]),
            Row::new(["source", &block.source]),
            Row::new(["fingerprint".into(), format!("{:016x}", block.version)]),
            Row::new([""]),
        ];
        rows.extend(body.lines().map(|line| Row::new([line])));
        return Listing::of(rows);
    }
    let mut rows = vec![
        Row::new([
            "System prompt".into(),
            size(estimate::text(prompt), prompt.len()),
        ]),
        Row::new([""]),
    ];
    rows.extend(prompt.lines().map(|line| Row::new([line])));
    Listing::of(rows)
}

fn tools(registry: &tool::Registry, name: &str) -> Listing {
    if !name.is_empty() {
        return match registry.get(name) {
            Some(tool) => Listing::of(tool_rows(&tool.definition())),
            None => Listing::say([format!("No active tool named {name}.")]),
        };
    }
    Listing::of(registry.defs().iter().flat_map(tool_rows))
}

fn tool_rows(tool: &llm::request::ToolDef) -> Vec<Row> {
    let mut rows = vec![
        Row::new([
            tool.name.clone(),
            size(
                estimate::tool_defs(std::slice::from_ref(tool)),
                llm::request::defs_bytes(std::slice::from_ref(tool)),
            ),
        ]),
        Row::new([""]),
    ];
    rows.extend(tool.description.lines().map(|line| Row::new([line])));
    rows.push(Row::new([""]));
    rows.push(Row::new(["Parameters"]));
    rows.extend(
        serde_json::to_string_pretty(&tool.input_schema)
            .expect("JSON values serialize")
            .lines()
            .map(|line| Row::new([line])),
    );
    rows.push(Row::new([""]));
    rows
}

fn raw(brief: &agent::Briefing, blocks: &[Block]) -> Listing {
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
    fn rendered<'a>(rows: impl Iterator<Item = &'a super::Row>) -> String {
        rows.map(|row| row.cells[0].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

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
            &crate::core::mcp::Live::default(),
        )
        .unwrap();
        let listing = super::raw(&resolved.brief, &resolved.prompt_blocks);
        let text = rendered(listing.rows.iter());
        assert!(text.contains(resolved.brief.system.trim_end()));
        let mut offset = 0;
        for block in &resolved.prompt_blocks {
            assert!(text.contains(&format!("{:016x} · {}", block.version, block.source)));
            let detail = super::system(&resolved.brief.system, &resolved.prompt_blocks, block.name);
            let body = rendered(detail.rows.iter().skip(4));
            assert_eq!(
                body,
                resolved.brief.system[offset..offset + block.bytes]
                    .lines()
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            offset += block.bytes;
        }
        assert_eq!(offset, resolved.brief.system.len());
        let defs = resolved.brief.registry.defs();
        for tool in &defs {
            assert!(text.contains(&format!("{} · ", tool.name)));
            assert!(text.contains(&tool.description));
            assert!(text.contains(&format!("parameters: {}", tool.input_schema)));
            let detail = super::tools(&resolved.brief.registry, &tool.name);
            let schema = rendered(
                detail
                    .rows
                    .iter()
                    .skip_while(|row| row.cells[0] != "Parameters")
                    .skip(1),
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&schema).unwrap(),
                tool.input_schema
            );
        }
        assert!(resolved.brief.registry.get("write").is_none());
        assert!(!text.contains("can change files anywhere"));
    }
}
