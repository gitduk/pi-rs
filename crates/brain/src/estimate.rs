use crate::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Replay, ToolResultContent, UserContent,
};
use crate::model::ModelSpec;
use crate::request::ToolDef;

// Bytes per token. Deliberately low: overestimating trips compaction a little
// early, underestimating trips a 400 from the provider mid-run.
const BYTES_PER_TOKEN: usize = 3;

/// Framing every message carries regardless of content.
pub const MESSAGE_OVERHEAD: usize = 8;

// Framing a tool call or result carries beyond its payload.
const BLOCK_OVERHEAD: usize = 12;

// What an image costs once encoded. A crude constant beats no accounting: an
// unmeasured image is the one thing that blows a budget silently.
const IMAGE_TOKENS: usize = 1_500;

/// A bound on what a string costs. Public because the system prompt and tool
/// schemas come out of the same budget the transcript does.
pub fn text(s: &str) -> usize {
    s.len().div_ceil(BYTES_PER_TOKEN)
}

use text as of;

// `<think>` and its closing tag, the wrapper a demoted block ships inside.
const TAG_OVERHEAD: usize = 6;

/// A bound on what a transcript costs to send to `spec`.
///
/// No tokenizer is embedded: this decides *when* to compact, and for that a
/// bound in the right direction beats an exact count for a tokenizer the
/// provider may not even be using.
///
/// Replay-aware, and that is the whole reason it takes a spec. A model that
/// drops prior reasoning is sent none of it, so counting it here compacts a
/// session against bytes that never leave — on a transcript that is 40%
/// reasoning, that is where the budget goes.
pub fn tokens(messages: &[Message], spec: &ModelSpec) -> usize {
    messages.iter().map(|m| message(m, spec)).sum()
}

pub fn message(m: &Message, spec: &ModelSpec) -> usize {
    MESSAGE_OVERHEAD
        + match m {
            Message::System { content } => of(content),
            Message::User { content } => content.iter().map(user_block).sum(),
            Message::Assistant { content, .. } => {
                content.iter().map(|b| assistant_block(b, spec)).sum()
            }
        }
}

pub fn user_block(b: &UserContent) -> usize {
    match b {
        UserContent::Text(t) => of(&t.text),
        UserContent::Image(_) => IMAGE_TOKENS,
        UserContent::ToolResult(r) => {
            BLOCK_OVERHEAD + of(&r.name) + r.content.iter().map(result_part).sum::<usize>()
        }
    }
}

fn result_part(p: &ToolResultContent) -> usize {
    match p {
        ToolResultContent::Text(t) => of(&t.text),
        ToolResultContent::Json { value } => of(&value.to_string()),
        ToolResultContent::Image(_) => IMAGE_TOKENS,
    }
}

/// What the block costs on a request to `spec`.
pub fn assistant_block(b: &AssistantContent, spec: &ModelSpec) -> usize {
    match b {
        AssistantContent::Reasoning(r) => replayed_reasoning(r, spec),
        other => whole_block(other),
    }
}

// What the block weighs, replay ignored.
fn whole_block(b: &AssistantContent) -> usize {
    match b {
        AssistantContent::Text(t) => of(&t.text),
        AssistantContent::Reasoning(r) => r
            .content
            .iter()
            .map(|c| match c {
                ReasoningContent::Text { text, signature } => {
                    of(text) + signature.as_deref().map_or(0, of)
                }
                ReasoningContent::Encrypted(s) => of(s),
            })
            .sum(),
        AssistantContent::ToolCall(c) => BLOCK_OVERHEAD + of(&c.name) + of(&c.args.to_string()),
    }
}

// What a prior reasoning block costs when replayed to `spec` — nothing at all
// on one that drops it.
//
// Which way it replays is `Reasoning::replay_for`'s call, the same one both
// encoders ask; only the sizing is this function's. Summed per block rather
// than over the joined prose, so the bound stays the higher of the two.
fn replayed_reasoning(r: &Reasoning, spec: &ModelSpec) -> usize {
    let text = || -> usize {
        r.content
            .iter()
            .filter_map(|c| match c {
                ReasoningContent::Text { text, .. } => Some(of(text)),
                ReasoningContent::Encrypted(_) => None,
            })
            .sum()
    };
    match r.replay_for(spec) {
        Replay::Signed { signature } => text() + of(signature),
        Replay::Encrypted { id, encrypted } => of(id) + of(encrypted),
        Replay::Demoted => text() + TAG_OVERHEAD,
        Replay::Dropped => 0,
    }
}

/// Notes ride outside the transcript — appended by the encoder, never stored —
/// so nothing walking the messages ever sees them. They come out of the same
/// budget all the same.
pub fn notes(notes: &[String]) -> usize {
    notes.iter().map(|n| of(n)).sum()
}

/// Tool schemas ride on every request, so they come out of the same budget the
/// transcript does.
pub fn tool_defs(tools: &[ToolDef]) -> usize {
    tools
        .iter()
        .map(|t| {
            BLOCK_OVERHEAD + of(&t.name) + of(&t.description) + of(&t.input_schema.to_string())
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Text, ToolResult};

    fn spec() -> ModelSpec {
        ModelSpec {
            max_output_tokens: 8_000,
            ..ModelSpec::test()
        }
    }

    #[test]
    fn the_estimate_errs_high_rather_than_low() {
        // 300 ASCII bytes is ~75 real tokens; the bound must sit above it.
        let m = vec![Message::user("a".repeat(300))];
        assert!(tokens(&m, &spec()) >= 100, "{}", tokens(&m, &spec()));
    }

    #[test]
    fn a_tool_result_counts_its_body_not_just_its_name() {
        let bare = vec![Message::tool_results(vec![ToolResult::text(
            "c",
            "read",
            "",
        )])];
        let full = vec![Message::tool_results(vec![ToolResult::text(
            "c",
            "read",
            "x".repeat(900),
        )])];
        assert!(tokens(&full, &spec()) - tokens(&bare, &spec()) >= 300);
    }

    #[test]
    fn an_image_is_not_free() {
        let with = vec![Message::User {
            content: vec![UserContent::Image(crate::message::Image::Url {
                url: "u".into(),
            })],
        }];
        let without = vec![Message::User {
            content: vec![UserContent::Text(Text { text: "u".into() })],
        }];
        assert!(tokens(&with, &spec()) > tokens(&without, &spec()) * 10);
    }
}
