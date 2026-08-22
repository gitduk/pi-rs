use crate::message::{AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent};
use crate::request::ToolDef;

/// Bytes per token. Deliberately low: overestimating trips compaction a little
/// early, underestimating trips a 400 from the provider mid-run.
const BYTES_PER_TOKEN: usize = 3;

/// Framing every message carries regardless of content.
const MESSAGE_OVERHEAD: usize = 8;

/// Framing a tool call or result carries beyond its payload.
const BLOCK_OVERHEAD: usize = 12;

/// What an image costs once encoded. A crude constant beats no accounting: an
/// unmeasured image is the one thing that blows a budget silently.
const IMAGE_TOKENS: usize = 1_500;

/// A bound on what a string costs. Public because the system prompt and tool
/// schemas come out of the same budget the transcript does.
pub fn text(s: &str) -> usize {
    s.len().div_ceil(BYTES_PER_TOKEN)
}

use text as of;

/// A bound on what a transcript costs.
///
/// No tokenizer is embedded: this decides *when* to compact, and for that a
/// bound in the right direction beats an exact count for a tokenizer the
/// provider may not even be using.
pub fn tokens(messages: &[Message]) -> usize {
    messages.iter().map(message).sum()
}

pub fn message(m: &Message) -> usize {
    MESSAGE_OVERHEAD
        + match m {
            Message::System { content } => of(content),
            Message::User { content } => content.iter().map(user_block).sum(),
            Message::Assistant { content, .. } => content.iter().map(assistant_block).sum(),
        }
}

fn user_block(b: &UserContent) -> usize {
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

fn assistant_block(b: &AssistantContent) -> usize {
    match b {
        AssistantContent::Text(t) => of(&t.text),
        AssistantContent::Reasoning(r) => r
            .content
            .iter()
            .map(|c| match c {
                ReasoningContent::Text { text, .. } => of(text),
                ReasoningContent::Encrypted(s) => of(s),
            })
            .sum(),
        AssistantContent::ToolCall(c) => BLOCK_OVERHEAD + of(&c.name) + of(&c.args.to_string()),
    }
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
    use crate::message::{Text, ToolCallId, ToolResult};

    #[test]
    fn the_estimate_errs_high_rather_than_low() {
        // 300 ASCII bytes is ~75 real tokens; the bound must sit above it.
        let m = vec![Message::user("a".repeat(300))];
        assert!(tokens(&m) >= 100, "{}", tokens(&m));
    }

    #[test]
    fn a_tool_result_counts_its_body_not_just_its_name() {
        let bare = vec![Message::tool_results(vec![ToolResult::text(
            ToolCallId("c".into()),
            "read",
            "",
        )])];
        let full = vec![Message::tool_results(vec![ToolResult::text(
            ToolCallId("c".into()),
            "read",
            "x".repeat(900),
        )])];
        assert!(tokens(&full) - tokens(&bare) >= 300);
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
        assert!(tokens(&with) > tokens(&without) * 10);
    }
}
