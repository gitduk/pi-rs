use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

use super::Transport;
use crate::catalog::{AnthropicCompat, ModelSpec, ThinkingReplay, ThinkingSupport, Wire};
use crate::error::{BrainError, Result};
use crate::message::{
    AssistantContent, Image, Message, ProviderCallId, Reasoning, ReasoningContent, ToolResult,
    ToolResultContent, UserContent,
};
use crate::request::{Effort, Request, ToolChoice};
use crate::stream::{BlockKind, StopReason, StreamEvent, Usage};

const API_VERSION: &str = "2023-06-01";
const MIN_THINKING_BUDGET: u32 = 1024;

pub struct Anthropic {
    http: reqwest::Client,
    api_key: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
        }
    }
}

fn compat(spec: &ModelSpec) -> Result<&AnthropicCompat> {
    match &spec.wire {
        Wire::Anthropic(c) => Ok(c),
        _ => Err(BrainError::Config(format!(
            "{} is not an anthropic-wire model",
            spec.id
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

fn encode_tool_result(r: &ToolResult, c: &AnthropicCompat) -> Value {
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

    // The wire keys on the provider's own id; ours is a local handle only.
    let wire_id = r
        .provider
        .as_ref()
        .map(|p| p.0.clone())
        .unwrap_or_else(|| r.call.0.clone());
    let mut block = json!({
        "type": "tool_result",
        "tool_use_id": wire_id,
        "content": content,
        "is_error": r.is_error,
    });
    if c.tool_result_id_alias {
        block["id"] = json!(wire_id);
    }
    block
}

/// Decide how a stored reasoning block leaves for this model. A block minted by
/// another transport or model can never replay its signature.
fn encode_reasoning(r: &Reasoning, spec: &ModelSpec) -> Option<Value> {
    let native = r
        .origin
        .as_ref()
        .is_some_and(|o| o.transport == "anthropic" && o.model == spec.wire_id);

    let text: String = r
        .content
        .iter()
        .filter_map(|c| match c {
            ReasoningContent::Text { text, .. } => Some(text.as_str()),
            ReasoningContent::Encrypted(_) => None,
        })
        .collect();
    let signature = r.content.iter().find_map(|c| match c {
        ReasoningContent::Text { signature, .. } => signature.clone(),
        ReasoningContent::Encrypted(_) => None,
    });

    if native && let Some(sig) = signature {
        return Some(json!({ "type": "thinking", "thinking": text, "signature": sig }));
    }
    if text.is_empty() {
        return None;
    }
    match spec.thinking_replay {
        // Tag-wrapped prior reasoning trips Anthropic's reasoning_extraction
        // classifier, so a demoted block ships as bare prose.
        ThinkingReplay::Signed | ThinkingReplay::BareProse => {
            Some(json!({ "type": "text", "text": text }))
        }
        ThinkingReplay::Tagged => {
            Some(json!({ "type": "text", "text": format!("<think>\n{text}\n</think>") }))
        }
        ThinkingReplay::Drop => None,
    }
}

fn encode_message(msg: &Message, spec: &ModelSpec, c: &AnthropicCompat) -> Option<Value> {
    match msg {
        // System prompts ride the top-level field, not the message array.
        Message::System { .. } => None,
        Message::User { content } => {
            let blocks: Vec<Value> = content
                .iter()
                .map(|b| match b {
                    UserContent::Text(t) => json!({ "type": "text", "text": t.text }),
                    UserContent::Image(img) => encode_image(img),
                    UserContent::ToolResult(r) => encode_tool_result(r, c),
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
                        let wire_id = call
                            .provider
                            .as_ref()
                            .map(|p| p.0.clone())
                            .unwrap_or_else(|| call.id.0.clone());
                        Some(json!({
                            "type": "tool_use",
                            "id": wire_id,
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

pub(crate) fn build_body(spec: &ModelSpec, req: &Request, c: &AnthropicCompat) -> Value {
    let max_tokens = req
        .max_output_tokens
        .unwrap_or(spec.max_output_tokens)
        .min(spec.max_output_tokens);

    let mut body = json!({
        "model": spec.wire_id,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": req
            .messages
            .iter()
            .filter_map(|m| encode_message(m, spec, c))
            .collect::<Vec<_>>(),
    });

    let system = req.system.clone().or_else(|| {
        req.messages.iter().find_map(|m| match m {
            Message::System { content } => Some(content.clone()),
            _ => None,
        })
    });
    if let Some(system) = system {
        let mut block = json!({ "type": "text", "text": system });
        if spec.caps.cache_breakpoints {
            block["cache_control"] = if c.long_cache_retention {
                json!({ "type": "ephemeral", "ttl": "1h" })
            } else {
                json!({ "type": "ephemeral" })
            };
        }
        body["system"] = json!([block]);
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
            _ if !c.forced_tool_choice => None,
            ToolChoice::Required => Some(json!({ "type": "any" })),
            ToolChoice::Named(name) => Some(json!({ "type": "tool", "name": name })),
        };
        if let Some(choice) = choice {
            body["tool_choice"] = choice;
        }
    }

    // Anthropic requires budget < max_tokens, and rejects any budget under the
    // floor: below that the request cannot carry thinking at all.
    let thinking_on = spec.caps.thinking == Some(ThinkingSupport::Budget)
        && req.effort != Effort::Off
        && max_tokens > MIN_THINKING_BUDGET;
    if thinking_on {
        let ratio = req.effort.budget_ratio().unwrap_or(0.5);
        let budget =
            ((max_tokens as f64 * ratio) as u32).clamp(MIN_THINKING_BUDGET, max_tokens - 1);
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    } else if c.sampling_params
        && let Some(t) = req.temperature
    {
        // Extended thinking pins temperature to 1; any other value is rejected.
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

fn decode_frame(data: &Value, stop: &mut StopReason, usage: &mut Usage) -> Option<StreamEvent> {
    let index = data["index"].as_u64().unwrap_or(0) as usize;
    match data["type"].as_str()? {
        "message_start" => {
            let u = &data["message"]["usage"];
            *usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            };
            Some(StreamEvent::MessageStart {
                id: data["message"]["id"].as_str().map(str::to_string),
                usage: *usage,
            })
        }
        "content_block_start" => {
            let block = &data["content_block"];
            let kind = match block["type"].as_str()? {
                "text" => BlockKind::Text,
                "thinking" | "redacted_thinking" => BlockKind::Reasoning,
                "tool_use" => BlockKind::ToolCall {
                    provider: block["id"].as_str().map(|s| ProviderCallId(s.to_string())),
                    name: block["name"].as_str().unwrap_or_default().to_string(),
                },
                _ => return None,
            };
            Some(StreamEvent::BlockStart { index, kind })
        }
        "content_block_delta" => {
            let delta = &data["delta"];
            match delta["type"].as_str()? {
                "text_delta" => Some(StreamEvent::TextDelta {
                    index,
                    delta: delta["text"].as_str()?.to_string(),
                }),
                "thinking_delta" => Some(StreamEvent::ReasoningDelta {
                    index,
                    delta: delta["thinking"].as_str()?.to_string(),
                }),
                "signature_delta" => Some(StreamEvent::ReasoningSignature {
                    index,
                    signature: delta["signature"].as_str()?.to_string(),
                }),
                "input_json_delta" => Some(StreamEvent::ToolArgsDelta {
                    index,
                    delta: delta["partial_json"].as_str()?.to_string(),
                }),
                _ => None,
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
        _ => None,
    }
}

#[async_trait]
impl Transport for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let c = compat(spec)?;
        let body = build_body(spec, req, c);

        let resp = self
            .http
            .post(format!(
                "{}/v1/messages",
                spec.base_url.trim_end_matches('/')
            ))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BrainError::Api {
                transport: "anthropic",
                status,
                body,
            });
        }

        let mut stop = StopReason::default();
        let mut usage = Usage::default();

        let stream = resp.bytes_stream().eventsource().filter_map(move |frame| {
            let out = match frame {
                Err(e) => Some(Err(BrainError::Stream(e.to_string()))),
                Ok(frame) => match serde_json::from_str::<Value>(&frame.data) {
                    // `ping` and other bodyless frames carry no JSON.
                    Err(_) => None,
                    Ok(data) if data["type"] == "error" => {
                        Some(Err(BrainError::Stream(data["error"].to_string())))
                    }
                    Ok(data) => decode_frame(&data, &mut stop, &mut usage).map(Ok),
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
    use crate::catalog::{Capabilities, Pricing};
    use crate::message::{Origin, Text, ToolCall, ToolCallId, ToolResultContent};

    fn spec(c: AnthropicCompat) -> ModelSpec {
        ModelSpec {
            id: "opus-5".into(),
            wire_id: "claude-opus-5".into(),
            base_url: "https://api.anthropic.com".into(),
            wire: Wire::Anthropic(c),
            context_window: 200_000,
            max_output_tokens: 32_000,
            caps: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                thinking: Some(ThinkingSupport::Budget),
                cache_breakpoints: true,
            },
            thinking_replay: ThinkingReplay::Signed,
            pricing: Pricing::default(),
        }
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
        let c = AnthropicCompat {
            sampling_params: false,
            ..Default::default()
        };
        let req = Request {
            temperature: Some(0.7),
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert!(body.get("temperature").is_none());

        let c = AnthropicCompat::default();
        let body = build_body(&spec(c.clone()), &req, &c);
        assert_eq!(body["temperature"], 0.7);
    }

    #[test]
    fn forced_tool_choice_downgrades_to_auto() {
        let c = AnthropicCompat {
            forced_tool_choice: false,
            ..Default::default()
        };
        let req = Request {
            tools: vec![tool()],
            tool_choice: ToolChoice::Required,
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert!(body.get("tool_choice").is_none());

        let c = AnthropicCompat::default();
        let body = build_body(&spec(c.clone()), &req, &c);
        assert_eq!(body["tool_choice"]["type"], "any");
    }

    #[test]
    fn thinking_budget_stays_under_max_tokens_and_suppresses_temperature() {
        let c = AnthropicCompat::default();
        let req = Request {
            effort: Effort::High,
            temperature: Some(0.7),
            max_output_tokens: Some(4_000),
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        let budget = body["thinking"]["budget_tokens"].as_u64().unwrap();
        assert!(budget < 4_000, "budget {budget} must stay below max_tokens");
        assert!(budget >= MIN_THINKING_BUDGET as u64);
        assert!(
            body.get("temperature").is_none(),
            "thinking pins temperature to 1"
        );
    }

    #[test]
    fn foreign_reasoning_is_demoted_rather_than_replayed() {
        let c = AnthropicCompat::default();
        let foreign = Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: "prior".into(),
                    signature: Some("sig-from-elsewhere".into()),
                }],
                origin: Some(Origin {
                    transport: "openai".into(),
                    model: "gpt-5".into(),
                }),
            })],
        };
        let req = Request {
            messages: vec![foreign],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(
            block["type"], "text",
            "a foreign signature must never replay"
        );
        assert_eq!(block["text"], "prior");
    }

    #[test]
    fn native_reasoning_replays_with_its_signature() {
        let c = AnthropicCompat::default();
        let native = Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: "prior".into(),
                    signature: Some("sig".into()),
                }],
                origin: Some(Origin {
                    transport: "anthropic".into(),
                    model: "claude-opus-5".into(),
                }),
            })],
        };
        let req = Request {
            messages: vec![native],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "thinking");
        assert_eq!(block["signature"], "sig");
    }

    #[test]
    fn tool_result_keys_on_the_provider_id_and_aliases_it_when_asked() {
        let result = ToolResult {
            call: ToolCallId("local_1".into()),
            provider: Some(ProviderCallId("toolu_abc".into())),
            name: "read".into(),
            content: vec![ToolResultContent::Text(Text { text: "ok".into() })],
            is_error: false,
            useless: false,
        };
        let req = Request {
            messages: vec![Message::tool_results(vec![result])],
            ..Default::default()
        };

        let c = AnthropicCompat::default();
        let body = build_body(&spec(c.clone()), &req, &c);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["tool_use_id"], "toolu_abc");
        assert!(block.get("id").is_none());

        let c = AnthropicCompat {
            tool_result_id_alias: true,
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert_eq!(body["messages"][0]["content"][0]["id"], "toolu_abc");
    }

    #[test]
    fn assistant_tool_call_uses_the_provider_id_on_the_wire() {
        let c = AnthropicCompat::default();
        let msg = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: ToolCallId("local_1".into()),
                provider: Some(ProviderCallId("toolu_abc".into())),
                name: "read".into(),
                args: json!({ "path": "a.rs" }),
            })],
        };
        let req = Request {
            messages: vec![msg],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert_eq!(body["messages"][0]["content"][0]["id"], "toolu_abc");
    }

    #[test]
    fn system_rides_the_top_level_field_with_a_cache_breakpoint() {
        let c = AnthropicCompat::default();
        let req = Request {
            messages: vec![Message::System {
                content: "be terse".into(),
            }],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert_eq!(body["system"][0]["text"], "be terse");
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn thinking_is_skipped_rather_than_underflowing_a_tiny_budget() {
        let c = AnthropicCompat::default();
        let req = Request {
            effort: Effort::High,
            max_output_tokens: Some(512),
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert!(body.get("thinking").is_none(), "512 < the 1024 floor");
    }
}
