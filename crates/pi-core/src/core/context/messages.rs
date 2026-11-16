//! Effective history inspection without flattening bodies for the index.

use agent::session::context::{Message, UserContent};
use llm::message::{AssistantContent, Replay};
use llm::{estimate, figures};
use pi_store::listing::{Listing, Row};
use pi_store::text;

use super::share;

pub(super) const CATEGORIES: [&str; 5] = [
    "user + images",
    "model + reasoning",
    "tool calls",
    "tool results",
    "message framing",
];

pub(super) fn listing(
    history: &[Message<'_>],
    spec: &llm::model::ModelSpec,
    name: &str,
) -> Listing {
    if !name.is_empty() {
        let selected = name
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| history.get(n).map(|message| (n, message)));
        let Some((index, message)) = selected else {
            return Listing::say([
                format!("No message numbered {name}."),
                "/context messages · list current message numbers".into(),
            ]);
        };
        let mut lines = vec![
            format!(
                "Message {} · {} · ~{} tokens",
                index + 1,
                role(message),
                figures::short(message.tokens(spec) as u64)
            ),
            String::new(),
        ];
        lines.extend(message_text(message, spec).lines().map(str::to_string));
        return Listing::say(lines);
    }
    let window = spec.context_window as usize;
    let mut totals = [0; 5];
    let mut index = Vec::with_capacity(history.len());
    for (at, message) in history.iter().enumerate() {
        let parts = estimate_parts(message, spec);
        for (total, part) in totals.iter_mut().zip(parts) {
            *total += part;
        }
        index.push(
            share(
                &format!("{} · {}", at + 1, role(message)),
                parts.iter().sum(),
                window,
            )
            .noting(message_preview(message)),
        );
    }
    let total: usize = totals.iter().sum();
    let mut rows = vec![
        Row::new([
            "Messages".into(),
            format!(
                "{} · ~{} tokens",
                history.len(),
                figures::short(total as u64)
            ),
        ]),
        Row::new([
            "Effective history after compaction and rewind; bars and percentages use the full window.",
        ]),
        Row::new([""]),
    ];
    for (label, tokens) in CATEGORIES.into_iter().zip(totals) {
        rows.push(share(label, tokens, window));
    }
    rows.push(Row::new([""]));
    rows.extend(index);
    rows.push(Row::new([""]));
    rows.push(Row::new([
        "/context <number> · inspect a message; numbers refer to this snapshot",
    ]));
    rows.push(Row::new([super::ESTIMATES]));
    Listing::of(rows)
}

pub(super) fn estimate_parts(message: &Message<'_>, spec: &llm::model::ModelSpec) -> [usize; 5] {
    // User speech/images, model speech/replayed reasoning, calls, results,
    // and per-message framing. Each block contributes to exactly one part.
    let mut parts = [0; 5];
    parts[4] = estimate::MESSAGE_OVERHEAD;
    match message {
        Message::User { content } => {
            for block in content {
                let at = match block {
                    UserContent::ToolResult(_) => 3,
                    UserContent::Text(_) | UserContent::Image(_) => 0,
                };
                parts[at] += block.tokens();
            }
        }
        Message::Assistant { content } => {
            for block in content {
                let at = match block.as_ref() {
                    AssistantContent::ToolCall(_) => 2,
                    AssistantContent::Text(_) | AssistantContent::Reasoning(_) => 1,
                };
                parts[at] += estimate::assistant_block(block, spec);
            }
        }
    }
    parts
}

pub(super) fn role(message: &Message<'_>) -> &'static str {
    match message {
        Message::User { content } => {
            let results = content
                .iter()
                .filter(|b| matches!(b, UserContent::ToolResult(_)))
                .count();
            match (results > 0, results < content.len()) {
                (true, true) => "user + results",
                (true, false) => "tool results",
                (false, _) => "user",
            }
        }
        Message::Assistant { content } => {
            let calls = content
                .iter()
                .filter(|b| matches!(b.as_ref(), AssistantContent::ToolCall(_)))
                .count();
            match (calls > 0, calls < content.len()) {
                (true, true) => "model + calls",
                (true, false) => "tool calls",
                (false, _) => "model",
            }
        }
    }
}

pub(super) fn message_preview(message: &Message<'_>) -> String {
    // Do not flatten large tool results or images just to show an index.
    let preview = match message {
        Message::User { content } => match content.first() {
            Some(UserContent::Text(text)) => text.as_ref(),
            Some(UserContent::ToolResult(result)) => &result.name,
            Some(UserContent::Image(_)) => "[image]",
            None => "[empty]",
        },
        Message::Assistant { content } => content
            .iter()
            .find_map(|block| match block.as_ref() {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                AssistantContent::ToolCall(call) => Some(call.name.as_str()),
                AssistantContent::Reasoning(_) => None,
            })
            .unwrap_or(if content.is_empty() {
                "[empty]"
            } else {
                "[reasoning]"
            }),
    };
    let prefix: String = preview.chars().take(81).collect();
    text::clip(&text::collapsed(&prefix), 80)
}

fn message_text(message: &Message<'_>, spec: &llm::model::ModelSpec) -> String {
    match message {
        Message::User { content } => content
            .iter()
            .map(|block| match block {
                UserContent::Text(text) => text.to_string(),
                UserContent::Image(_) => "[image]".into(),
                UserContent::ToolResult(result) => format!(
                    "Tool result · {} · {}{}\n{}",
                    result.name,
                    result.call,
                    if result.is_error { " · error" } else { "" },
                    result.flatten_text()
                ),
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        Message::Assistant { content } => content
            .iter()
            .filter_map(|block| match block.as_ref() {
                AssistantContent::Text(text) => Some(text.text.clone()),
                AssistantContent::ToolCall(call) => Some(format!(
                    "Tool call · {} · {}\n{}",
                    call.name,
                    call.id,
                    serde_json::to_string_pretty(&call.args).expect("JSON values serialize")
                )),
                AssistantContent::Reasoning(reasoning) => {
                    match llm::message::replay_for(reasoning, spec) {
                        Replay::Thinking { .. } => Some(format!("Reasoning\n{}", reasoning.text())),
                        Replay::Prose { text } => Some(format!("Reasoning\n{text}")),
                        Replay::Encrypted { .. } => Some("[encrypted reasoning]".into()),
                        Replay::Redacted { .. } => Some("[redacted reasoning]".into()),
                        Replay::Dropped => None,
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}
