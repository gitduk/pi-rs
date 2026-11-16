//! Flat context query names shared by completion and detail lookup.

use super::{Choice, tool_help};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    System,
    Tools,
    Messages,
    Raw,
    Prompt(String),
    Tool(String),
}

#[derive(Debug)]
pub(crate) struct Item {
    pub choice: Choice,
    pub target: Target,
}

pub(crate) fn items(prompts: &[Choice], registry: Option<&tool::Registry>) -> Vec<Item> {
    let mut items: Vec<_> = [
        ("system", "full system prompt", Target::System),
        ("tools", "all tool definitions", Target::Tools),
        ("messages", "all effective messages", Target::Messages),
        (
            "raw",
            "full system prompt and tool definitions",
            Target::Raw,
        ),
    ]
    .into_iter()
    .map(|(name, note, target)| Item {
        choice: Choice {
            name: name.into(),
            note: note.into(),
        },
        target,
    })
    .collect();
    items.extend(prompts.iter().map(|prompt| Item {
        choice: prompt.clone(),
        target: Target::Prompt(prompt.name.clone()),
    }));
    items.extend(
        registry
            .into_iter()
            .flat_map(|registry| registry.listed())
            .map(|tool| Item {
                choice: Choice {
                    name: tool.name().into(),
                    note: tool_help(tool.as_ref()),
                },
                target: Target::Tool(tool.name().into()),
            }),
    );
    let mut counts = BTreeMap::new();
    for item in &items {
        *counts.entry(item.choice.name.clone()).or_insert(0) += 1;
    }
    for item in &mut items {
        let name = &item.choice.name;
        if counts[name] > 1 || name.contains(':') || name.bytes().all(|byte| byte.is_ascii_digit())
        {
            let prefix = match &item.target {
                Target::Prompt(_) => "prompt",
                Target::Tool(_) => "tool",
                Target::System | Target::Tools | Target::Messages | Target::Raw => continue,
            };
            item.choice.name = format!("{prefix}:{name}");
        }
    }
    items
}
