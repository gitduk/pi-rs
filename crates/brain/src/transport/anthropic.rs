use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

use super::Transport;
use super::{Gaps, Shared};
use crate::error::{BrainError, Result};
use crate::message::{
    AssistantContent, Image, Message, Reasoning, Replay, ToolResult, ToolResultContent,
    UserContent, tagged,
};
use crate::model::{CacheControl, Format, ModelSpec, ThinkingControl};
use crate::request::{Request, ToolChoice};
use crate::stream::{BlockKind, StopReason, StreamEvent, Usage};

const API_VERSION: &str = "2023-06-01";
const MIN_THINKING_BUDGET: u32 = 1024;

pub struct Anthropic {
    http: reqwest::Client,
    api_key: Option<String>,
    // Session-lived, not per-request: what a host gets wrong it gets wrong
    // every turn, and the reader needs to hear it once.
    gaps: Shared,
}

impl Anthropic {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            gaps: Shared::new("anthropic"),
        }
    }
}

fn cache_control(spec: &ModelSpec) -> Result<CacheControl> {
    match spec.format {
        Format::Anthropic { cache_control } => Ok(cache_control),
        _ => Err(BrainError::Config(format!(
            "{} is not an anthropic-format model",
            spec.model
        ))),
    }
}

fn encode_image(img: &Image) -> Value {
    match img {
        Image::Base64 { media_type, data } => json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        }),
        Image::Url { url } => json!({
            "type": "image",
            "source": { "type": "url", "url": url },
        }),
    }
}

fn encode_tool_result(r: &ToolResult) -> Value {
    let has_image = r
        .content
        .iter()
        .any(|p| matches!(p, ToolResultContent::Image(_)));
    let content = if has_image {
        Value::Array(
            r.content
                .iter()
                .map(|p| match p {
                    ToolResultContent::Text(t) => json!({ "type": "text", "text": t.text }),
                    ToolResultContent::Json { value } => {
                        json!({ "type": "text", "text": value.to_string() })
                    }
                    ToolResultContent::Image(img) => encode_image(img),
                })
                .collect(),
        )
    } else {
        Value::String(r.flatten_text())
    };

    let block = json!({
        "type": "tool_result",
        "tool_use_id": r.call,
        "content": content,
        "is_error": r.is_error,
    });
    block
}

// Dress a stored reasoning block in this wire's shapes. Which way it leaves is
// `Reasoning::replay_for`'s call, shared with the estimate that sizes it.
fn encode_reasoning(r: &Reasoning, spec: &ModelSpec) -> Option<Value> {
    match r.replay_for(spec) {
        Replay::Signed { signature } => Some(json!({
            "type": "thinking",
            "thinking": r.text(),
            "signature": signature,
        })),
        // Tag-wrapped prior reasoning trips Anthropic's reasoning_extraction
        // classifier, so a demoted block ships as bare prose.
        Replay::Demoted => Some(json!({ "type": "text", "text": tagged(&r.text()) })),
        // No Anthropic spec ever encrypts one: the transport is chosen by the
        // same format `replay_for` reads.
        Replay::Encrypted { .. } | Replay::Dropped => None,
    }
}

fn encode_message(msg: &Message, spec: &ModelSpec) -> Option<Value> {
    match msg {
        // System prompts ride the top-level field, not the message array.
        Message::System { .. } => None,
        Message::User { content } => {
            let blocks: Vec<Value> = content
                .iter()
                .map(|b| match b {
                    UserContent::Text(t) => json!({ "type": "text", "text": t.text }),
                    UserContent::Image(img) => encode_image(img),
                    UserContent::ToolResult(r) => encode_tool_result(r),
                })
                .collect();
            Some(json!({ "role": "user", "content": blocks }))
        }
        Message::Assistant { content, .. } => {
            let blocks: Vec<Value> = content
                .iter()
                .filter_map(|b| match b {
                    AssistantContent::Text(t) => Some(json!({ "type": "text", "text": t.text })),
                    AssistantContent::Reasoning(r) => encode_reasoning(r, spec),
                    AssistantContent::ToolCall(call) => Some(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.args,
                    })),
                })
                .collect();
            (!blocks.is_empty()).then(|| json!({ "role": "assistant", "content": blocks }))
        }
    }
}

// Anthropic takes one `role:"user"` message per turn, so a turn's separate
// user entries join here. Responses wants them apart, which is why the join is
// the encoder's job and not the session view's.
fn encode_messages(msgs: &[Message], spec: &ModelSpec) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for mut msg in msgs.iter().filter_map(|m| encode_message(m, spec)) {
        let join = msg["role"] == "user" && out.last().is_some_and(|p| p["role"] == "user");
        if join
            && let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(prev) = out
                .last_mut()
                .and_then(|p| p.get_mut("content"))
                .and_then(|c| c.as_array_mut())
        {
            prev.append(blocks);
            continue;
        }
        out.push(msg);
    }
    out
}

// The cache marker this endpoint is known to take, if any.
fn marker(cache: CacheControl) -> Option<Value> {
    match cache {
        // An endpoint nobody has measured is not told to cache, per block no
        // less than per request: an unknown field is a 400 on some of them.
        CacheControl::Off => None,
        CacheControl::Standard => Some(json!({ "type": "ephemeral" })),
        CacheControl::LongTtl => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

pub(crate) fn build_body(spec: &ModelSpec, req: &Request) -> Value {
    let max_tokens = req
        .max_output_tokens
        .unwrap_or(spec.max_output_tokens)
        .min(spec.max_output_tokens);

    let mut body = json!({
        "model": spec.model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": encode_messages(&req.messages, spec),
    });

    // One field, and the API places the breakpoint itself — on the last
    // cacheable block, moving it forward as the conversation grows.
    if let Some(m) = marker(cache_control(spec).unwrap_or(CacheControl::Off)) {
        body["cache_control"] = m;
    }

    let system = req.system_text();
    if let Some(system) = system {
        body["system"] = json!([{ "type": "text", "text": system }]);
    }

    if !req.tools.is_empty() {
        body["tools"] = json!(
            req.tools
                .iter()
                .map(|t| json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                }))
                .collect::<Vec<_>>()
        );
        let choice = match &req.tool_choice {
            ToolChoice::Auto => None,
            ToolChoice::None => Some(json!({ "type": "none" })),
            // Fable/Mythos reject a forced choice outright; auto is the fallback.
            _ if !spec.can_force_tool => None,
            ToolChoice::Required => Some(json!({ "type": "any" })),
            ToolChoice::Named(name) => Some(json!({ "type": "tool", "name": name })),
        };
        if let Some(choice) = choice {
            body["tool_choice"] = choice;
        }
    }

    let thinking_on = match (spec.thinking, req.effort.as_anthropic()) {
        (Some(ThinkingControl::Adaptive), Some(effort)) => {
            body["thinking"] = json!({ "type": "adaptive" });
            body["output_config"] = json!({ "effort": effort });
            true
        }
        // Asking for no thinking has to be said out loud here: an adaptive
        // model left to itself thinks whenever it judges the input hard.
        (Some(ThinkingControl::Adaptive), None) => {
            body["thinking"] = json!({ "type": "disabled" });
            false
        }
        // Anthropic requires budget < max_tokens, and rejects any budget under
        // the floor: below that the request cannot carry thinking at all.
        (Some(ThinkingControl::Budget), Some(_)) if max_tokens > MIN_THINKING_BUDGET => {
            let ratio = req.effort.budget_ratio().unwrap_or(0.5);
            let budget =
                ((max_tokens as f64 * ratio) as u32).clamp(MIN_THINKING_BUDGET, max_tokens - 1);
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            true
        }
        _ => false,
    };

    // Thinking pins temperature to 1; any other value is rejected.
    if !thinking_on
        && spec.accepts_temperature
        && let Some(t) = req.finite_temperature()
    {
        body["temperature"] = json!(t);
    }

    body
}

fn stop_reason(raw: &str) -> StopReason {
    match raw {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

// Dialect endpoints (z.ai, Volces) defer the input-side counts to the final
// `message_delta`; counts there are cumulative, so a present field is current.
fn repair_usage(usage: &mut Usage, u: &Value) {
    if let Some(v) = u["input_tokens"].as_u64() {
        usage.input = v;
    }
    if let Some(v) = u["cache_read_input_tokens"].as_u64() {
        usage.cache_read = v;
    }
    if let Some(v) = u["cache_creation_input_tokens"].as_u64() {
        usage.cache_write = v;
    }
}

fn decode_frame(
    data: &Value,
    stop: &mut StopReason,
    usage: &mut Usage,
    gaps: &mut Gaps,
) -> Option<StreamEvent> {
    let event = gaps.owed(data, "frame", "type")?;
    // The block index rides only the content_block frames: message_start and
    // friends never carry one, and reading it there reports a gap every stream.
    match event {
        "message_start" => {
            let u = &data["message"]["usage"];
            *usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            };
            Some(StreamEvent::MessageStart { usage: *usage })
        }
        "content_block_start" => {
            let index = gaps.owed_index(data, "frame", "index");
            let block = &data["content_block"];
            let kind = match gaps.owed(block, event, "type")? {
                "text" => BlockKind::Text,
                "thinking" | "redacted_thinking" => BlockKind::Reasoning,
                "tool_use" => BlockKind::ToolCall {
                    id: block["id"].as_str().map(str::to_string),
                    name: gaps.owed(block, "tool_use", "name")?.to_string(),
                },
                // A block type added after this was written. Dropped, and the
                // model did say it: unreported, a new one reaches the reader
                // as the model having said less than it did.
                other => {
                    gaps.lost(event, other);
                    return None;
                }
            };
            Some(StreamEvent::BlockStart { index, kind })
        }
        "content_block_delta" => {
            let index = gaps.owed_index(data, "frame", "index");
            let delta = &data["delta"];
            match gaps.owed(delta, event, "type")? {
                "text_delta" => Some(StreamEvent::TextDelta {
                    index,
                    delta: gaps.owed(delta, "text_delta", "text")?.to_string(),
                }),
                "thinking_delta" => Some(StreamEvent::ReasoningDelta {
                    index,
                    delta: gaps.owed(delta, "thinking_delta", "thinking")?.to_string(),
                }),
                "signature_delta" => Some(StreamEvent::ReasoningSignature {
                    index,
                    signature: gaps
                        .owed(delta, "signature_delta", "signature")?
                        .to_string(),
                }),
                "input_json_delta" => Some(StreamEvent::ToolArgsDelta {
                    index,
                    delta: gaps
                        .owed(delta, "input_json_delta", "partial_json")?
                        .to_string(),
                }),
                other => {
                    gaps.lost(event, other);
                    None
                }
            }
        }
        "content_block_stop" => {
            let index = gaps.owed_index(data, "frame", "index");
            Some(StreamEvent::BlockEnd { index })
        }
        "message_delta" => {
            if let Some(r) = data["delta"]["stop_reason"].as_str() {
                *stop = stop_reason(r);
            }
            if let Some(o) = data["usage"]["output_tokens"].as_u64() {
                usage.output = o;
            }
            repair_usage(usage, &data["usage"]);
            None
        }
        "message_stop" => Some(StreamEvent::Done {
            stop: *stop,
            usage: *usage,
        }),
        other => {
            gaps.ignored("frame", other);
            None
        }
    }
}

#[async_trait]
impl Transport for Anthropic {
    fn gaps(&self) -> Vec<String> {
        self.gaps.drain()
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        cache_control(spec)?;
        let body = build_body(spec, req);
        let url = format!("{}/v1/messages", spec.base_url.trim_end_matches('/'));
        let mut call = self.http.post(&url);
        if let Some(key) = &self.api_key {
            call = call.header("x-api-key", key);
        }
        let call = call.header("anthropic-version", API_VERSION).json(&body);
        let resp = super::exchange("anthropic", url, spec, req, &body, call).await?;

        let mut stop = StopReason::default();
        let mut usage = Usage::default();
        let gaps = self.gaps.clone();

        let stream = resp.bytes_stream().eventsource().filter_map(move |frame| {
            let out = match frame {
                Err(e) => Some(Err(BrainError::Stream(e.to_string()))),
                Ok(frame) => match serde_json::from_str::<Value>(&frame.data) {
                    // `ping` and other bodyless frames carry no JSON.
                    Err(_) => None,
                    Ok(data) if data["type"] == "error" => {
                        tracing::warn!(
                            target: "pi::wire", wire = "anthropic",
                            detail = %data["error"], "error frame"
                        );
                        Some(Err(BrainError::Stream(data["error"].to_string())))
                    }
                    Ok(data) => {
                        decode_frame(&data, &mut stop, &mut usage, &mut gaps.frame()).map(Ok)
                    }
                },
            };
            futures::future::ready(out)
        });

        Ok(stream.boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ReasoningContent;
    use crate::model::ReplayThinking;

    fn spec() -> ModelSpec {
        ModelSpec {
            base_url: "https://api.anthropic.com".into(),
            format: Format::Anthropic {
                cache_control: CacheControl::LongTtl,
            },
            thinking: Some(ThinkingControl::Budget),
            ..ModelSpec::test()
        }
    }

    // Feed frames through the decoder the way the wire delivers them, and hand
    // back the usage it ends with.
    fn stream(frames: &[Value]) -> Usage {
        let mut stop = StopReason::default();
        let mut usage = Usage::default();
        let mut gaps = Gaps::new("anthropic");
        for frame in frames {
            let _ = decode_frame(frame, &mut stop, &mut usage, &mut gaps);
        }
        usage
    }

    #[test]
    fn dialect_input_counts_deferred_to_the_final_delta_still_land() {
        // z.ai reports zeros at `message_start` and the real counts only in
        // the final `message_delta`; the repair fills the start's zeros in.
        let done = stream(&[
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 0, "output_tokens": 0 } } }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": {
                    "input_tokens": 11214,
                    "cache_read_input_tokens": 600,
                    "cache_creation_input_tokens": 40,
                    "output_tokens": 32
                }
            }),
            json!({ "type": "message_stop" }),
        ]);
        assert_eq!(done.input, 11214);
        assert_eq!(done.cache_read, 600);
        assert_eq!(done.cache_write, 40);
        assert_eq!(done.output, 32);
    }

    #[test]
    fn an_official_delta_leaves_the_start_s_counts_standing() {
        // The official shape states input once, at the start, and the delta
        // carries output only — the repair must not touch what is already set.
        let done = stream(&[
            json!({ "type": "message_start", "message": { "usage": { "input_tokens": 472, "output_tokens": 2 } } }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": 89 }
            }),
            json!({ "type": "message_stop" }),
        ]);
        assert_eq!(done.input, 472);
        assert_eq!(done.output, 89);
        assert_eq!(done.cache_read, 0);
        assert_eq!(done.cache_write, 0);
    }

    // The estimate and the encoder must answer the same question. They are
    // separate walks of the same transcript — one decides when to compact, the
    // other decides what ships — and a gap between them is invisible: the
    // budget simply runs out early, and what pays is real context dropped to
    // make room for bytes that were never sent. Measured on real sessions the
    // gap was 53%, because prior reasoning was counted whatever the spec did
    // with it.
    #[test]
    fn the_estimate_counts_what_the_wire_carries() {
        let thinking = "z".repeat(20_000);
        let with = |replay| {
            let mut s = spec();
            s.replay_thinking = replay;
            s
        };
        let req = |_: &ModelSpec| Request {
            messages: vec![
                Message::user("go"),
                Message::Assistant {
                    content: vec![AssistantContent::Reasoning(Reasoning {
                        id: None,
                        // No signature, and a foreign author: exactly the block
                        // a demotion decides about.
                        content: vec![ReasoningContent::Text {
                            text: thinking.clone(),
                            signature: None,
                        }],
                        by: None,
                    })],
                },
            ],
            ..Default::default()
        };

        let dropped = with(ReplayThinking::Off);
        let kept = with(ReplayThinking::Tagged);

        let sent = |s: &ModelSpec| build_body(s, &req(s)).to_string();
        assert!(
            !sent(&dropped).contains(&thinking),
            "it was dropped from the body"
        );
        assert!(sent(&kept).contains(&thinking), "it rode the body");

        let counted = |s: &ModelSpec| crate::estimate::tokens(&req(s).messages, s);
        let bare = crate::estimate::tokens(&[Message::user("go")], &dropped);
        assert!(
            counted(&dropped) < bare + 100,
            "a block that never leaves must cost nothing: {} vs {bare}",
            counted(&dropped)
        );
        assert!(
            counted(&kept) > counted(&dropped) * 10,
            "a block that does leave must be paid for: {} vs {}",
            counted(&kept),
            counted(&dropped)
        );
    }
}
