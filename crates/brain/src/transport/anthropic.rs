use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

use super::Transport;
use crate::model::{CacheControl, Format, ModelSpec, ThinkingControl};
use crate::error::{BrainError, Result};
use crate::message::{
    AssistantContent, Image, Message, Reasoning, Replay, ToolResult,
    ToolResultContent, UserContent, tagged,
};
use crate::request::{Request, ToolChoice};
use super::{Gaps, Shared};
use crate::stream::{BlockKind, StopReason, StreamEvent, Usage};

const API_VERSION: &str = "2023-06-01";
const MIN_THINKING_BUDGET: u32 = 1024;

pub struct Anthropic {
    http: reqwest::Client,
    api_key: String,
    /// Session-lived, not per-request: what a host gets wrong it gets wrong
    /// every turn, and the reader needs to hear it once.
    gaps: Shared,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
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

/// Dress a stored reasoning block in this wire's shapes. Which way it leaves is
/// `Reasoning::replay_for`'s call, shared with the estimate that sizes it.
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
                    AssistantContent::ToolCall(call) => {
                        Some(json!({
                            "type": "tool_use",
                            "id": call.id,
                            "name": call.name,
                            "input": call.args,
                        }))
                    }
                })
                .collect();
            (!blocks.is_empty()).then(|| json!({ "role": "assistant", "content": blocks }))
        }
    }
}

/// Anthropic takes one `role:"user"` message per turn, so a turn's separate
/// user entries join here. Responses wants them apart, which is why the join is
/// the encoder's job and not the session view's.
fn encode_messages(msgs: &[Message], spec: &ModelSpec) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for msg in msgs.iter().filter_map(|m| encode_message(m, spec)) {
        let join = msg["role"] == "user" && out.last().is_some_and(|p| p["role"] == "user");
        if join
            && let Some(blocks) = msg["content"].as_array()
            && let Some(prev) = out.last_mut().and_then(|p| p["content"].as_array_mut())
        {
            prev.extend(blocks.iter().cloned());
            continue;
        }
        out.push(msg);
    }
    out
}

/// Notes ride the tail of the last user message. A breakpoint caches up to and
/// including the block it sits on, so anything after it is outside the cache —
/// which is exactly where something that changes every turn belongs. Put them
/// in `system` instead and every breakpoint in the message array sits behind
/// content that just changed, so none of them ever hit.
fn append_notes(messages: &mut [Value], notes: &[String]) {
    if notes.is_empty() {
        return;
    }
    // Merged into the last user block rather than appended as their own, which
    // is what the other wire does: this one requires user and assistant to
    // alternate, so a trailing user item after a user turn is a 400.
    let Some(last) = messages
        .iter_mut()
        .rfind(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_array_mut())
    else {
        // Nothing to merge into. A transcript always opens with a user turn, so
        // this is unreachable — and unreported it would be a plan the model was
        // never shown, on a request that looks ordinary.
        tracing::warn!(
            target: "pi::wire", format = "anthropic", notes = notes.len(),
            "no user message to carry the notes; they were dropped"
        );
        return;
    };
    last.extend(notes.iter().map(|n| json!({ "type": "text", "text": n })));
}

pub(crate) fn build_body(spec: &ModelSpec, req: &Request) -> Value {
    let max_tokens = req
        .max_output_tokens
        .unwrap_or(spec.max_output_tokens)
        .min(spec.max_output_tokens);

    let mut messages = encode_messages(&req.messages, spec);
    append_notes(&mut messages, &req.notes);

    let mut body = json!({
        "model": spec.model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": messages,
    });

    // One field, and the API places the breakpoint itself — on the last
    // cacheable block, moving it forward as the conversation grows. Until now
    // the only breakpoint sat on the system block, so the transcript, which is
    // the largest part of the request, was re-read at full price every turn.
    match cache_control(spec) {
        Ok(CacheControl::Standard) => body["cache_control"] = json!({ "type": "ephemeral" }),
        Ok(CacheControl::LongTtl) => {
            body["cache_control"] = json!({ "type": "ephemeral", "ttl": "1h" })
        }
        _ => {}
    }

    let system = req.system.clone().or_else(|| {
        req.messages.iter().find_map(|m| match m {
            Message::System { content } => Some(content.clone()),
            _ => None,
        })
    });
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
        && let Some(t) = req.temperature
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

fn decode_frame(
    data: &Value,
    stop: &mut StopReason,
    usage: &mut Usage,
    gaps: &mut Gaps,
) -> Option<StreamEvent> {
    let index = data["index"].as_u64().unwrap_or(0) as usize;
    let event = gaps.owed(data, "frame", "type")?;
    match event {
        "message_start" => {
            let u = &data["message"]["usage"];
            *usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            };
            Some(StreamEvent::MessageStart {
                usage: *usage,
            })
        }
        "content_block_start" => {
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
                    signature: gaps.owed(delta, "signature_delta", "signature")?.to_string(),
                }),
                "input_json_delta" => Some(StreamEvent::ToolArgsDelta {
                    index,
                    delta: gaps.owed(delta, "input_json_delta", "partial_json")?.to_string(),
                }),
                other => {
                    gaps.lost(event, other);
                    None
                }
            }
        }
        "content_block_stop" => Some(StreamEvent::BlockEnd { index }),
        "message_delta" => {
            if let Some(r) = data["delta"]["stop_reason"].as_str() {
                *stop = stop_reason(r);
            }
            if let Some(o) = data["usage"]["output_tokens"].as_u64() {
                usage.output = o;
            }
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
        let call = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body);
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
    use crate::model::CacheControl;
    use crate::request::Effort;
    use crate::message::{Image, ReasoningContent, Text, ToolCall, ToolResultContent};
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

    fn result(call: &str, name: &str) -> ToolResult {
        ToolResult::text(call, name, "ok")
    }

    /// One block per message, the way `Session::context` hands them over now.
    fn apart() -> Vec<Message> {
        vec![
            Message::user("go"),
            Message::Assistant {
                content: vec![
                    AssistantContent::ToolCall(ToolCall { id: "c1".into(), name: "read".into(), args: json!({}) }),
                    AssistantContent::ToolCall(ToolCall { id: "c2".into(), name: "grep".into(), args: json!({}) }),
                ],
            },
            Message::tool_results(vec![result("c1", "read")]),
            Message::tool_results(vec![result("c2", "grep")]),
            Message::User { content: vec![UserContent::Image(Image::Url { url: "http://x/i.png".into() })] },
            Message::user("next"),
        ]
    }

    /// The same conversation as the view used to merge it, before the join
    /// moved into the encoder.
    fn together() -> Vec<Message> {
        let mut msgs = apart();
        let tail: Vec<UserContent> = msgs
            .drain(2..)
            .flat_map(|m| match m {
                Message::User { content } => content,
                other => panic!("only user messages join, got {other:?}"),
            })
            .collect();
        msgs.push(Message::User { content: tail });
        msgs
    }

    #[test]
    fn the_join_moving_into_the_encoder_changed_no_bytes() {
        // `Session::context` used to merge a turn's user entries before the
        // encoder saw them. It no longer does — Responses wants them apart —
        // so the encoder joins instead, and the two orders must agree.
        let body = |m: Vec<Message>| {
            build_body(&spec(), &Request { messages: m, ..Default::default() })
        };
        assert_eq!(body(apart()), body(together()));
        assert_eq!(body(apart())["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_turn_that_encodes_to_nothing_no_longer_splits_the_turn_around_it() {
        // The one place the two orders disagree, and the reason the new one is
        // right: under `Drop` an assistant of pure reasoning encodes to nothing,
        // and joining before that filter left the users on either side shipping
        // as two consecutive `role:"user"` messages.
        let mut s = spec();
        s.replay_thinking = ReplayThinking::Off;
        let msgs = vec![
            Message::user("a"),
            Message::Assistant {
                content: vec![AssistantContent::Reasoning(Reasoning {
                    id: None,
                    content: vec![ReasoningContent::Text { text: "t".into(), signature: None }],
                    by: None,
                })],
            },
            Message::user("b"),
        ];

        let joined = encode_messages(&msgs, &s);
        assert_eq!(joined.len(), 1, "{joined:?}");
        assert_eq!(joined[0]["content"].as_array().unwrap().len(), 2);

        // What joining before the filter produced, pinned so the change is not
        // mistaken for a regression later.
        let before: Vec<Value> = msgs.iter().filter_map(|m| encode_message(m, &s)).collect();
        assert_eq!(before.len(), 2);
    }

    fn adaptive_spec() -> ModelSpec {
        let mut adaptive = spec();
        adaptive.thinking = Some(ThinkingControl::Adaptive);
        adaptive
    }

    fn tool() -> crate::request::ToolDef {
        crate::request::ToolDef {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({ "type": "object" }),
        }
    }

    #[test]
    fn sampling_params_dropped_when_the_model_rejects_them() {
        let req = Request {
            temperature: Some(0.7),
            ..Default::default()
        };
        let mut rejects = spec();
        rejects.accepts_temperature = false;
        assert!(build_body(&rejects, &req).get("temperature").is_none());

        assert_eq!(build_body(&spec(), &req)["temperature"], 0.7);
    }

    #[test]
    fn forced_tool_choice_downgrades_to_auto() {
        let req = Request {
            tools: vec![tool()],
            tool_choice: ToolChoice::Required,
            ..Default::default()
        };
        let mut rejects = spec();
        rejects.can_force_tool = false;
        assert!(build_body(&rejects, &req).get("tool_choice").is_none());

        let body = build_body(&spec(), &req);
        assert_eq!(body["tool_choice"]["type"], "any");
    }

    #[test]
    fn adaptive_sends_output_config_effort_and_suppresses_temperature() {
        let req = Request {
            effort: Effort::Medium,
            temperature: Some(0.7),
            max_output_tokens: Some(4_000),
            ..Default::default()
        };
        let body = build_body(&adaptive_spec(), &req);
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
        assert_eq!(body["output_config"], json!({ "effort": "medium" }));
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "budget_tokens is the shape 4.7+ rejects"
        );
        assert!(
            body.get("temperature").is_none(),
            "thinking pins temperature to 1"
        );
    }

    #[test]
    fn adaptive_says_disabled_rather_than_omitting_thinking() {
        let req = Request {
            effort: Effort::Off,
            temperature: Some(0.7),
            ..Default::default()
        };
        let body = build_body(&adaptive_spec(), &req);
        // Omitting it would let the model decide to think anyway.
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        assert!(body.get("output_config").is_none());
        assert_eq!(body["temperature"], json!(0.7));
    }

    #[test]
    fn thinking_budget_stays_under_max_tokens_and_suppresses_temperature() {
        let req = Request {
            effort: Effort::High,
            temperature: Some(0.7),
            max_output_tokens: Some(4_000),
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(budget < 4_000, "budget {budget} must stay below max_tokens");
        assert!(budget >= MIN_THINKING_BUDGET as u64);
        assert!(
            body.get("temperature").is_none(),
            "thinking pins temperature to 1"
        );
    }

    #[test]
    fn a_foreign_signature_never_replays_as_one() {
        let foreign = Message::Assistant {
            content: vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: "prior".into(),
                    signature: Some("sig-from-elsewhere".into()),
                }],
                by: Some("another-model".into()),
            })],
        };
        let req = Request {
            messages: vec![foreign],
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(
            block["type"], "text",
            "a foreign signature must never replay"
        );
        assert_eq!(block["text"], "<think>\nprior\n</think>");
    }

    #[test]
    fn native_reasoning_replays_with_its_signature() {
        let native = Message::Assistant {
            content: vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: "prior".into(),
                    signature: Some("sig".into()),
                }],
                by: Some(spec().model),
            })],
        };
        let req = Request {
            messages: vec![native],
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["signature"], "sig");
    }

    #[test]
    fn a_tool_result_keys_on_the_call_id_and_nothing_else() {
        let result = ToolResult {
            call: "toolu_abc".into(),
            name: "read".into(),
            content: vec![ToolResultContent::Text(Text { text: "ok".into() })],
            is_error: false,
            useless: false,
        };
        let req = Request {
            messages: vec![Message::tool_results(vec![result])],
            ..Default::default()
        };

        let body = build_body(&spec(), &req);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["tool_use_id"], "toolu_abc");
        assert!(block.get("id").is_none());

    }

    #[test]
    fn assistant_tool_call_sends_the_id_untouched() {
        let msg = Message::Assistant {
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: "toolu_abc".into(),
                name: "read".into(),
                args: json!({ "path": "a.rs" }),
            })],
        };
        let req = Request {
            messages: vec![msg],
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        assert_eq!(body["messages"][0]["content"][0]["id"], "toolu_abc");
    }

    #[test]
    fn system_rides_the_top_level_field_and_leaves_the_message_array() {
        let req = Request {
            messages: vec![Message::System {
                content: "be terse".into(),
            }],
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        assert_eq!(body["system"][0]["text"], "be terse");
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    /// One field, placed once. The breakpoint used to sit on the system block
    /// and nowhere else, so the transcript — much the largest part of the
    /// request — was re-read at full price every turn.
    #[test]
    fn caching_is_asked_for_once_at_the_top_and_not_per_block() {
        let req = Request {
            system: Some("be terse".into()),
            messages: vec![Message::user("go")],
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        assert_eq!(body["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["messages"][0].get("cache_control").is_none());

        let mut short = spec();
        short.format = Format::Anthropic { cache_control: CacheControl::Standard };
        assert_eq!(
            build_body(&short, &req)["cache_control"],
            json!({ "type": "ephemeral" })
        );

        // An endpoint nobody measured is not told to cache at all.
        let mut cold = spec();
        cold.format = Format::Anthropic { cache_control: CacheControl::Off };
        assert!(build_body(&cold, &req).get("cache_control").is_none());
    }

    /// A breakpoint caches up to and including the block it lands on, so
    /// anything after it is outside — which is where something recomputed every
    /// turn has to sit, or the prefix breaks on it.
    /// The estimate and the encoder must answer the same question. They are
    /// separate walks of the same transcript — one decides when to compact, the
    /// other decides what ships — and a gap between them is invisible: the
    /// budget simply runs out early, and what pays is real context dropped to
    /// make room for bytes that were never sent. Measured on real sessions the
    /// gap was 53%, because prior reasoning was counted whatever the spec did
    /// with it.
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
        assert!(!sent(&dropped).contains(&thinking), "it was dropped from the body");
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

    #[test]
    fn a_note_rides_the_tail_of_the_last_user_message() {
        let req = Request {
            system: Some("be terse".into()),
            messages: vec![
                Message::user("first"),
                Message::assistant_text("mid"),
                Message::user("last"),
            ],
            notes: vec!["[true only this turn]".into()],
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "a note opens no message of its own");
        assert_eq!(
            msgs[2]["content"].as_array().unwrap().len(),
            2,
            "it is the last block of the last user message"
        );
        assert_eq!(msgs[2]["content"][1]["text"], "[true only this turn]");
        // Never in the system prompt: every message-array breakpoint would then
        // sit behind content that changed, and none of them would ever hit.
        assert_eq!(body["system"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn thinking_is_skipped_rather_than_underflowing_a_tiny_budget() {
        let req = Request {
            effort: Effort::High,
            max_output_tokens: Some(512),
            ..Default::default()
        };
        let body = build_body(&spec(), &req);
        assert!(body.get("thinking").is_none(), "512 < the 1024 floor");
    }
}
