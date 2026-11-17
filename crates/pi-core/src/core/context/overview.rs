//! Global and directory budgets, all scaled to the full model window.

use super::{messages, percent, percentage};
use crate::core::lane::Lane;
use crate::core::meter::ContextSample;
use llm::{estimate, figures};
use pi_store::listing::{Role, Row};
use std::cmp::Reverse;

const LARGEST_MESSAGES: usize = 3;
// A skill loaded once and never again still costs a row on every read; the
// ones that are used are the ones worth reading.
const MOST_SKILLS: usize = 8;

pub(super) fn rows(lane: &Lane) -> Vec<Row> {
    let spec = lane.agent().spec();
    let window = spec.context_window as usize;
    let brief = &lane.agent().brief;
    let defs = brief.registry.defs();
    let system = estimate::text(&brief.system);
    let mut tools: Vec<_> = defs
        .iter()
        .map(|tool| (tool, estimate::tool_defs(std::slice::from_ref(tool))))
        .collect();
    tools.sort_by_key(|(tool, tokens)| (Reverse(*tokens), &tool.name));
    let tool_tokens: usize = tools.iter().map(|(_, tokens)| tokens).sum();
    let history = lane.session().map(|session| session.context_view());
    let mut parts = [0; 5];
    let mut largest: Vec<(usize, usize)> = Vec::with_capacity(LARGEST_MESSAGES + 1);
    if let Some(history) = &history {
        for (index, message) in history.iter().enumerate() {
            let costs = messages::estimate_parts(message, spec);
            for (total, cost) in parts.iter_mut().zip(costs) {
                *total += cost;
            }
            let tokens = costs.iter().sum::<usize>();
            if largest.len() < LARGEST_MESSAGES || tokens > largest[LARGEST_MESSAGES - 1].0 {
                largest.push((tokens, index));
                largest.sort_by_key(|(tokens, index)| (Reverse(*tokens), *index));
                largest.truncate(LARGEST_MESSAGES);
            }
        }
    }
    let message_tokens: usize = parts.iter().sum();
    let total = history
        .as_ref()
        .map(|_| system + tool_tokens + message_tokens);
    let mut rows = vec![
        Row::new(["Context"])
            .noting(&spec.model)
            .with_role(Role::Title),
    ];
    rows.push(match total {
        Some(total) => Row::new([
            "current input".into(),
            format!("~{}", figures::short(total as u64)),
            format!("/ {}", figures::size(window as u64)),
            percentage(total, window),
        ])
        .with_role(Role::Metric),
        None => Row::new([
            "current input",
            "unavailable",
            "message breakdown unavailable during run",
        ])
        .with_role(Role::Metric),
    });
    if let Some(total) = total {
        rows.push(Row::default().with_role(Role::Progress {
            used: total,
            total: window,
        }));
        if window > 0 && total > window {
            rows.push(Row::new([
                "Estimate exceeds the window; the next request must fit or compact.",
            ]));
        }
    }
    if let Some((used, window)) = lane.tally().ctx() {
        rows.push(Row::new([
            "last request".into(),
            format!(
                "{} / {}{} · provider reported",
                figures::short(used as u64),
                figures::size(window as u64),
                percent(used, window)
            ),
        ]));
    }
    rows.push(Row::new([""]));
    rows.push(section("Sources", "WINDOW"));
    let mut sources = [
        (
            "system",
            system,
            format!("{} blocks", lane.resolved().prompt_blocks.len()),
        ),
        ("tools", tool_tokens, format!("{} definitions", defs.len())),
        (
            "messages",
            message_tokens,
            history.as_ref().map_or_else(
                || "unavailable during run".into(),
                |history| format!("{} messages", history.len()),
            ),
        ),
    ];
    sources.sort_by_key(|(_, tokens, _)| Reverse(*tokens));
    for (name, tokens, count) in sources {
        rows.push(if name == "messages" && history.is_none() {
            Row::new([name, count.as_str()])
        } else {
            budget(name, tokens, window).noting(count)
        });
    }

    let mut directories = Vec::with_capacity(3);
    let mut system_rows = vec![section("System", "WINDOW")];
    let mut remaining = brief.system.as_str();
    let mut prompts = Vec::with_capacity(lane.resolved().prompt_blocks.len());
    for block in &lane.resolved().prompt_blocks {
        let (body, rest) = remaining.split_at(block.bytes);
        remaining = rest;
        prompts.push((block.name, estimate::text(body)));
    }
    prompts.sort_by_key(|(_, tokens)| Reverse(*tokens));
    system_rows.extend(
        prompts
            .into_iter()
            .map(|(name, tokens)| budget(name, tokens, window)),
    );

    directories.push((system, system_rows));
    tools.retain(|(_, tokens)| window == 0 || percentage(*tokens, window) != "0.0%");
    let mut tool_rows =
        vec![section("Tools", "WINDOW").noting(format!("{} / {} shown", tools.len(), defs.len()))];
    for (tool, tokens) in tools {
        tool_rows.push(budget(&tool.name, tokens, window));
    }
    if !defs.is_empty() {
        directories.push((tool_tokens, tool_rows));
    }

    if let Some(history) = &history
        && !history.is_empty()
    {
        let mut message_rows = vec![section("Messages", "WINDOW")];
        let mut categories: Vec<_> = messages::CATEGORIES
            .into_iter()
            .zip(parts)
            .filter(|(_, tokens)| *tokens > 0)
            .collect();
        categories.sort_by_key(|(_, tokens)| Reverse(*tokens));
        message_rows.extend(
            categories
                .into_iter()
                .map(|(name, tokens)| budget(name, tokens, window)),
        );
        message_rows.push(Row::new(["Largest messages"]).with_role(Role::Hint));
        for (tokens, index) in largest {
            message_rows.push(budget(&(index + 1).to_string(), tokens, window).noting(
                pi_store::text::clip(&messages::message_preview(&history[index]), 32),
            ));
        }
        directories.push((message_tokens, message_rows));
    }
    directories.sort_by_key(|(tokens, _)| Reverse(*tokens));
    // A skill costs nothing until it is loaded, so it reads beside the sources
    // rather than inside them: its tokens are what a load would add, and its
    // count outlives this session.
    let loads = pi_store::state::read().skills;
    if !loads.is_empty() {
        let mut skills: Vec<_> = loads.into_iter().collect();
        skills.sort_by_key(|(_, load)| Reverse((load.loads, load.bytes)));
        let loaded = skills.len();
        skills.truncate(MOST_SKILLS);
        let mut heading = section("Skills", "COUNT");
        if loaded > skills.len() {
            heading = heading.noting(format!("{} / {} shown", skills.len(), loaded));
        }
        let mut skill_rows = vec![heading];
        skill_rows.extend(skills.into_iter().map(|(name, load)| {
            Row::new([
                name,
                estimated(estimate::bytes(load.bytes as usize)),
                load.loads.to_string(),
            ])
            .with_role(Role::Budget)
        }));
        directories.push((0, skill_rows));
    }
    for (_, directory) in directories {
        rows.push(Row::new([""]));
        rows.extend(directory);
    }
    trend(&mut rows, lane.tally());
    rows
}

fn budget(name: &str, tokens: usize, window: usize) -> Row {
    Row::new([name.into(), estimated(tokens), percentage(tokens, window)]).with_role(Role::Budget)
}

// A section row names the numeric columns its own rows are read under; the
// second is the one a section has that another does not.
fn section(name: &str, share: &str) -> Row {
    Row::new([name, "TOKENS", share]).with_role(Role::Section)
}

// An estimate, marked as one: a provider's own count may differ.
fn estimated(tokens: usize) -> String {
    format!("~{}", figures::short(tokens as u64))
}

fn trend(rows: &mut Vec<Row>, tally: &crate::core::meter::Tally) {
    let samples = tally.context_samples();
    if samples
        .iter()
        .filter(|sample| matches!(sample, ContextSample::Request(_)))
        .count()
        < 2
    {
        return;
    }
    let mut range: Option<(usize, usize)> = None;
    for sample in samples {
        if let ContextSample::Request(used) = sample {
            range = Some(match range {
                Some((low, high)) => (low.min(*used), high.max(*used)),
                None => (*used, *used),
            });
        }
    }
    let Some((low, high)) = range else {
        return;
    };
    let spark: String = samples
        .iter()
        .map(|sample| match sample {
            ContextSample::Compacted => '│',
            ContextSample::Request(used) => {
                let at = (used - low)
                    .saturating_mul(7)
                    .checked_div(high - low)
                    .unwrap_or(0);
                ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'][at]
            }
        })
        .collect();
    rows.push(Row::new([""]));
    rows.push(
        Row::new([
            "requests".into(),
            format!(
                "{spark}  {}–{} · relative scale · this run{}",
                figures::short(low as u64),
                figures::short(high as u64),
                if samples
                    .iter()
                    .any(|sample| matches!(sample, ContextSample::Compacted))
                {
                    " · │ compacted"
                } else {
                    ""
                },
            ),
        ])
        .with_role(Role::Footer),
    );
}
