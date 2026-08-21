use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

use super::Transport;
use crate::catalog::{
    MaxTokensField, ModelSpec, OpenAiCompat, ReasoningField, ThinkingReplay, ThinkingSupport, Wire,
};
use crate::error::{BrainError, Result};
use crate::message::{
    AssistantContent, Image, Message, ProviderCallId, Reasoning, ReasoningContent, UserContent,
};
use crate::request::{Request, ToolChoice};
use crate::stream::{BlockKind, StopReason, StreamEvent, Usage};

/// Synthetic block indices. OpenAI has no global block index, so one is
/// assigned per kind; the ordering is what the final message preserves.
const IDX_REASONING: usize = 0;
const IDX_TEXT: usize = 1;
const IDX_TOOL_BASE: usize = 100;

pub struct OpenAi {
    http: reqwest::Client,
    api_key: String,
}

impl OpenAi {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key: api_key.into(),
        }
    }
}

fn compat(spec: &ModelSpec) -> Result<&OpenAiCompat> {
    match &spec.wire {
        Wire::OpenAi(c) => Ok(c),
        _ => Err(BrainError::Config(format!(
            "{} is not an openai-wire model",
            spec.id
        ))),
    }
}

/// Mistral accepts exactly nine alphanumeric characters and rejects anything
/// else, including the ids every other provider mints.
fn mistral_id(id: &str) -> String {
    let kept: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(9)
        .collect();
    format!("{kept:a<9}")
}

fn wire_call_id(local: &str, provider: Option<&ProviderCallId>, c: &OpenAiCompat) -> String {
    let id = provider.map(|p| p.0.as_str()).unwrap_or(local);
    if c.mistral_tool_ids {
        mistral_id(id)
    } else {
        id.to_string()
    }
}

fn encode_image(img: &Image) -> Value {
    let url = match img {
        Image::Url { url } => url.clone(),
        Image::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    };
    json!({ "type": "image_url", "image_url": { "url": url } })
}

fn reasoning_text(r: &Reasoning) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            ReasoningContent::Text { text, .. } => Some(text.as_str()),
            ReasoningContent::Encrypted(_) => None,
        })
        .collect()
}

fn reasoning_key(f: ReasoningField) -> &'static str {
    match f {
        ReasoningField::ReasoningContent => "reasoning_content",
        ReasoningField::Reasoning => "reasoning",
        ReasoningField::ReasoningText => "reasoning_text",
    }
}

fn encode(msgs: &[Message], spec: &ModelSpec, c: &OpenAiCompat) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    for msg in msgs {
        match msg {
            Message::System { content } => {
                out.push(json!({ "role": "system", "content": content }))
            }

            Message::User { content } => {
                let results: Vec<&crate::message::ToolResult> = content
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::ToolResult(r) => Some(r),
                        _ => None,
                    })
                    .collect();

                // One `role: "tool"` message per result; the stored shape packs
                // them into a single user message and only splits on this wire.
                for r in &results {
                    let mut m = json!({
                        "role": "tool",
                        "tool_call_id": wire_call_id(&r.call.0, r.provider.as_ref(), c),
                        "content": r.flatten_text(),
                    });
                    if c.tool_result_name {
                        m["name"] = json!(r.name);
                    }
                    out.push(m);
                }

                let rest: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::Text(t) => Some(json!({ "type": "text", "text": t.text })),
                        UserContent::Image(img) => Some(encode_image(img)),
                        UserContent::ToolResult(_) => None,
                    })
                    .collect();

                if !rest.is_empty() {
                    // Some hosts reject a user turn that directly answers a tool
                    // message; a stub assistant turn keeps the sequence legal.
                    if c.assistant_after_tool_result && !results.is_empty() {
                        out.push(json!({ "role": "assistant", "content": "" }));
                    }
                    out.push(json!({ "role": "user", "content": rest }));
                }
            }

            Message::Assistant { content, .. } => {
                let mut text = String::new();
                let mut calls: Vec<Value> = Vec::new();
                let mut reasoning: Option<String> = None;

                for b in content {
                    match b {
                        AssistantContent::Text(t) => text.push_str(&t.text),
                        AssistantContent::ToolCall(call) => calls.push(json!({
                            "id": wire_call_id(&call.id.0, call.provider.as_ref(), c),
                            "type": "function",
                            "function": { "name": call.name, "arguments": call.args.to_string() },
                        })),
                        AssistantContent::Reasoning(r) => {
                            let native = r.origin.as_ref().is_some_and(|o| {
                                o.transport == "openai" && o.model == spec.wire_id
                            });
                            let body = reasoning_text(r);
                            if body.is_empty() {
                                continue;
                            }
                            match (native, spec.thinking_replay) {
                                (_, ThinkingReplay::Drop) => {}
                                (true, _) => reasoning = Some(body),
                                (false, ThinkingReplay::Tagged) => {
                                    text.push_str(&format!("<think>\n{body}\n</think>\n"))
                                }
                                (false, _) => text.push_str(&body),
                            }
                        }
                    }
                }

                if text.is_empty() && calls.is_empty() && reasoning.is_none() {
                    continue;
                }
                let mut m = json!({ "role": "assistant", "content": text });
                if !calls.is_empty() {
                    m["tool_calls"] = Value::Array(calls);
                }
                if let Some(body) = reasoning {
                    m[reasoning_key(c.reasoning_field)] = json!(body);
                }
                out.push(m);
            }
        }
    }

    if !c.multiple_system_messages {
        coalesce_system(&mut out);
    }
    out
}

/// Strict chat templates accept only one leading system message. Joining them
/// costs KV-cache reuse, so this runs only where the host demands it.
fn coalesce_system(msgs: &mut Vec<Value>) {
    let lead = msgs.iter().take_while(|m| m["role"] == "system").count();
    if lead <= 1 {
        return;
    }
    let joined = msgs[..lead]
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    msgs.splice(..lead, [json!({ "role": "system", "content": joined })]);
}

pub(crate) fn build_body(spec: &ModelSpec, req: &Request, c: &OpenAiCompat) -> Value {
    let max_tokens = req
        .max_output_tokens
        .unwrap_or(spec.max_output_tokens)
        .min(spec.max_output_tokens);

    let mut messages = encode(&req.messages, spec, c);
    if let Some(system) = &req.system {
        messages.insert(0, json!({ "role": "system", "content": system }));
        if !c.multiple_system_messages {
            coalesce_system(&mut messages);
        }
    }

    let mut body = json!({
        "model": spec.wire_id,
        "stream": true,
        "messages": messages,
    });

    let key = match c.max_tokens_field {
        MaxTokensField::MaxTokens => "max_tokens",
        MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
    };
    body[key] = json!(max_tokens);

    if c.usage_in_streaming {
        body["stream_options"] = json!({ "include_usage": true });
    }
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if !req.tools.is_empty() {
        body["tools"] = json!(
            req.tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                }))
                .collect::<Vec<_>>()
        );
        body["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Named(name) => json!({ "type": "function", "function": { "name": name } }),
        };
    }
    if spec.caps.thinking == Some(ThinkingSupport::Effort)
        && c.reasoning_effort
        && let Some(e) = req.effort.as_openai()
    {
        body["reasoning_effort"] = json!(e);
    }

    body
}

fn finish_reason(raw: &str) -> StopReason {
    match raw {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        _ => StopReason::EndTurn,
    }
}

#[derive(Default)]
struct Decoder {
    stop: StopReason,
    usage: Usage,
    started: std::collections::HashSet<usize>,
    text_open: bool,
    reasoning_open: bool,
}

impl Decoder {
    fn frame(&mut self, data: &Value, c: &OpenAiCompat) -> Vec<StreamEvent> {
        let mut out = Vec::new();

        if let Some(u) = data.get("usage").filter(|u| !u.is_null()) {
            self.usage.input = u["prompt_tokens"].as_u64().unwrap_or(self.usage.input);
            self.usage.output = u["completion_tokens"].as_u64().unwrap_or(self.usage.output);
            self.usage.cache_read = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(self.usage.cache_read);
        }

        // A usage-only terminal frame carries no choices.
        let Some(choice) = data["choices"].get(0) else {
            return out;
        };

        let delta = &choice["delta"];

        let reasoning = delta[reasoning_key(c.reasoning_field)]
            .as_str()
            .or_else(|| delta["reasoning_content"].as_str());
        if let Some(r) = reasoning.filter(|r| !r.is_empty()) {
            if !self.reasoning_open {
                self.reasoning_open = true;
                out.push(StreamEvent::BlockStart {
                    index: IDX_REASONING,
                    kind: BlockKind::Reasoning,
                });
            }
            out.push(StreamEvent::ReasoningDelta {
                index: IDX_REASONING,
                delta: r.to_string(),
            });
        }

        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            if !self.text_open {
                self.text_open = true;
                out.push(StreamEvent::BlockStart {
                    index: IDX_TEXT,
                    kind: BlockKind::Text,
                });
            }
            out.push(StreamEvent::TextDelta {
                index: IDX_TEXT,
                delta: t.to_string(),
            });
        }

        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let slot = call["index"].as_u64().unwrap_or(0) as usize;
                let index = IDX_TOOL_BASE + slot;
                if self.started.insert(index) {
                    out.push(StreamEvent::BlockStart {
                        index,
                        kind: BlockKind::ToolCall {
                            provider: call["id"].as_str().map(|s| ProviderCallId(s.to_string())),
                            name: call["function"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        },
                    });
                }
                if let Some(args) = call["function"]["arguments"]
                    .as_str()
                    .filter(|a| !a.is_empty())
                {
                    out.push(StreamEvent::ToolArgsDelta {
                        index,
                        delta: args.to_string(),
                    });
                }
            }
        }

        if let Some(r) = choice["finish_reason"].as_str().filter(|r| !r.is_empty()) {
            self.stop = finish_reason(r);
        }

        out
    }

    /// `[DONE]` is the wire's only reliable terminator: a host may omit
    /// `stream_options`, or send usage on a frame that still carries choices.
    fn done(&self) -> StreamEvent {
        StreamEvent::Done {
            stop: self.stop,
            usage: self.usage,
        }
    }
}

#[async_trait]
impl Transport for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let c = compat(spec)?.clone();
        let body = build_body(spec, req, &c);

        let resp = self
            .http
            .post(format!(
                "{}/chat/completions",
                spec.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BrainError::Api {
                transport: "openai",
                status,
                body,
            });
        }

        let mut dec = Decoder::default();

        let stream = resp
            .bytes_stream()
            .eventsource()
            .flat_map(move |frame| {
                let events: Vec<Result<StreamEvent>> = match frame {
                    Err(e) => vec![Err(BrainError::Stream(e.to_string()))],
                    // `[DONE]` is a sentinel, not JSON.
                    Ok(f) if f.data.trim() == "[DONE]" => vec![Ok(dec.done())],
                    Ok(f) => match serde_json::from_str::<Value>(&f.data) {
                        Err(e) => vec![Err(BrainError::Stream(e.to_string()))],
                        Ok(data) if data.get("error").is_some_and(|e| !e.is_null()) => {
                            vec![Err(BrainError::Stream(data["error"].to_string()))]
                        }
                        Ok(data) => dec.frame(&data, &c).into_iter().map(Ok).collect(),
                    },
                };
                futures::stream::iter(events)
            })
            .boxed();

        Ok(stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Capabilities, Pricing};
    use crate::message::{Text, ToolCallId, ToolResult, ToolResultContent};

    fn spec(c: OpenAiCompat) -> ModelSpec {
        ModelSpec {
            id: "local".into(),
            wire_id: "qwen3".into(),
            base_url: "http://127.0.0.1:8000/v1".into(),
            wire: Wire::OpenAi(c),
            context_window: 128_000,
            max_output_tokens: 8_000,
            caps: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                vision: false,
                thinking: Some(ThinkingSupport::Effort),
                cache_breakpoints: false,
            },
            thinking_replay: ThinkingReplay::Tagged,
            pricing: Pricing::default(),
        }
    }

    fn result(call: &str, name: &str) -> ToolResult {
        ToolResult {
            call: ToolCallId(call.into()),
            provider: None,
            name: name.into(),
            content: vec![ToolResultContent::Text(Text { text: "ok".into() })],
            is_error: false,
            useless: false,
        }
    }

    #[test]
    fn one_user_message_splits_into_one_tool_message_per_result() {
        let c = OpenAiCompat::default();
        let req = Request {
            messages: vec![Message::tool_results(vec![
                result("c1", "read"),
                result("c2", "grep"),
            ])],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "c1");
        assert_eq!(msgs[1]["tool_call_id"], "c2");
        assert!(msgs[0].get("name").is_none());
    }

    #[test]
    fn tool_result_name_added_only_where_the_wire_needs_it() {
        let c = OpenAiCompat {
            tool_result_name: true,
            ..Default::default()
        };
        let req = Request {
            messages: vec![Message::tool_results(vec![result("c1", "read")])],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        assert_eq!(body["messages"][0]["name"], "read");
    }

    #[test]
    fn a_user_turn_after_tool_results_gets_a_stub_assistant_when_required() {
        let mut msg_content = vec![crate::message::UserContent::ToolResult(result(
            "c1", "read",
        ))];
        msg_content.push(crate::message::UserContent::Text(Text {
            text: "now what".into(),
        }));
        let msgs = vec![Message::User {
            content: msg_content,
        }];

        let c = OpenAiCompat::default();
        let body = build_body(
            &spec(c.clone()),
            &Request {
                messages: msgs.clone(),
                ..Default::default()
            },
            &c,
        );
        let roles: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].clone())
            .collect();
        assert_eq!(roles, vec![json!("tool"), json!("user")]);

        let c = OpenAiCompat {
            assistant_after_tool_result: true,
            ..Default::default()
        };
        let body = build_body(
            &spec(c.clone()),
            &Request {
                messages: msgs,
                ..Default::default()
            },
            &c,
        );
        let roles: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].clone())
            .collect();
        assert_eq!(
            roles,
            vec![json!("tool"), json!("assistant"), json!("user")]
        );
    }

    #[test]
    fn max_tokens_field_follows_the_host() {
        let c = OpenAiCompat::default();
        let body = build_body(&spec(c.clone()), &Request::default(), &c);
        assert_eq!(body["max_completion_tokens"], 8_000);
        assert!(body.get("max_tokens").is_none());

        let c = OpenAiCompat {
            max_tokens_field: MaxTokensField::MaxTokens,
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &Request::default(), &c);
        assert_eq!(body["max_tokens"], 8_000);
    }

    #[test]
    fn leading_system_messages_coalesce_only_for_strict_templates() {
        let msgs = vec![
            Message::System {
                content: "a".into(),
            },
            Message::System {
                content: "b".into(),
            },
            Message::user("hi"),
        ];

        let c = OpenAiCompat::default();
        let body = build_body(
            &spec(c.clone()),
            &Request {
                messages: msgs.clone(),
                ..Default::default()
            },
            &c,
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);

        let c = OpenAiCompat {
            multiple_system_messages: false,
            ..Default::default()
        };
        let body = build_body(
            &spec(c.clone()),
            &Request {
                messages: msgs,
                ..Default::default()
            },
            &c,
        );
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0]["content"], "a\n\nb");
    }

    #[test]
    fn mistral_ids_are_exactly_nine_alphanumerics() {
        assert_eq!(mistral_id("toolu_01ABCdefGH"), "toolu01AB");
        assert_eq!(mistral_id("ab"), "abaaaaaaa");
        assert_eq!(mistral_id("").len(), 9);
    }

    #[test]
    fn foreign_reasoning_is_folded_into_text_not_the_reasoning_field() {
        let c = OpenAiCompat::default();
        let msg = Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: "prior".into(),
                    signature: None,
                }],
                origin: Some(crate::message::Origin {
                    transport: "anthropic".into(),
                    model: "claude-opus-5".into(),
                }),
            })],
        };
        let req = Request {
            messages: vec![msg],
            ..Default::default()
        };
        let body = build_body(&spec(c.clone()), &req, &c);
        let m = &body["messages"][0];
        assert!(m.get("reasoning_content").is_none());
        assert_eq!(m["content"], "<think>\nprior\n</think>\n");
    }

    #[test]
    fn streaming_tool_call_slots_map_to_distinct_blocks() {
        let c = OpenAiCompat::default();
        let mut dec = Decoder::default();
        let frame = json!({
            "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "id": "a", "function": { "name": "read", "arguments": "{\"p\":" } },
                { "index": 1, "id": "b", "function": { "name": "grep", "arguments": "{}" } }
            ]}}]
        });
        let events = dec.frame(&frame, &c);
        let starts: Vec<usize> = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::BlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![IDX_TOOL_BASE, IDX_TOOL_BASE + 1]);

        // A second frame continues slot 0 without reopening it.
        let more = dec.frame(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "1}" } }
            ]}}]}),
            &c,
        );
        assert_eq!(
            more,
            vec![StreamEvent::ToolArgsDelta {
                index: IDX_TOOL_BASE,
                delta: "1}".into()
            }]
        );
    }

    #[test]
    fn done_fires_on_the_sentinel_carrying_the_usage_seen_so_far() {
        let c = OpenAiCompat::default();
        let mut dec = Decoder::default();
        dec.frame(
            &json!({ "choices": [{ "delta": { "content": "hi" }, "finish_reason": "stop" }] }),
            &c,
        );
        // Usage rides a trailing choice-less frame; Done must come after it.
        dec.frame(
            &json!({ "choices": [], "usage": { "prompt_tokens": 9, "completion_tokens": 2 } }),
            &c,
        );

        assert_eq!(
            dec.done(),
            StreamEvent::Done {
                stop: StopReason::EndTurn,
                usage: Usage {
                    input: 9,
                    output: 2,
                    ..Default::default()
                },
            }
        );
    }

    #[test]
    fn a_dropped_reasoning_turn_does_not_emit_an_empty_assistant_message() {
        let c = OpenAiCompat::default();
        let mut s = spec(c.clone());
        s.thinking_replay = ThinkingReplay::Drop;
        let msg = Message::Assistant {
            id: None,
            content: vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: "prior".into(),
                    signature: None,
                }],
                origin: Some(crate::message::Origin {
                    transport: "anthropic".into(),
                    model: "claude-opus-5".into(),
                }),
            })],
        };
        let body = build_body(
            &s,
            &Request {
                messages: vec![msg],
                ..Default::default()
            },
            &c,
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }
}
