use crate::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Replay, ToolResult, ToolResultContent,
    UserContent,
};
use crate::model::ModelSpec;
use crate::request::ToolDef;
use serde_json::Value;

// Bytes per token. Deliberately low: overestimating trips compaction a little
// early, underestimating trips a 400 from the provider mid-run.
const BYTES_PER_TOKEN: usize = 3;

/// Framing every message carries regardless of content.
pub const MESSAGE_OVERHEAD: usize = 8;

// Framing a tool call or result carries beyond its payload.
const BLOCK_OVERHEAD: usize = 12;

// A crude constant beats no accounting for image cost. Ceiling for a
// standard-resolution image; a high-res one costs several times this.
const IMAGE_TOKENS: usize = 1_568;

/// A bound on what a string costs. Public because the system prompt and tool
/// schemas come out of the same budget the transcript does.
pub fn text(s: &str) -> usize {
    of_bytes(s.len())
}

/// The same bound for something already measured in bytes.
fn of_bytes(n: usize) -> usize {
    n.div_ceil(BYTES_PER_TOKEN)
}

/// Upper bound on serde_json's compact form: every escape at its widest
/// (`\u00XX`), every number at a double's longest. Costly to serialize twice.
fn json_bytes(v: &Value) -> usize {
    match v {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        // -1.7976931348623157e308: the longest a shortest-form double can be.
        Value::Number(_) => 24,
        Value::String(s) => quoted(s),
        Value::Array(items) => 2 + items.iter().map(json_bytes).sum::<usize>() + items.len(),
        // A pair is `"key":value,`: quotes and colon, then the comma.
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| quoted(k) + 2 + json_bytes(v))
                .sum::<usize>()
                + map.len()
        }
    }
}

/// A string as JSON writes it: the quotes, a byte each, and for a byte that
/// escapes the widest form it could take.
fn quoted(s: &str) -> usize {
    2 + s
        .bytes()
        .map(|b| match b {
            b'"' | b'\\' => 2,
            0x00..=0x1f => 6,
            _ => 1,
        })
        .sum::<usize>()
}

use text as of;

// `<think>` and its closing tag, the wrapper a demoted block ships inside.
const TAG_OVERHEAD: usize = 6;

/// A bound on what a transcript costs to send to `spec`, not a token count:
/// no tokenizer is embedded, so this only decides *when* to compact.
///
/// Replay-aware: a model that drops prior reasoning is sent none of it, so
/// counting it here would compact against bytes that never leave.
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
            result_head(r) + r.content.iter().map(result_part).sum::<usize>()
        }
    }
}

/// What a result costs once its content is a notice rather than the body: the
/// block framing and both names stay, because the `tool_use` it answers still
/// has to find it. A planner replacing a result prices it through this.
pub fn omitted_result(r: &ToolResult, notice: &str) -> usize {
    result_head(r) + of(notice)
}

// Framing plus both names: the id so a caller can match the result to its
// call. One spelling, so a planner and a sender never price it differently.
fn result_head(r: &ToolResult) -> usize {
    BLOCK_OVERHEAD + of(&r.call) + of(&r.name)
}

fn result_part(p: &ToolResultContent) -> usize {
    match p {
        ToolResultContent::Text(t) => of(&t.text),
        ToolResultContent::Json { value } => of_bytes(json_bytes(value)),
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
        AssistantContent::ToolCall(c) => {
            BLOCK_OVERHEAD + of(&c.id) + of(&c.name) + of_bytes(json_bytes(&c.args))
        }
    }
}

// What a prior reasoning block costs when replayed to `spec` (0 if dropped).
// Summed per block, not joined prose, so the bound stays the higher of the two.
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

/// Tool schemas ride on every request, so they come out of the same budget the
/// transcript does.
pub fn tool_defs(tools: &[ToolDef]) -> usize {
    tools
        .iter()
        .map(|t| {
            BLOCK_OVERHEAD
                + of(&t.name)
                + of(&t.description)
                + of_bytes(json_bytes(&t.input_schema))
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Text, ToolCall, ToolResult};
    use serde_json::json;

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
            "c", "read", "",
        )])];
        let full = vec![Message::tool_results(vec![ToolResult::text(
            "c",
            "read",
            "x".repeat(900),
        )])];
        assert!(tokens(&full, &spec()) - tokens(&bare, &spec()) >= 300);
    }

    #[test]
    fn a_serialized_value_is_bounded_without_being_serialized() {
        // The bound must not run low: a little high compacts a bit early; too low
        // and the provider rejects the request as too large.
        for value in [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(f64::MAX),
            json!(f64::MIN),
            json!(18_446_744_073_709_551_615u64),
            json!("quote \" backslash \\ newline \n tab \t nul \u{0}"),
            json!("日本語はバイトそのまま"),
            json!({"a": [1, 2, {"b": "c"}], "d": null, "": []}),
            json!([[], {}, "", 0]),
        ] {
            let printed = value.to_string();
            assert!(
                json_bytes(&value) >= printed.len(),
                "{printed}: bound {} < {}",
                json_bytes(&value),
                printed.len()
            );
        }
    }

    // What a call of the given arguments costs, the whole message included.
    fn call(args: serde_json::Value) -> usize {
        let m = vec![Message::Assistant {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "c".into(),
                name: "read".into(),
                args,
            })],
        }];
        tokens(&m, &spec())
    }

    #[test]
    fn a_tool_call_is_sized_by_its_arguments() {
        let small = call(json!({}));
        let large = call(json!({"path": "a".repeat(900)}));
        assert!(large - small >= 300, "{small} -> {large}");
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
