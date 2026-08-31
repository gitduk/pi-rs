use brain::model::ModelSpec;
use brain::message::{AssistantContent, Message};

use crate::session::{Entry, UserBody};
use brain::request::{Effort, Request, ToolChoice};
use brain::stream::{Accumulator, Usage};
use brain::transport::Transport;
use futures::StreamExt;

pub const PROMPT: &str = include_str!("../prompts/summarize.md");

// Per-block cap in the rendered history. A summarizer needs to know a file was
// read, not to re-read it.
const BLOCK_CHARS: usize = 1_500;

// Total cap. The history being summarized is over budget by definition, so the
// request that summarizes it has to be bounded too.
const TOTAL_CHARS: usize = 60_000;

const MAX_SUMMARY_TOKENS: u32 = 2_000;

fn clip(s: &str, max: usize) -> String {
    match s.char_indices().find(|(i, c)| i + c.len_utf8() > max) {
        Some((i, _)) => format!("{}… ({} more bytes)", &s[..i], s.len() - i),
        None => s.to_string(),
    }
}

/// Flatten history into one user turn.
///
/// The dropped span starts on an assistant turn, so replaying it as messages
/// would break the alternation both wires require — and a summarizer wants tool
/// output as prose anyway.
pub fn render(earlier: &[&str], entries: &[&Entry]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for s in earlier {
        lines.push(format!("[earlier summary]\n{s}"));
    }
    for e in entries {
        match e {
            Entry::User { body, .. } => match body {
                UserBody::Prompt(t) => {
                    lines.push(format!("[user] {}", clip(&t.text, BLOCK_CHARS)))
                }
                UserBody::Aside(t) => {
                    lines.push(format!("[user ran] {}", clip(&t.text, BLOCK_CHARS)))
                }
                UserBody::Image(_) => lines.push("[user] (image)".into()),
                UserBody::Result { result: r, .. } => {
                    let mark = if r.is_error { " error" } else { "" };
                    lines.push(format!(
                        "[{} result{mark}] {}",
                        r.name,
                        clip(&r.flatten_text(), BLOCK_CHARS)
                    ));
                }
            },
            Entry::Assistant { blocks, .. } => {
                for b in blocks {
                    match b {
                        AssistantContent::Text(t) => {
                            lines.push(format!("[assistant] {}", clip(&t.text, BLOCK_CHARS)))
                        }
                        // Prior reasoning is the agent's scratch work, not a
                        // record of what happened.
                        AssistantContent::Reasoning(_) => {}
                        AssistantContent::ToolCall(c) => lines.push(format!(
                            "[calls {}] {}",
                            c.name,
                            clip(&c.args.to_string(), 400)
                        )),
                    }
                }
            }
            // Not content: it is the record that produced this call.
            Entry::Compaction { .. } => {}
        }
    }

    let mut out = lines.join("\n");
    if out.len() > TOTAL_CHARS {
        // Both ends carry more than the middle: the opening says what the task
        // was, the tail says where it got to.
        let half = TOTAL_CHARS / 2;
        let head = clip(&out, half);
        let start = out.len().saturating_sub(half);
        let tail = match out.char_indices().find(|(i, _)| *i >= start) {
            Some((i, _)) => out[i..].to_string(),
            None => String::new(),
        };
        out = format!("{head}\n… (middle omitted) …\n{tail}");
    }
    out
}

/// Ask the model to compact a span of history into prose.
/// `focus` rides in the user turn rather than the system prompt: the prompt is
/// the same string on every summarization, which is what makes its cache worth
/// having, and a per-call instruction folded into it would break that.
pub async fn run(
    transport: &dyn Transport,
    spec: &ModelSpec,
    history: String,
    focus: Option<&str>,
) -> brain::Result<(String, Usage)> {
    let body = match focus.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("Focus the summary on: {f}\n\n{history}"),
        None => history,
    };
    let req = Request {
        system: Some(PROMPT.to_string()),
        messages: vec![Message::user(body)],
        notes: Vec::new(),
        tools: Vec::new(),
        max_output_tokens: Some(MAX_SUMMARY_TOKENS.min(spec.max_output_tokens)),
        temperature: None,
        // Reasoning about a summary costs more than the summary is worth.
        effort: Effort::Off,
        tool_choice: ToolChoice::None,
    };

    let mut acc = Accumulator::new(spec.model.clone());
    let mut stream = transport.stream(spec, &req).await?;
    while let Some(ev) = stream.next().await {
        acc.push(ev?);
    }
    let done = acc.finish();
    let text = done.message.text();
    if text.trim().is_empty() {
        return Err(brain::BrainError::Stream(
            "the summarizer returned nothing".into(),
        ));
    }
    Ok((text, done.usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EntryId, UserText};
    use brain::message::{ToolCall, ToolResult};
    use serde_json::json;

    fn user(text: &str) -> Entry {
        Entry::User {
            id: EntryId(0),
            at: 0,
            body: UserBody::Prompt(UserText {
                text: text.into(),
                shown: None,
            }),
        }
    }

    fn result(call: &str, name: &str, body: impl Into<String>) -> Entry {
        Entry::User {
            id: EntryId(0),
            at: 0,
            body: UserBody::Result {
                result: ToolResult::text(call, name, body),
                preview: None,
            },
        }
    }

    fn assistant(blocks: Vec<AssistantContent>) -> Entry {
        Entry::Assistant {
            id: EntryId(0),
            at: 0,
            blocks,
        }
    }

    #[test]
    fn a_rendered_history_names_tools_and_keeps_prose() {
        let a = assistant(vec![
            AssistantContent::Text(brain::message::Text {
                text: "reading it".into(),
            }),
            AssistantContent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read".into(),
                args: json!({ "path": "a.rs" }),
            }),
        ]);
        let r = result("c1", "read", "fn main() {}");
        let out = render(&[], &[&a, &r]);

        assert!(out.contains("[assistant] reading it"), "{out}");
        assert!(out.contains("[calls read] {\"path\":\"a.rs\"}"), "{out}");
        assert!(out.contains("[read result] fn main() {}"), "{out}");
    }

    #[test]
    fn an_earlier_summary_leads_so_the_model_can_fold_it_in() {
        let m = user("go");
        let out = render(&["did three things"], &[&m]);
        assert!(
            out.starts_with("[earlier summary]\ndid three things"),
            "{out}"
        );
    }

    #[test]
    fn a_huge_result_is_clipped_rather_than_sent_whole() {
        let r = result("c", "read", "x".repeat(50_000));
        let out = render(&[], &[&r]);
        assert!(out.len() < BLOCK_CHARS + 200, "{}", out.len());
        assert!(out.contains("more bytes"), "{out}");
    }

    #[test]
    fn a_long_history_keeps_both_ends() {
        let first = user("the original task");
        let last = user("the final state");
        let filler: Vec<Entry> = (0..200)
            .map(|i| {
                assistant(vec![AssistantContent::Text(brain::message::Text {
                    text: format!("step {i} ") + &"y".repeat(600),
                })])
            })
            .collect();
        let mut refs: Vec<&Entry> = vec![&first];
        refs.extend(filler.iter());
        refs.push(&last);

        let out = render(&[], &refs);
        assert!(out.len() < TOTAL_CHARS + 200, "{}", out.len());
        assert!(out.contains("the original task"), "the head must survive");
        assert!(out.contains("the final state"), "the tail must survive");
        assert!(out.contains("middle omitted"), "{}", &out[..80]);
    }

    #[test]
    fn reasoning_is_left_out_of_the_record() {
        let a = assistant(vec![AssistantContent::Reasoning(brain::message::Reasoning {
            id: None,
            content: vec![brain::message::ReasoningContent::Text {
                text: "scratch work".into(),
                signature: None,
            }],
            by: None,
        })]);
        assert!(!render(&[], &[&a]).contains("scratch work"));
    }
}
