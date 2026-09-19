use brain::message::AssistantContent;
use brain::model::ModelSpec;

use crate::session::Entry;
use brain::stream::Usage;
use brain::transport::Transport;

pub const PROMPT: &str = include_str!("../prompts/summarize.md");

// Per-block cap, in bytes, of the rendered history. A summarizer needs to
// know a file was read, not to re-read it.
const BLOCK_BYTES: usize = 1_500;

// Total cap, in bytes. The history being summarized is over budget by
// definition, so the request that summarizes it has to be bounded too.
const TOTAL_BYTES: usize = 60_000;

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
            Entry::Ask { ask, .. } => lines.push(format!(
                "[user]{} {}",
                if ask.image.is_some() { " (image)" } else { "" },
                clip(&ask.text, BLOCK_BYTES)
            )),
            Entry::Bash { run, .. } => {
                lines.push(format!("[user ran] {}", clip(&run.text, BLOCK_BYTES)))
            }
            // A note is a directive for the one turn it opened; by the
            // time a summary carries it, it is stale.
            Entry::Note { .. } => {}
            Entry::Tool { result: r, .. } => {
                let mark = if r.is_error { " error" } else { "" };
                lines.push(format!(
                    "[{} result{mark}] {}",
                    r.name,
                    clip(&r.flatten_text(), BLOCK_BYTES)
                ));
            }
            Entry::Answer { blocks, .. } => {
                for b in blocks {
                    match b {
                        AssistantContent::Text(t) => {
                            lines.push(format!("[assistant] {}", clip(&t.text, BLOCK_BYTES)))
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
    if out.len() > TOTAL_BYTES {
        // Both ends carry more than the middle: the opening says what the task
        // was, the tail says where it got to.
        let half = TOTAL_BYTES / 2;
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
    idle: std::time::Duration,
) -> brain::Result<(String, Usage)> {
    let body = match focus.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!("Focus the summary on: {f}\n\n{history}"),
        None => history,
    };
    let (text, usage) =
        crate::oneshot::ask(transport, spec, PROMPT, body, MAX_SUMMARY_TOKENS, idle).await?;
    // Nothing to say is a failure here: the span goes either way, and it goes
    // unsummarized.
    if text.trim().is_empty() {
        return Err(brain::BrainError::Summarizer(
            "the summarizer returned nothing".into(),
        ));
    }
    Ok((text, usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{EntryId, Prompt};
    use brain::message::ToolResult;

    fn user(text: &str) -> Entry {
        Entry::Ask {
            id: EntryId(0),
            at: 0,
            round: None,
            ask: Prompt {
                text: text.into(),
                image: None,
                shown: None,
            },
        }
    }

    fn result(call: &str, name: &str, body: impl Into<String>) -> Entry {
        Entry::Tool {
            id: EntryId(0),
            at: 0,
            result: ToolResult::text(call, name, body),
            preview: None,
        }
    }

    fn assistant(blocks: Vec<AssistantContent>) -> Entry {
        Entry::Answer {
            id: EntryId(0),
            at: 0,
            blocks,
        }
    }

    // Over budget is the one thing the history certainly is, so the render
    // never ships it whole: a block over its cap is clipped in place, and a
    // whole history over the total keeps both ends and drops the middle.
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
                assistant(vec![AssistantContent::Text(brain::message::Text {
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
        assert!(out.contains("middle omitted"), "{}", &out[..80]);
    }
}
