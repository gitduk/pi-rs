//! The Chat Completions wire (`POST /chat/completions`): OpenAI's older chat
//! shape, and the one DeepSeek's `deepseek-v4-*` family speaks natively.
//!
//! Responses is a different protocol from this one, not a newer version of it
//! — different message roles, different usage accounting, different tool
//! framing. Both are OpenAI-format in the config's sense, but the wire shapes
//! share only the auth header and the SSE transport, so this transport is its
//! own decoder rather than a mode on the Responses one.

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};

use super::{Shared, Transport};
use crate::error::{BrainError, Result};
use crate::message::{
    AssistantContent, Image, Message, Replay, ToolResult, UserContent, tagged,
};
use crate::model::{Format, ModelSpec, ThinkingControl};
use crate::request::{Request, ToolChoice};
use crate::stream::{BlockKind, StopReason, StreamEvent, Usage};

pub struct ChatCompletions {
    http: reqwest::Client,
    api_key: Option<String>,
    gaps: Shared,
}

impl ChatCompletions {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            gaps: Shared::new("chat"),
        }
    }
}

fn check_format(spec: &ModelSpec) -> Result<()> {
    match spec.format {
        Format::Chat => Ok(()),
        _ => Err(BrainError::Config(format!(
            "{} is not a chat-completions-format model",
            spec.model
        ))),
    }
}

// The one thing the wire cannot say. A tool message has no `is_error` — the
// result body is plain text — so a failure that is not marked in the text
// reads to the model as a result.
const FAILED: &str = "[tool error]";

fn encode_image(img: &Image) -> Value {
    let url = match img {
        Image::Url { url } => url.clone(),
        Image::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    };
    json!({ "type": "image_url", "image_url": { "url": url } })
}

// A tool result becomes a `tool` message keyed by the call id, exactly as the
// Chat Completions schema wants it. `flatten_text` renders each part the way
// the other two wires read a string result body.
fn encode_tool_result(r: &ToolResult) -> Value {
    let mut parts: Vec<String> = Vec::new();
    if r.is_error {
        parts.push(FAILED.to_string());
    }
    parts.push(r.flatten_text());
    json!({
        "role": "tool",
        "tool_call_id": r.call,
        "content": parts.join("\n"),
    })
}

// One stored assistant turn as the chat wire wants it: content and tool calls
// on the same message. The wire keeps no ordering between the two, so the
// merge is lossless up to that.
fn encode_assistant(content: &[AssistantContent], spec: &ModelSpec, out: &mut Vec<Value>) {
    let mut text = String::new();
    let mut calls: Vec<Value> = Vec::new();
    for b in content {
        match b {
            AssistantContent::Text(t) => text.push_str(&t.text),
            AssistantContent::Reasoning(r) => {
                // Demoted reasoning ships as ` thinking`-wrapped prose, the
                // same demotion the other wires do. Signed and encrypted are
                // unreachable for this format, so only demotion ships.
                if let Replay::Demoted = r.replay_for(spec) {
                    text.push_str(&tagged(&r.text()));
                }
            }
            AssistantContent::ToolCall(call) => calls.push(json!({
                "id": call.id,
                "type": "function",
                "function": {
                    "name": call.name,
                    "arguments": call.args.to_string(),
                },
            })),
        }
    }
    if !text.is_empty() || !calls.is_empty() {
        let mut msg = json!({
            "role": "assistant",
            "content": text,
        });
        if !calls.is_empty() {
            msg["tool_calls"] = Value::Array(calls);
        }
        out.push(msg);
    }
}

// The `messages` array. System rides the top-level field; a user turn with a
// tool result splits into a user message and a `tool` message, in order.
fn encode(msgs: &[Message], spec: &ModelSpec) -> Vec<Value> {
    let mut out = Vec::new();
    for m in msgs {
        match m {
            Message::System { .. } => {}
            Message::User { content } => {
                let mut blocks: Vec<Value> = Vec::new();
                for b in content {
                    match b {
                        UserContent::Text(t) => {
                            blocks.push(json!({ "type": "text", "text": t.text }));
                        }
                        UserContent::Image(img) => blocks.push(encode_image(img)),
                        UserContent::ToolResult(r) => {
                            if !blocks.is_empty() {
                                out.push(json!({ "role": "user", "content": blocks }));
                                blocks = Vec::new();
                            }
                            out.push(encode_tool_result(r));
                        }
                    }
                }
                if !blocks.is_empty() {
                    out.push(json!({ "role": "user", "content": blocks }));
                }
            }
            Message::Assistant { content } => encode_assistant(content, spec, &mut out),
        }
    }
    out
}

pub(crate) fn build_body(spec: &ModelSpec, req: &Request) -> Value {
    let max_output_tokens = req
        .max_output_tokens
        .unwrap_or(spec.max_output_tokens)
        .min(spec.max_output_tokens);

    let mut body = json!({
        "model": spec.model,
        "stream": true,
        "messages": encode(&req.messages, spec),
        "max_tokens": max_output_tokens,
    });

    let system = req.system_text();
    if let Some(system) = system {
        body["messages"].as_array_mut().unwrap().insert(
            0,
            json!({ "role": "system", "content": system }),
        );
    }
    // The notes are not a statement of fact, so they ride the last user turn
    // rather than their own role.
    if !req.notes.is_empty() {
        let notes = req
            .notes
            .iter()
            .map(|n| json!({ "type": "text", "text": n }))
            .collect::<Vec<_>>();
        body["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "role": "user", "content": notes }));
    }

    // The thinking switch. DeepSeek's chat wire takes `thinking: {type}` with
    // `reasoning_effort`; a model that takes no thinking instruction gets no
    // such field.
    if let Some(ThinkingControl::Effort) = spec.thinking {
        if let Some(effort) = req.effort.as_openai() {
            body["thinking"] = json!({ "type": "enabled", "reasoning_effort": effort });
        } else {
            body["thinking"] = json!({ "type": "disabled" });
        }
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
            ToolChoice::Named(name) => {
                json!({ "type": "function", "function": { "name": name } })
            }
        };
    }

    if spec.accepts_temperature
        && let Some(t) = req.temperature
    {
        body["temperature"] = json!(t);
    }

    body
}

// DeepSeek's chat wire reports the cached and uncached prompt halves
// separately: `prompt_tokens` is their sum, so the fresh input is the
// difference. A host that omits `prompt_cache_miss_tokens` (plain OpenAI)
// leaves the miss to be derived from the other two; only when the whole
// usage block is missing does every token bill as fresh — the same direction
// the Responses subtraction takes, and the safe one for a budget.
fn usage_of(u: &Value) -> Usage {
    let cache_read = u["prompt_cache_hit_tokens"].as_u64().unwrap_or(0);
    let miss = u["prompt_cache_miss_tokens"].as_u64().unwrap_or_else(|| {
        u["prompt_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(cache_read)
    });
    Usage {
        input: miss,
        output: u["completion_tokens"].as_u64().unwrap_or(0),
        cache_read,
        cache_write: 0,
    }
}

fn stop_of(finish: &str) -> StopReason {
    match finish {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        // "stop", an unrecognised reason, and a resource-interrupted turn all
        // end the turn; the accumulator turns it into a tool use when calls
        // are pending.
        _ => StopReason::EndTurn,
    }
}

// The accumulator's block indices, synthesized because the chat wire has no
// native output index. Indexes are handed out in arrival order from one
// counter — reasoning reaches the wire before text, so it keeps that position
// after the fold — and a block reuses its index for every later delta.
struct Decoder {
    text_index: Option<usize>,
    reasoning_index: Option<usize>,
    // Wire tool index → accumulator index, assigned on first sight.
    tools: std::collections::BTreeMap<usize, usize>,
    // Wire tool indexes that already got their `BlockStart`. A host may resend
    // a call's id or name on a later delta; re-emitting `BlockStart` would
    // overwrite the id and name the accumulator already holds.
    started: std::collections::BTreeSet<usize>,
    next_free: usize,
}

impl Decoder {
    fn new() -> Self {
        Self {
            text_index: None,
            reasoning_index: None,
            tools: std::collections::BTreeMap::new(),
            started: std::collections::BTreeSet::new(),
            next_free: 0,
        }
    }

    fn take_index(&mut self) -> usize {
        let i = self.next_free;
        self.next_free += 1;
        i
    }

    fn frame(&mut self, data: &Value) -> Vec<StreamEvent> {
        let Some(choices) = data["choices"].as_array() else {
            return Vec::new();
        };
        let Some(choice) = choices.first() else {
            return Vec::new();
        };
        let delta = &choice["delta"];
        let mut events = Vec::new();

        if let Some(text) = delta["content"].as_str()
            && !text.is_empty()
        {
            let index = match self.text_index {
                Some(i) => i,
                None => {
                    let i = self.take_index();
                    self.text_index = Some(i);
                    i
                }
            };
            events.push(StreamEvent::TextDelta {
                index,
                delta: text.to_string(),
            });
        }

        if let Some(think) = delta["reasoning_content"].as_str()
            && !think.is_empty()
        {
            let index = match self.reasoning_index {
                Some(i) => i,
                None => {
                    let i = self.take_index();
                    self.reasoning_index = Some(i);
                    i
                }
            };
            events.push(StreamEvent::ReasoningDelta {
                index,
                delta: think.to_string(),
            });
        }

        // Tool calls stream one `tool_calls` array per chunk. The id and name
        // arrive on the first delta of a call, the arguments over the rest;
        // the block is started once, so a later delta can only add arguments.
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let wire = call["index"].as_u64().unwrap_or(0) as usize;
                let index = match self.tools.get(&wire) {
                    Some(&i) => i,
                    None => {
                        let i = self.take_index();
                        self.tools.insert(wire, i);
                        i
                    }
                };
                if self.started.insert(wire) {
                    events.push(StreamEvent::BlockStart {
                        index,
                        kind: BlockKind::ToolCall {
                            id: call["id"].as_str().map(str::to_string),
                            name: call["function"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                        },
                    });
                }
                if let Some(args) = call["function"]["arguments"].as_str()
                    && !args.is_empty()
                {
                    events.push(StreamEvent::ToolArgsDelta {
                        index,
                        delta: args.to_string(),
                    });
                }
            }
        }

        // The terminal chunk states `finish_reason` and carries the usage. The
        // documentation is explicit that no usage-only chunk is emitted: the
        // statistics ride this one.
        if let Some(finish) = choice["finish_reason"].as_str() {
            events.push(StreamEvent::Done {
                stop: stop_of(finish),
                usage: usage_of(&data["usage"]),
            });
        }

        events
    }
}

#[async_trait]
impl Transport for ChatCompletions {
    fn gaps(&self) -> Vec<String> {
        self.gaps.drain()
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        check_format(spec)?;
        let body = build_body(spec, req);
        let url = format!("{}/chat/completions", spec.base_url.trim_end_matches('/'));
        let mut call = self.http.post(&url);
        if let Some(key) = &self.api_key {
            call = call.bearer_auth(key);
        }
        let call = call.json(&body);
        let resp = super::exchange("chat", url, spec, req, &body, call).await?;

        let mut dec = Decoder::new();

        let stream = resp
            .bytes_stream()
            .eventsource()
            .flat_map(move |item| {
                let events: Vec<Result<StreamEvent>> = match item {
                    Err(e) => vec![Err(BrainError::Stream(e.to_string()))],
                    // `[DONE]` is data-only and carries no JSON; the chunks
                    // before it already delivered the turn.
                    Ok(f) if f.data == "[DONE]" => Vec::new(),
                    Ok(f) => match serde_json::from_str::<Value>(&f.data) {
                        Err(e) => vec![Err(BrainError::Stream(e.to_string()))],
                        Ok(data) if data.get("error").is_some_and(|e| !e.is_null()) => {
                            tracing::warn!(
                                target: "pi::wire", format = "chat",
                                detail = %data["error"], "error frame"
                            );
                            vec![Err(BrainError::Stream(data["error"].to_string()))]
                        }
                        Ok(data) => dec.frame(&data).into_iter().map(Ok).collect(),
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
    use crate::message::{Reasoning, ReasoningContent, Text, ToolResult, ToolResultContent};
    use crate::request::Effort;
    use crate::stream::Accumulator;

    fn spec() -> ModelSpec {
        ModelSpec {
            base_url: "https://api.deepseek.com".into(),
            format: Format::Chat,
            context_window: 64_000,
            thinking: Some(ThinkingControl::Effort),
            ..ModelSpec::test()
        }
    }

    fn body(req: Request) -> Value {
        build_body(&spec(), &req)
    }

    #[test]
    fn the_deepseek_cache_halves_split_into_usage() {
        // DeepSeek's chat wire reports hit and miss separately; the fresh
        // input is the miss half, not `prompt_tokens` (which is their sum).
        let u = usage_of(&json!({
            "prompt_tokens": 1_000,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200,
            "completion_tokens": 500,
            "total_tokens": 1_500,
        }));
        assert_eq!(u.input, 200);
        assert_eq!(u.cache_read, 800);
        assert_eq!(u.cache_write, 0);
        assert_eq!(u.output, 500);
    }

    #[test]
    fn a_host_without_the_cache_split_bills_the_whole_prompt_fresh() {
        // Plain OpenAI reports only `prompt_tokens`; the miss half is absent,
        // so the whole prompt counts as fresh rather than cached — the safe
        // direction for a budget.
        let u = usage_of(&json!({
            "prompt_tokens": 1_000,
            "completion_tokens": 42,
        }));
        assert_eq!(u.input, 1_000);
        assert_eq!(u.cache_read, 0);
        assert_eq!(u.output, 42);
    }

    #[test]
    fn system_rides_the_head_and_tools_follow_the_schema() {
        let req = Request {
            system: Some("Be terse.".into()),
            messages: vec![Message::user("hello")],
            tools: vec![crate::request::ToolDef {
                name: "read".into(),
                description: "Read a file".into(),
                input_schema: json!({ "type": "object" }),
            }],
            ..Default::default()
        };
        let b = body(req);
        assert_eq!(b["model"], "test-model");
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_tokens"], 32_000);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be terse.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(b["tools"][0]["type"], "function");
        assert_eq!(b["tools"][0]["function"]["name"], "read");
    }

    #[test]
    fn an_effort_model_says_enabled_and_off_says_disabled() {
        let on = body(Request {
            effort: Effort::Medium,
            ..Default::default()
        });
        assert_eq!(on["thinking"]["type"], "enabled");
        assert_eq!(on["thinking"]["reasoning_effort"], "medium");

        let off = body(Request::default());
        assert_eq!(off["thinking"]["type"], "disabled");
    }

    #[test]
    fn a_tool_result_becomes_a_tool_message_keyed_by_the_call() {
        let req = Request {
            messages: vec![Message::tool_results(vec![ToolResult {
                call: "call_1".into(),
                name: "read".into(),
                content: vec![ToolResultContent::Text(Text {
                    text: "the file".into(),
                })],
                is_error: false,
                useless: false,
            }])],
            ..Default::default()
        };
        let b = body(req);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "tool");
        assert_eq!(msgs[0]["tool_call_id"], "call_1");
        assert_eq!(msgs[0]["content"], "the file");
    }

    #[test]
    fn the_terminal_chunk_carries_the_usage_and_the_turn_folds() {
        let mut acc = Accumulator::new("test-model".into());
        let mut dec = Decoder::new();

        // Content and reasoning arrive on their own deltas.
        for d in [
            json!({ "choices": [{ "delta": { "role": "assistant", "content": "Hel" } }] }),
            json!({ "choices": [{ "delta": { "content": "lo" } }] }),
        ] {
            for e in dec.frame(&d) {
                acc.push(e);
            }
        }

        // A tool call streams across chunks: id and name on the first delta,
        // arguments on the next.
        for d in [
            json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "read", "arguments": "" },
            }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "function": { "arguments": "{\"path\":\"a" },
            }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "function": { "arguments": ".rs\"}" },
            }] } }] }),
        ] {
            for e in dec.frame(&d) {
                acc.push(e);
            }
        }

        // The terminal chunk states finish_reason and usage together.
        for e in dec.frame(&json!({
            "choices": [{ "delta": { "content": "" }, "finish_reason": "tool_calls" }],
            "usage": {
                "prompt_tokens": 17,
                "prompt_cache_hit_tokens": 0,
                "prompt_cache_miss_tokens": 17,
                "completion_tokens": 9,
            },
        })) {
            acc.push(e);
        }

        let done = acc.finish();
        assert_eq!(done.stop, StopReason::ToolUse);
        assert_eq!(done.usage.input, 17);
        assert_eq!(done.usage.output, 9);
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        assert!(content.iter().any(|c| matches!(
            c,
            AssistantContent::Text(t) if t.text == "Hello"
        )));
        assert!(content.iter().any(|c| matches!(
            c,
            AssistantContent::ToolCall(call) if call.name == "read" && call.args["path"] == "a.rs"
        )));
    }

    #[test]
    fn a_demoted_reasoning_block_ships_as_tagged_prose() {
        let req = Request {
            messages: vec![Message::Assistant {
                content: vec![AssistantContent::Reasoning(Reasoning {
                    id: None,
                    content: vec![ReasoningContent::Text {
                        text: "think hard".into(),
                        signature: None,
                    }],
                    by: Some("other-model".into()),
                })],
            }],
            ..Default::default()
        };
        let b = body(req);
        let msgs = b["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "assistant");
        assert!(msgs[0]["content"].as_str().unwrap().contains("think hard"));
    }

    #[test]
    fn reasoning_streams_first_and_keeps_its_position() {
        // The wire sends reasoning_content before content. Indexes are handed
        // out in arrival order, so the fold must put reasoning ahead of text —
        // the same shape the sibling transports persist.
        let mut acc = Accumulator::new("test-model".into());
        let mut dec = Decoder::new();
        for e in dec.frame(&json!({
            "choices": [{ "delta": { "reasoning_content": "think" } }]
        })) {
            acc.push(e);
        }
        for e in dec.frame(&json!({
            "choices": [{ "delta": { "content": "answer" } }]
        })) {
            acc.push(e);
        }
        let done = acc.finish();
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        assert!(matches!(&content[0], AssistantContent::Reasoning(_)));
        assert!(matches!(&content[1], AssistantContent::Text(t) if t.text == "answer"));
    }

    #[test]
    fn a_resent_tool_delta_does_not_reopen_the_block() {
        // Some hosts resend a call's id or name on a later delta. The block is
        // started once, so the resent fields must not overwrite the id and
        // name the first delta carried.
        let mut acc = Accumulator::new("test-model".into());
        let mut dec = Decoder::new();
        for d in [
            json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": { "name": "read", "arguments": "" },
            }] } }] }),
            json!({ "choices": [{ "delta": { "tool_calls": [{
                "index": 0, "function": { "arguments": "{}" },
            }] } }] }),
        ] {
            for e in dec.frame(&d) {
                acc.push(e);
            }
        }
        let done = acc.finish();
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        assert!(content.iter().any(|c| matches!(
            c,
            AssistantContent::ToolCall(call) if call.id == "call_1" && call.name == "read"
        )));
    }
}