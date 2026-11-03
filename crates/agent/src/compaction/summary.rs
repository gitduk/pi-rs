use llm::message::AssistantContent;
use llm::model::ModelSpec;
use llm::slice::{clip_bytes, head_bytes, tail_bytes};

use crate::session::Entry;
use llm::stream::{StopReason, Usage};

pub const PROMPT: &str = include_str!("../../prompts/summarize.md");

// Per-block cap, in bytes, of the rendered history. A summarizer needs to
// know a file was read, not to re-read it.
const BLOCK_BYTES: usize = 1_500;

// Total cap, in bytes. The history being summarized is over budget by
// definition, so the request that summarizes it has to be bounded too.
const TOTAL_BYTES: usize = 60_000;

// A ceiling, not the length: the prompt asks for far less, and a reply that
// reaches it has lost its last sections.
const MAX_SUMMARY_TOKENS: u32 = 4_000;

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
            // Back on the model's own say-so, not the user's.
            Entry::Ask { ask, .. } if ask.relayed.is_some() => lines.push(format!(
                "[came back: {}] {}",
                ask.shown_text(),
                user_text(&ask.text)
            )),
            Entry::Ask { ask, .. } => lines.push(format!(
                "[user]{} {}",
                match ask.images.len() {
                    0 => String::new(),
                    1 => " (image)".to_string(),
                    n => format!(" ({n} images)"),
                },
                user_text(&ask.text)
            )),
            Entry::Bash { run, .. } => {
                lines.push(format!("[user ran] {}", clip_bytes(&run.text, BLOCK_BYTES)))
            }
            // A note is a directive for the one turn it opened; by the
            // time a summary carries it, it is stale.
            Entry::Note { .. } => {}
            Entry::Tool { result: r, .. } => {
                let mark = if r.is_error { " error" } else { "" };
                lines.push(format!(
                    "[{} result{mark}] {}",
                    r.name,
                    clip_bytes(&r.flatten_text(), BLOCK_BYTES)
                ));
            }
            Entry::Answer { blocks, .. } => {
                for b in blocks {
                    match b {
                        AssistantContent::Text(t) => {
                            lines.push(format!("[assistant] {}", clip_bytes(&t.text, BLOCK_BYTES)))
                        }
                        // Prior reasoning is the agent's scratch work, not a
                        // record of what happened.
                        AssistantContent::Reasoning(_) => {}
                        AssistantContent::ToolCall(c) => lines.push(format!(
                            "[calls {}] {}",
                            c.name,
                            clip_bytes(&c.args.to_string(), 400)
                        )),
                    }
                }
            }
            // Not content: it is the record that produced this call.
            Entry::Compaction { .. } => {}
            // Reports retained for the user carry no model context.
            Entry::Plain { .. } | Entry::Notice { .. } | Entry::Tally { .. } => {}
        }
    }

    let mut out = lines.join("\n");
    if out.len() > TOTAL_BYTES {
        // Both ends carry more than the middle: the opening says what the task
        // was, the tail says where it got to.
        let half = TOTAL_BYTES / 2;
        let head = (0..=half)
            .rev()
            .find(|&i| out.is_char_boundary(i))
            .unwrap_or(0);
        let tail = (out.len() - half..=out.len())
            .find(|&i| out.is_char_boundary(i))
            .unwrap_or(out.len());
        let gone = tail - head;
        out = format!(
            "{}\n… {gone} bytes omitted …\n{}",
            &out[..head],
            &out[tail..]
        );
    }
    out
}

// A user's task or correction can follow a long pasted body or skill.
fn user_text(text: &str) -> String {
    if text.len() <= BLOCK_BYTES {
        return text.to_string();
    }
    let head = head_bytes(text, BLOCK_BYTES / 2);
    let tail = tail_bytes(text, BLOCK_BYTES / 2);
    let gone = text.len() - head.len() - tail.len();
    format!("{head}\n… {gone} bytes omitted …\n{tail}")
}

/// Ask the model to compact a span of history into prose.
/// `focus` rides in the user turn rather than the system prompt: the prompt is
/// the same string on every summarization, which is what makes its cache worth
/// having, and a per-call instruction folded into it would break that.
pub async fn run(
    transport: llm::meter::Metered<'_>,
    spec: &ModelSpec,
    prompt: &str,
    history: String,
    focus: Option<&str>,
    idle: std::time::Duration,
) -> llm::Result<(String, Usage)> {
    let body = match focus.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("Focus the summary on: {f}\n\n{history}"),
        None => history,
    };
    let (text, usage, stop) =
        super::oneshot::ask(transport, spec, prompt, body, MAX_SUMMARY_TOKENS, idle).await?;
    // Cut short, it would replace the earlier summary with one missing its
    // tail; failing keeps that one in view instead.
    if stop != StopReason::EndTurn {
        return Err(llm::LlmError::Summarizer(format!(
            "the summarizer stopped early ({stop:?})"
        )));
    }
    // Nothing to say is a failure here: the span goes either way, and it goes
    // unsummarized.
    if text.trim().is_empty() {
        return Err(llm::LlmError::Summarizer(
            "the summarizer returned nothing".into(),
        ));
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EntryId, Prompt};
    use llm::message::ToolResult;

    fn user(text: &str) -> Entry {
        Entry::Ask {
            id: EntryId(0),
            at: 0,
            ask: Prompt {
                text: text.into(),
                images: Vec::new(),
                shown: None,
                relayed: None,
            },
        }
    }

    fn result(call: &str, name: &str, body: impl Into<String>) -> Entry {
        Entry::Tool {
            id: EntryId(0),
            at: 0,
            result: ToolResult::text(call, name, body),
            preview: None,
            warned: false,
            took: None,
        }
    }

    fn assistant(blocks: Vec<AssistantContent>) -> Entry {
        Entry::Answer {
            id: EntryId(0),
            at: 0,
            blocks,
        }
    }

    // A block over its cap is clipped in place; a whole history over the
    // total keeps both ends and drops the middle.
    #[test]
    fn oversize_history_is_clipped_rather_than_sent_whole() {
        let r = result("c", "read", "x".repeat(50_000));
        let out = render(&[], &[&r]);
        assert!(out.len() < BLOCK_BYTES + 200, "{}", out.len());
        assert!(out.contains("more bytes"), "{out}");

        let first = user("the original task");
        let last = user("the final state");
        let filler: Vec<Entry> = (0..200)
            .map(|i| {
                assistant(vec![AssistantContent::Text(llm::message::Text {
                    text: format!("step {i} ") + &"y".repeat(600),
                })])
            })
            .collect();
        let mut refs: Vec<&Entry> = vec![&first];
        refs.extend(filler.iter());
        refs.push(&last);

        let out = render(&[], &refs);
        assert!(out.len() < TOTAL_BYTES + 200, "{}", out.len());
        assert!(out.contains("the original task"), "the head must survive");
        assert!(out.contains("the final state"), "the tail must survive");
        assert!(out.contains("bytes omitted"), "{}", &out[..80]);
    }

    #[test]
    fn a_long_user_input_keeps_its_source_and_final_task_constraints() {
        let text = format!(
            "Run the review skill.\nSource: /skills/review/SKILL.md\n{}\nReview src/api.rs. Do not change files.",
            "步骤".repeat(2_000),
        );
        let ask = user(&text);
        let out = render(&[], &[&ask]);
        assert!(out.contains("Source: /skills/review/SKILL.md"));
        assert!(out.ends_with("Review src/api.rs. Do not change files."));
        assert!(out.contains("bytes omitted"));
        assert!(out.len() < BLOCK_BYTES + 100, "{}", out.len());
        let correction = user("Do not change files.");
        assert_eq!(render(&[], &[&correction]), "[user] Do not change files.");
    }

    #[test]
    fn a_long_relayed_input_keeps_its_tail_without_becoming_a_user_request() {
        let relayed = Entry::Ask {
            id: EntryId(1),
            at: 0,
            ask: Prompt {
                text: format!(
                    "{}\nThe command failed, not the task.",
                    "result\n".repeat(2_000)
                ),
                shown: Some("watch".into()),
                relayed: Some("came from a command".into()),
                images: Vec::new(),
            },
        };
        let note = Entry::Note {
            id: EntryId(2),
            at: 0,
            note: "A stale directive.".into(),
        };
        let out = render(&["Earlier verified work."], &[&relayed, &note]);
        assert!(out.contains("[earlier summary]\nEarlier verified work."));
        assert!(out.contains("[came back: watch]"));
        assert!(!out.contains("[user]"));
        assert!(!out.contains("A stale directive."));
        assert!(out.ends_with("The command failed, not the task."));
    }
}
