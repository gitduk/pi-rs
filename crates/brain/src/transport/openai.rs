use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::Transport;
use crate::model::{Format, ModelSpec, ThinkingControl};
use crate::error::{BrainError, Result};
use crate::message::{
    AssistantContent, Image, Message, Reasoning, ReasoningContent, Replay, Text, ToolCall,
    ToolResult, ToolResultContent, UserContent, tagged,
};
use crate::request::{Request, ToolChoice};
use super::{Gaps, Shared};
use crate::stream::{BlockKind, InvalidToolArgs, StopReason, StreamEvent, Usage};

pub struct OpenAi {
    http: reqwest::Client,
    api_key: Option<String>,
    /// Session-lived, not per-request: what a host gets wrong it gets wrong
    /// every turn, and the reader needs to hear it once.
    gaps: Shared,
}

impl OpenAi {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            gaps: Shared::new("openai"),
        }
    }
}

fn check_format(spec: &ModelSpec) -> Result<()> {
    match spec.format {
        Format::OpenAi => Ok(()),
        _ => Err(BrainError::Config(format!(
            "{} is not an openai-format model",
            spec.model
        ))),
    }
}
// The one thing the wire cannot say. A `function_call_output` has no
// `is_error` — `status` is the item's own generation state — so a failure
// that is not marked in the text reads to the model as a result.
const FAILED: &str = "[tool error]";

fn encode_image(img: &Image) -> Value {
    let url = match img {
        Image::Url { url } => url.clone(),
        Image::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    };
    json!({ "type": "input_image", "image_url": url })
}

// `output` takes a string or a content-block array; the array is what carries
// an image back, so a result without one stays a plain string.
fn encode_tool_result(r: &ToolResult) -> Value {
    let has_image = r
        .content
        .iter()
        .any(|p| matches!(p, ToolResultContent::Image(_)));

    let output = if has_image {
        let mut blocks = Vec::new();
        if r.is_error {
            blocks.push(json!({ "type": "input_text", "text": FAILED }));
        }
        blocks.extend(r.content.iter().map(|p| match p {
            ToolResultContent::Text(t) => json!({ "type": "input_text", "text": t.text }),
            ToolResultContent::Json { value } => {
                json!({ "type": "input_text", "text": value.to_string() })
            }
            ToolResultContent::Image(img) => encode_image(img),
        }));
        Value::Array(blocks)
    } else if r.is_error {
        json!(format!("{FAILED}\n{}", r.flatten_text()))
    } else {
        json!(r.flatten_text())
    };

    json!({
        "type": "function_call_output",
        "call_id": r.call,
        "output": output,
    })
}

// Dress a stored reasoning block in this wire's shapes. Which way it leaves is
// `Reasoning::replay_for`'s call, shared with the estimate that sizes it.
fn encode_reasoning(r: &Reasoning, spec: &ModelSpec) -> Option<Value> {
    match r.replay_for(spec) {
        Replay::Encrypted { id, encrypted } => Some(json!({
            "type": "reasoning",
            "id": id,
            "summary": [],
            "encrypted_content": encrypted,
        })),
        Replay::Demoted => Some(assistant_text(&tagged(&r.text()))),
        // No OpenAI spec ever signs one: the transport is chosen by the same
        // format `replay_for` reads.
        Replay::Signed { .. } | Replay::Dropped => None,
    }
}

fn assistant_text(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text }],
    })
}

// Tool results are their own top-level items here, so a user turn holding both
// a result and prose leaves as two items — and in the order it held them.
fn encode_user(content: &[UserContent], out: &mut Vec<Value>) {
    let mut blocks: Vec<Value> = Vec::new();
    for b in content {
        match b {
            UserContent::Text(t) => blocks.push(json!({ "type": "input_text", "text": t.text })),
            UserContent::Image(img) => blocks.push(encode_image(img)),
            UserContent::ToolResult(r) => {
                flush_user(&mut blocks, out);
                out.push(encode_tool_result(r));
            }
        }
    }
    flush_user(&mut blocks, out);
}

fn flush_user(blocks: &mut Vec<Value>, out: &mut Vec<Value>) {
    if !blocks.is_empty() {
        out.push(json!({
            "type": "message",
            "role": "user",
            "content": std::mem::take(blocks),
        }));
    }
}

// An assistant turn splits into reasoning, message and function_call items,
// in the order the model produced them. Consecutive text stays one message.
fn encode_assistant(content: &[AssistantContent], spec: &ModelSpec, out: &mut Vec<Value>) {
    let mut text = String::new();
    for b in content {
        match b {
            AssistantContent::Text(t) => text.push_str(&t.text),
            AssistantContent::Reasoning(r) => {
                flush_assistant(&mut text, out);
                out.extend(encode_reasoning(r, spec));
            }
            AssistantContent::ToolCall(call) => {
                flush_assistant(&mut text, out);
                out.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.args.to_string(),
                }));
            }
        }
    }
    flush_assistant(&mut text, out);
}

fn flush_assistant(text: &mut String, out: &mut Vec<Value>) {
    if !text.is_empty() {
        out.push(assistant_text(&std::mem::take(text)));
    }
}

// The `input` item array. Nothing joins: `input` is flat and has no
// alternation rule, so one entry leaves as one item — the opposite of the
// Anthropic encoder, and the reason the join is the encoder's job.
fn encode(msgs: &[Message], spec: &ModelSpec, notes: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in msgs {
        match m {
            // The system prompt rides `instructions`, not the item array.
            Message::System { .. } => {}
            Message::User { content } => encode_user(content, &mut out),
            Message::Assistant { content, .. } => encode_assistant(content, spec, &mut out),
        }
    }
    // Its own trailing item: `input` is flat, so appending one changes nothing
    // before it and the cached prefix reaches just as far as it did.
    if !notes.is_empty() {
        out.push(json!({
            "type": "message",
            "role": "user",
            "content": notes
                .iter()
                .map(|n| json!({ "type": "input_text", "text": n }))
                .collect::<Vec<_>>(),
        }));
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
        "input": encode(&req.messages, spec, &req.notes),
        "max_output_tokens": max_output_tokens,
        // A transcript on someone else's disk is not pi's to leave behind, and
        // the reference page documents no default for this — so it is said.
        "store": false,
        // Without it a stateless reasoning item comes back with nothing to
        // replay, and nothing says so.
        "include": ["reasoning.encrypted_content"],
    });

    let instructions = req.system_text();
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }

    // Gated, as on the other wire: the OpenAI reasoning models are half of why
    // this field exists, and they refuse every value but the default.
    if let Some(t) = req.temperature
        && spec.accepts_temperature
    {
        body["temperature"] = json!(t);
    }

    if !req.tools.is_empty() {
        body["tools"] = json!(
            req.tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }))
                .collect::<Vec<_>>()
        );
        // Documented without a default, so it is sent rather than assumed.
        body["parallel_tool_calls"] = json!(true);
        body["tool_choice"] = match &req.tool_choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Named(name) => json!({ "type": "function", "name": name }),
        };
    }

    if spec.thinking == Some(ThinkingControl::Effort)
        && let Some(effort) = req.effort.as_openai()
    {
        body["reasoning"] = json!({ "effort": effort });
    }

    body
}


// `input_tokens` counts the cached prefix and the newly written one as well as
// the fresh tokens, so both come out of it. Subtracting only the read half —
// which is what the Chat Completions decoder did — bills the write twice.
//
// `cache_write_tokens` is in OpenAI's own usage type but not every
// implementation fills it: DeepSeek's Responses endpoint sends only
// `cached_tokens`. Absent, it reads as zero and the arithmetic still balances.
fn usage_of(u: &Value) -> Usage {
    let details = &u["input_tokens_details"];
    let cache_read = details["cached_tokens"].as_u64().unwrap_or(0);
    let cache_write = details["cache_write_tokens"].as_u64().unwrap_or(0);
    Usage {
        input: u["input_tokens"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read,
        cache_write,
    }
}

// Two levels, not one: a truncated turn is `status: "incomplete"` carrying the
// reason. Reading only the status would file it as a turn that ended.
fn stop_of(response: &Value) -> StopReason {
    if response["status"] != "incomplete" {
        return StopReason::EndTurn;
    }
    match response["incomplete_details"]["reason"].as_str() {
        Some("content_filter") => StopReason::Refusal,
        // The other documented reason is max_output_tokens, and an
        // unrecognised one is still a turn that did not finish.
        _ => StopReason::MaxTokens,
    }
}

fn text_of(parts: &Value, key: &str) -> String {
    parts
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p[key].as_str())
        .collect()
}

// The turn as the terminal frame states it. This is the authority: the deltas
// before it were for the screen.
fn read_output(
    output: &Value,
    origin: &str,
    gaps: &mut Gaps,
) -> (Vec<AssistantContent>, Vec<InvalidToolArgs>) {
    let mut content = Vec::new();
    let mut invalid = Vec::new();

    for item in output.as_array().into_iter().flatten() {
        match item["type"].as_str() {
            Some("reasoning") => {
                let mut parts = Vec::new();
                // The ciphertext is the only part that replays; the prose is
                // for the reader.
                if let Some(enc) = item["encrypted_content"].as_str() {
                    parts.push(ReasoningContent::Encrypted(enc.to_string()));
                }
                // Body and summary are two separate streams of the same
                // thinking. The body is the real one; the summary stands in
                // when an org is not shown the body.
                let mut text = text_of(&item["content"], "text");
                if text.is_empty() {
                    text = text_of(&item["summary"], "text");
                }
                if !text.is_empty() {
                    parts.push(ReasoningContent::Text {
                        text,
                        signature: None,
                    });
                }
                if !parts.is_empty() {
                    content.push(AssistantContent::Reasoning(Reasoning {
                        // Required when the item is sent back, so it is kept.
                        id: item["id"].as_str().map(str::to_string),
                        content: parts,
                        by: Some(origin.to_string()),
                    }));
                }
            }
            Some("message") => {
                let mut text = text_of(&item["content"], "text");
                // A refusal is a turn the model declined, not an empty one.
                text.push_str(&text_of(&item["content"], "refusal"));
                if !text.is_empty() {
                    content.push(AssistantContent::Text(Text { text }));
                }
            }
            Some("function_call") => {
                let id = item["call_id"].as_str().unwrap_or_default().to_string();
                let name = item["name"].as_str().unwrap_or_default().to_string();
                let raw = item["arguments"].as_str().unwrap_or_default();
                let args = match serde_json::from_str::<Value>(raw.trim()) {
                    Ok(v) => v,
                    Err(_) if raw.trim().is_empty() => json!({}),
                    Err(e) => {
                        invalid.push(InvalidToolArgs {
                            call: id.clone(),
                            name: name.clone(),
                            raw: raw.to_string(),
                            error: e.to_string(),
                        });
                        json!({})
                    }
                };
                content.push(AssistantContent::ToolCall(ToolCall { id, name, args }));
            }
            // An item kind this build does not know, sitting in the frame that
            // states the turn: whatever the model put there is now missing from
            // it, so this is a loss and not a curiosity.
            other => gaps.lost("response.output", other.unwrap_or("(untyped)")),
        }
    }
    (content, invalid)
}

// Responses events are typed and carry their own `output_index`, so the
// synthetic per-kind indices Chat Completions needed are gone.
struct Decoder {
    origin: String,
    gaps: Shared,
    /// Which of the two reasoning streams opened an item. They describe the
    /// same thinking, so letting both through prints it twice.
    reasoning_stream: BTreeMap<usize, bool>,
}

impl Decoder {
    /// Takes the whole identity rather than a model name. It stamps every
    /// reasoning block it decodes, and `replay_for` later compares that stamp
    /// against `spec.model.clone()` — so a decoder that names the provider itself
    /// can only ever name it wrong, and the ciphertext it stamped stops
    /// replaying without anything failing.
    fn new(origin: String, gaps: Shared) -> Self {
        Self {
            origin,
            gaps,
            reasoning_stream: BTreeMap::new(),
        }
    }

    fn frame(&mut self, data: &Value) -> Vec<StreamEvent> {
        let index = data["output_index"].as_u64().unwrap_or(0) as usize;
        let mut gaps = self.gaps.frame();
        let Some(event) = gaps.owed(data, "frame", "type") else {
            return Vec::new();
        };
        match event {
            "response.output_item.added" => {
                let item = &data["item"];
                let kind = match item["type"].as_str() {
                    Some("reasoning") => BlockKind::Reasoning,
                    Some("message") => BlockKind::Text,
                    Some("function_call") => BlockKind::ToolCall {
                        id: item["call_id"].as_str().map(str::to_string),
                        name: match gaps.owed(item, "function_call", "name") {
                            Some(name) => name.to_string(),
                            None => return Vec::new(),
                        },
                    },
                    other => {
                        gaps.lost(event, other.unwrap_or("(untyped)"));
                        return Vec::new();
                    }
                };
                vec![StreamEvent::BlockStart { index, kind }]
            }
            "response.output_text.delta" => match gaps.owed(data, event, "delta") {
                Some(delta) => vec![StreamEvent::TextDelta {
                    index,
                    delta: delta.to_string(),
                }],
                None => Vec::new(),
            },
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let body = data["type"] == "response.reasoning_text.delta";
                match self.reasoning_stream.entry(index) {
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(body);
                    }
                    std::collections::btree_map::Entry::Occupied(held) if *held.get() != body => {
                        return Vec::new();
                    }
                    _ => {}
                }
                match gaps.owed(data, event, "delta") {
                    Some(delta) => vec![StreamEvent::ReasoningDelta {
                        index,
                        delta: delta.to_string(),
                    }],
                    None => Vec::new(),
                }
            }
            "response.function_call_arguments.delta" => {
                match gaps.owed(data, event, "delta") {
                    Some(delta) => vec![StreamEvent::ToolArgsDelta {
                        index,
                        delta: delta.to_string(),
                    }],
                    None => Vec::new(),
                }
            }
            "response.output_item.done" => vec![StreamEvent::BlockEnd { index }],
            "response.completed" | "response.incomplete" => {
                let response = &data["response"];
                let mut events = Vec::new();
                // `output` is required on the Response this frame carries, so a
                // host omitting it is out of spec — but saying nothing is still
                // not saying the turn was empty. Read as a statement, an absent
                // `output` threw away a tool call the deltas had already
                // delivered and ended the run after the thinking, looking
                // ordinary. The deltas stand instead, and the host is named:
                // the quirk belongs in whatever gateway is doing this, and it
                // cannot be fixed there while it is invisible here.
                if response["output"].is_null() {
                    gaps.lost(event, "output");
                } else {
                    let (content, invalid) =
                        read_output(&response["output"], &self.origin, &mut gaps);
                    events.push(StreamEvent::Complete { content, invalid });
                }
                events.push(StreamEvent::Done {
                    stop: stop_of(response),
                    usage: usage_of(&response["usage"]),
                });
                events
            }
            // Bookkeeping, or an event added after this was written. Costs the
            // turn nothing: these frames only paint, and what the model said
            // arrives either in the terminal frame or in the deltas.
            other => {
                gaps.ignored("frame", other);
                Vec::new()
            }
        }
    }
}

#[async_trait]
impl Transport for OpenAi {
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
        let url = format!("{}/responses", spec.base_url.trim_end_matches('/'));
        let mut call = self.http.post(&url);
        if let Some(key) = &self.api_key {
            call = call.bearer_auth(key);
        }
        let call = call.json(&body);
        let resp = super::exchange("openai", url, spec, req, &body, call).await?;

        let mut dec = Decoder::new(spec.model.clone(), self.gaps.clone());

        let stream = resp
            .bytes_stream()
            .eventsource()
            .flat_map(move |frame| {
                let events: Vec<Result<StreamEvent>> = match frame {
                    Err(e) => vec![Err(BrainError::Stream(e.to_string()))],
                    Ok(f) => match serde_json::from_str::<Value>(&f.data) {
                        Err(e) => vec![Err(BrainError::Stream(e.to_string()))],
                        Ok(data) if data.get("error").is_some_and(|e| !e.is_null()) => {
                            tracing::warn!(
                                target: "pi::wire", format = "openai",
                                detail = %data["error"], "error frame"
                            );
                            vec![Err(BrainError::Stream(data["error"].to_string()))]
                        }
                        // A run that failed server-side reports it here, not
                        // as a status: without this the turn ends looking
                        // ordinary and empty.
                        Ok(data) if data["type"] == "response.failed" => {
                            let detail = &data["response"]["error"];
                            tracing::warn!(
                                target: "pi::wire", format = "openai",
                                detail = %detail, "failed response"
                            );
                            vec![Err(BrainError::Stream(detail.to_string()))]
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
    use crate::message::{Text, ToolResult, ToolResultContent};
    use crate::model::ReplayThinking;
    use crate::request::Effort;

    fn spec() -> ModelSpec {
        ModelSpec {
            base_url: "https://api.openai.com/v1".into(),
            format: Format::OpenAi,
            context_window: 400_000,
            thinking: Some(ThinkingControl::Effort),
            ..ModelSpec::test()
        }
    }

    fn call(id: &str, name: &str, args: Value) -> AssistantContent {
        AssistantContent::ToolCall(crate::message::ToolCall {
            id: id.into(),
            name: name.into(),
            args,
        })
    }

    fn reasoning(parts: Vec<ReasoningContent>, from: Option<&str>) -> AssistantContent {
        AssistantContent::Reasoning(Reasoning {
            id: Some("rs_1".into()),
            content: parts,
            // Provider from the spec under test, never a literal: whether a
            // block is native is decided by comparing against
            // `spec().model`, so a hand-written provider here would make
            // these pass on a coincidence.
            by: from.map(|m| m.into()),
        })
    }

    fn body(req: Request) -> Value {
        build_body(&spec(), &req)
    }

    /// Scaffolding, not an assertion: dumps a request body to `$PI_SNAP` for
    /// eyeballing against a live endpoint, and is a no-op without it.
    #[test]
    fn tmp_dump_live_body() {
        let Ok(dir) = std::env::var("PI_SNAP") else { return };
        let model = std::env::var("PI_MODEL").unwrap();
        let mut s = spec();
        s.model = model;
        s.max_output_tokens = 128;
        let req = Request {
            system: Some("You are terse.".into()),
            messages: vec![Message::user(
                "Read the file a.rs using the read tool, then say done.",
            )],
            tools: vec![crate::request::ToolDef {
                name: "read".into(),
                description: "Read a file from disk".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"],
                }),
            }],
            effort: Effort::Off,
            ..Default::default()
        };
        std::fs::write(
            format!("{dir}/live_body.json"),
            serde_json::to_vec_pretty(&build_body(&s, &req)).unwrap(),
        )
        .unwrap();
    }

    /// The whole request, written out by hand. There is no old encoder to
    /// compare against — Chat Completions is gone — so this test is the only
    /// statement of what pi puts on the wire.
    #[test]
    fn a_turn_of_traffic_encodes_to_exactly_this() {
        let req = Request {
            system: Some("you are pi".into()),
            messages: vec![
                Message::user("go"),
                Message::Assistant {
                    content: vec![
                        reasoning(
                            vec![ReasoningContent::Encrypted("ct".into())],
                            Some("test-model"),
                        ),
                        AssistantContent::Text(Text {
                            text: "reading".into(),
                        }),
                        call("call_1", "read", json!({ "path": "a.rs" })),
                    ],
                },
                Message::tool_results(vec![ToolResult::text("call_1", "read", "fn main() {}")]),
                Message::user("now the other one"),
            ],
            tools: vec![crate::request::ToolDef {
                name: "read".into(),
                description: "read a file".into(),
                input_schema: json!({ "type": "object" }),
            }],
            effort: Effort::Medium,
            ..Default::default()
        };

        assert_eq!(
            body(req),
            json!({
                "model": "test-model",
                "stream": true,
                "max_output_tokens": 32_000,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "instructions": "you are pi",
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "reasoning": { "effort": "medium" },
                "tools": [{
                    "type": "function",
                    "name": "read",
                    "description": "read a file",
                    "parameters": { "type": "object" },
                }],
                "input": [
                    { "type": "message", "role": "user",
                      "content": [{ "type": "input_text", "text": "go" }] },
                    { "type": "reasoning", "id": "rs_1", "summary": [],
                      "encrypted_content": "ct" },
                    { "type": "message", "role": "assistant",
                      "content": [{ "type": "output_text", "text": "reading" }] },
                    { "type": "function_call", "call_id": "call_1", "name": "read",
                      "arguments": "{\"path\":\"a.rs\"}" },
                    { "type": "function_call_output", "call_id": "call_1",
                      "output": "fn main() {}" },
                    { "type": "message", "role": "user",
                      "content": [{ "type": "input_text", "text": "now the other one" }] },
                ],
            })
        );
    }

    /// The wire has no `is_error`, so an unmarked failure reads as a result.
    #[test]
    fn a_failed_tool_result_says_so_in_the_only_place_the_wire_leaves() {
        let req = Request {
            messages: vec![Message::tool_results(vec![ToolResult::error(
                "c1",
                "read",
                "no such file",
            )])],
            ..Default::default()
        };
        assert_eq!(
            body(req)["input"][0],
            json!({
                "type": "function_call_output",
                "call_id": "c1",
                "output": "[tool error]\nno such file",
            })
        );
    }

    /// The other half of the same defect: Chat Completions flattened an image
    /// to the literal `[image]`.
    #[test]
    fn an_image_in_a_tool_result_travels_as_an_image() {
        let req = Request {
            messages: vec![Message::tool_results(vec![ToolResult {
                call: "c1".into(),
                name: "screenshot".into(),
                content: vec![
                    ToolResultContent::Text(Text {
                        text: "captured".into(),
                    }),
                    ToolResultContent::Image(Image::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    }),
                ],
                is_error: false,
                useless: false,
            }])],
            ..Default::default()
        };
        assert_eq!(
            body(req)["input"][0]["output"],
            json!([
                { "type": "input_text", "text": "captured" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
            ])
        );
    }

    /// Both defects at once, which is the shape a failing screenshot takes.
    #[test]
    fn a_failed_result_carrying_an_image_keeps_both_facts() {
        let req = Request {
            messages: vec![Message::tool_results(vec![ToolResult {
                call: "c1".into(),
                name: "screenshot".into(),
                content: vec![ToolResultContent::Image(Image::Url {
                    url: "http://x/i.png".into(),
                })],
                is_error: true,
                useless: false,
            }])],
            ..Default::default()
        };
        assert_eq!(
            body(req)["input"][0]["output"],
            json!([
                { "type": "input_text", "text": "[tool error]" },
                { "type": "input_image", "image_url": "http://x/i.png" },
            ])
        );
    }

    #[test]
    fn only_this_models_own_ciphertext_replays_as_reasoning() {
        let prose = ReasoningContent::Text {
            text: "step".into(),
            signature: None,
        };
        let cases = [
            // Another model's ciphertext cannot be decrypted by this one.
            ("another model", Some("another-model"), Some("rs_1"), true),
            // Ours, but the run never asked for the ciphertext.
            ("no ciphertext", Some("test-model"), Some("rs_1"), false),
            // Ours and encrypted, but with no item id to name it by — the
            // endpoint requires one, so this must not ship as a reasoning item.
            ("no item id", Some("test-model"), None, true),
            // Locally synthesized: nothing says which model could read it.
            ("no origin", None, Some("rs_1"), true),
        ];
        for (case, from, id, encrypted) in cases {
            let content = if encrypted {
                vec![ReasoningContent::Encrypted("ct".into()), prose.clone()]
            } else {
                vec![prose.clone()]
            };
            let req = Request {
                messages: vec![Message::Assistant {
                    content: vec![AssistantContent::Reasoning(Reasoning {
                        id: id.map(str::to_string),
                        content,
                        by: from.map(|m| m.into()),
                    })],
                }],
                ..Default::default()
            };
            assert_ne!(body(req)["input"][0]["type"], "reasoning", "{case}");
        }
    }

    /// A demoted block ships as prose rather than vanishing; `Drop` is what
    /// makes it vanish, and then the turn leaves nothing behind at all.
    #[test]
    fn a_reasoning_block_this_model_cannot_replay_is_demoted_not_dropped() {
        let msgs = vec![Message::Assistant {
            content: vec![reasoning(
                vec![ReasoningContent::Text {
                    text: "step".into(),
                    signature: None,
                }],
                Some("another-model"),
            )],
        }];
        assert_eq!(
            body(Request { messages: msgs.clone(), ..Default::default() })["input"][0],
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "<think>\nstep\n</think>" }],
            })
        );

        let mut dropping = spec();
        dropping.replay_thinking = ReplayThinking::Off;
        let dropped = build_body(
            &dropping,
            &Request { messages: msgs, ..Default::default() },
        );
        assert_eq!(dropped["input"].as_array().unwrap().len(), 0);
    }

    /// `input` is flat, so the turn's own ordering is the only thing carrying
    /// which reasoning belongs to which call.
    #[test]
    fn an_assistant_turn_splits_into_items_in_the_order_it_produced_them() {
        let req = Request {
            messages: vec![Message::Assistant {
                content: vec![
                    AssistantContent::Text(Text { text: "first ".into() }),
                    AssistantContent::Text(Text { text: "second".into() }),
                    call("c1", "read", json!({})),
                    AssistantContent::Text(Text { text: "after".into() }),
                    call("c2", "grep", json!({})),
                ],
            }],
            ..Default::default()
        };
        let kinds: Vec<String> = body(req)["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            kinds,
            ["message", "function_call", "message", "function_call"],
            "consecutive text joins; everything else keeps its place"
        );
    }

    /// A user turn holding both leaves as two items, in order — the shape a
    /// tool result followed by a prompt takes.
    #[test]
    fn a_result_and_the_prose_after_it_are_two_items() {
        let req = Request {
            messages: vec![Message::User {
                content: vec![
                    UserContent::ToolResult(ToolResult::text("c1", "read", "body")),
                    UserContent::Text(Text {
                        text: "now what".into(),
                    }),
                ],
            }],
            ..Default::default()
        };
        let items = body(req)["input"].clone();
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[1]["role"], "user");
    }

    #[test]
    fn a_system_message_rides_instructions_and_leaves_the_item_array() {
        let req = Request {
            messages: vec![
                Message::System {
                    content: "from the array".into(),
                },
                Message::user("hi"),
            ],
            ..Default::default()
        };
        let out = body(req);
        assert_eq!(out["instructions"], "from the array");
        assert_eq!(out["input"].as_array().unwrap().len(), 1);
    }

    // ─── decoding ────────────────────────────────────────────────────────
    //
    // The fixture below is hand-written from the SDK's event types, not
    // recorded off a live endpoint. That is weaker evidence than a replay and
    // is worth remembering: it proves pi reads what the types say, not what
    // the server sends.

    fn drive(frames: &[Value]) -> crate::stream::Completion {
        let mut dec = Decoder::new(spec().model, Shared::new("openai"));
        let mut acc = crate::stream::Accumulator::new(spec().model);
        for f in frames {
            for ev in dec.frame(f) {
                acc.push(ev);
            }
        }
        acc.finish()
    }

    /// Replay a stream recorded off a live endpoint. `data:` lines only — the
    /// same thing `eventsource` hands the transport.
    fn replay(sse: &str) -> crate::stream::Completion {
        let frames: Vec<Value> = sse
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|l| serde_json::from_str(l.trim()).expect("a recorded frame is JSON"))
            .collect();
        drive(&frames)
    }

    /// Recorded against DeepSeek's Responses endpoint, 2026-08-29. Hand-written
    /// frames prove pi reads what the SDK types say; these prove it reads what
    /// a server actually sends, which is the stronger claim and the one the
    /// landing plan asked for.
    #[test]
    fn a_recorded_tool_call_replays_into_the_call_the_model_made() {
        let done = replay(include_str!("../../tests/fixtures/responses_tools.sse"));
        assert!(done.invalid.is_empty());
        assert_eq!(done.stop, StopReason::ToolUse);

        let Message::Assistant { content } = done.message else {
            panic!("assistant")
        };
        assert_eq!(content.len(), 1);
        let AssistantContent::ToolCall(c) = &content[0] else {
            panic!("a function_call item is a tool call")
        };
        // The wire carries two ids on this item; the one that travels back on
        // `function_call_output` is `call_id`, never the item's own `id`.
        assert_eq!(c.id, "call_00_RuRN73rvb71G7bDaBQlV2823");
        assert_eq!(c.name, "read");
        assert_eq!(c.args["path"], "a.rs");

        assert_eq!(done.usage.cache_read, 0);
        assert_eq!(done.usage.cache_write, 0);
    }

    #[test]
    fn a_recorded_reasoning_turn_keeps_the_ciphertext_that_replays_it() {
        let done = replay(include_str!("../../tests/fixtures/responses_reason.sse"));
        assert_eq!(done.stop, StopReason::EndTurn);

        let Message::Assistant { content } = done.message else {
            panic!("assistant")
        };
        let AssistantContent::Reasoning(r) = &content[0] else {
            panic!("the reasoning item comes first")
        };
        // Required when the item goes back, and only present because the
        // request asked for it with `include`.
        assert!(r.id.is_some());
        assert!(
            matches!(&r.content[0], ReasoningContent::Encrypted(s) if !s.is_empty()),
            "the ciphertext is the whole of what replays"
        );
        // This turn carried a body and no summary, so the body is what is kept.
        assert!(matches!(
            &r.content[1],
            ReasoningContent::Text { text, .. } if !text.is_empty()
        ));
        assert!(matches!(&content[1], AssistantContent::Text(t) if !t.text.is_empty()));

        // 93 input, none cached: the three parts add back up.
        let u = done.usage;
        assert_eq!(u.input + u.cache_read + u.cache_write, 93);
        assert_eq!(u.output, 45);
    }

    /// The reasoning models are half of why `accepts_temperature` exists, and
    /// this wire is where they are. It gated on the other one only.
    #[test]
    fn sampling_params_dropped_when_the_model_rejects_them() {
        let req = Request {
            temperature: Some(0.7),
            ..Default::default()
        };
        let mut rejects = spec();
        rejects.accepts_temperature = false;
        assert!(build_body(&rejects, &req)["temperature"].is_null());
        // And still sent to a model that takes one.
        assert_eq!(build_body(&spec(), &req)["temperature"], 0.7);
    }

    /// One `function_call` on the wire must be one call in the turn. Recorded
    /// off the gateway, whose terminal frame states no output, so the whole
    /// turn is rebuilt from the deltas.
    #[test]
    fn one_streamed_call_is_one_call() {
        let done = replay(include_str!("../../tests/fixtures/responses_one_call.sse"));
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        let calls: Vec<_> = content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{content:?}");
        assert_eq!(calls[0].args["path"], "a.txt");
    }

    /// Recorded against a local gateway translating for DeepSeek, 2026-08-30.
    /// Its terminal frame carries status and usage and no `output` at all, so
    /// the turn is only ever stated by the deltas — the shape that ended every
    /// tool-calling run after the thinking and before the call.
    #[test]
    fn a_terminal_frame_that_states_no_output_leaves_the_deltas_standing() {
        let done = replay(include_str!("../../tests/fixtures/responses_no_output.sse"));
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        let call = content
            .iter()
            .find_map(|c| match c {
                AssistantContent::ToolCall(c) => Some(c),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the call the deltas delivered: {content:?}"));
        assert_eq!(call.name, "read");
        assert!(call.args["path"].is_string(), "{}", call.args);
        // The frame said `completed`; the calls are what make it a tool turn.
        assert_eq!(done.stop, StopReason::ToolUse);
    }

    /// The seam neither suite either side of it crossed. One drove a stream and
    /// read what came out; the other replayed reasoning it had built by hand.
    /// Between them sat the decoder's idea of who produced the block, and while
    /// it named the provider itself, every ciphertext it stamped quietly
    /// demoted to prose on the way back out — with both suites still green.
    #[test]
    fn reasoning_decoded_from_a_stream_replays_as_reasoning() {
        let done = replay(include_str!("../../tests/fixtures/responses_reason.sse"));
        let req = Request {
            messages: vec![done.message],
            ..Default::default()
        };
        let item = &body(req)["input"][0];
        assert_eq!(item["type"], "reasoning", "it demoted instead of replaying");
        assert!(
            item["encrypted_content"].as_str().is_some_and(|s| !s.is_empty()),
            "the ciphertext is the whole of what replays: {item}"
        );
    }

    fn a_turn() -> Vec<Value> {
        vec![
            json!({ "type": "response.created" }),
            json!({ "type": "response.output_item.added", "output_index": 0,
                    "item": { "type": "reasoning", "id": "rs_1" } }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "delta": "weigh" }),
            json!({ "type": "response.output_item.done", "output_index": 0 }),
            json!({ "type": "response.output_item.added", "output_index": 1,
                    "item": { "type": "message", "role": "assistant" } }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "read" }),
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "ing" }),
            json!({ "type": "response.output_item.done", "output_index": 1 }),
            json!({ "type": "response.output_item.added", "output_index": 2,
                    "item": { "type": "function_call", "call_id": "call_1", "name": "read" } }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2,
                    "delta": "{\"pa" }),
            json!({ "type": "response.function_call_arguments.delta", "output_index": 2,
                    "delta": "th\":\"a.rs\"}" }),
            json!({ "type": "response.output_item.done", "output_index": 2 }),
            json!({ "type": "response.completed", "response": {
                "status": "completed",
                "usage": { "input_tokens": 1_000, "output_tokens": 42,
                           "input_tokens_details": { "cached_tokens": 600,
                                                     "cache_write_tokens": 300 } },
                "output": [
                    { "type": "reasoning", "id": "rs_1", "encrypted_content": "ct",
                      "summary": [{ "type": "summary_text", "text": "weighed it" }] },
                    { "type": "message", "role": "assistant",
                      "content": [{ "type": "output_text", "text": "reading" }] },
                    { "type": "function_call", "call_id": "call_1", "name": "read",
                      "arguments": "{\"path\":\"a.rs\"}" },
                ],
            }}),
        ]
    }

    #[test]
    fn a_whole_turn_replays_into_the_message_the_terminal_frame_states() {
        let done = drive(&a_turn());
        assert!(done.invalid.is_empty());
        assert_eq!(done.stop, StopReason::ToolUse, "a pending call is not an end");

        let Message::Assistant { content, .. } = done.message else {
            panic!("assistant")
        };
        assert_eq!(content.len(), 3);
        // The ciphertext is what replays; the prose rides along for the reader.
        let AssistantContent::Reasoning(r) = &content[0] else {
            panic!("reasoning first")
        };
        assert_eq!(r.id.as_deref(), Some("rs_1"));
        assert_eq!(r.content[0], ReasoningContent::Encrypted("ct".into()));
        assert!(matches!(&content[1], AssistantContent::Text(t) if t.text == "reading"));
        let AssistantContent::ToolCall(c) = &content[2] else {
            panic!("call last")
        };
        assert_eq!((c.id.as_str(), c.name.as_str()), ("call_1", "read"));
        assert_eq!(c.args["path"], "a.rs");
    }

    /// The whole reason the accumulator was split. Deltas are for the screen;
    /// what lands in the transcript is what the terminal frame states.
    #[test]
    fn the_terminal_frame_outranks_the_deltas_that_preceded_it() {
        let mut frames = a_turn();
        // A truncated ciphertext is exactly what reassembling deltas produces,
        // and it fails silently next turn rather than erroring here.
        frames.insert(
            3,
            json!({ "type": "response.output_text.delta", "output_index": 1, "delta": "half" }),
        );
        let done = drive(&frames);
        let Message::Assistant { content, .. } = done.message else {
            panic!("assistant")
        };
        assert!(
            matches!(&content[1], AssistantContent::Text(t) if t.text == "reading"),
            "the deltas said `halfreading`; the frame says `reading`"
        );
    }

    /// `input_tokens` counts both halves of the cache. Taking out only the read
    /// half — which is what the Chat Completions decoder did — bills the write
    /// twice, once as fresh input and once as a write.
    #[test]
    fn the_three_token_counts_add_back_up_to_what_was_billed() {
        let done = drive(&a_turn());
        let u = done.usage;
        assert_eq!(u.cache_read, 600);
        assert_eq!(u.cache_write, 300);
        assert_eq!(u.input, 100);
        assert_eq!(
            u.input + u.cache_read + u.cache_write,
            1_000,
            "the three parts must add back to input_tokens"
        );
        assert_eq!(u.output, 42);
    }

    /// Two levels, not one: reading `status` alone files a truncated turn as
    /// one that ended normally, and the loop never retries it.
    #[test]
    fn a_truncated_turn_is_not_a_turn_that_ended() {
        let cases = [
            (json!({ "status": "completed" }), StopReason::EndTurn),
            (
                json!({ "status": "incomplete",
                        "incomplete_details": { "reason": "max_output_tokens" } }),
                StopReason::MaxTokens,
            ),
            (
                json!({ "status": "incomplete",
                        "incomplete_details": { "reason": "content_filter" } }),
                StopReason::Refusal,
            ),
            // An unrecognised reason is still a turn that did not finish.
            (
                json!({ "status": "incomplete", "incomplete_details": { "reason": "new" } }),
                StopReason::MaxTokens,
            ),
        ];
        for (response, want) in cases {
            let mut response = response;
            response["output"] = json!([]);
            let frames = vec![json!({ "type": "response.completed", "response": response })];
            assert_eq!(drive(&frames).stop, want, "{response}");
        }
    }

    /// Body and summary are two streams of the same thinking. Letting both
    /// through prints the reasoning twice.
    #[test]
    fn only_one_of_the_two_reasoning_streams_reaches_the_screen() {
        let frames = [
            json!({ "type": "response.output_item.added", "output_index": 0,
                    "item": { "type": "reasoning", "id": "rs_1" } }),
            json!({ "type": "response.reasoning_summary_text.delta", "output_index": 0,
                    "delta": "summary" }),
            json!({ "type": "response.reasoning_text.delta", "output_index": 0,
                    "delta": "body" }),
        ];
        let mut dec = Decoder::new(spec().model, Shared::new("openai"));
        let seen: String = frames
            .iter()
            .flat_map(|f| dec.frame(f))
            .filter_map(|e| match e {
                StreamEvent::ReasoningDelta { delta, .. } => Some(delta),
                _ => None,
            })
            .collect();
        assert_eq!(seen, "summary", "the second stream must not join the first");
    }

    /// The body is the real thinking; the summary stands in only when an
    /// organisation is not shown the body.
    #[test]
    fn the_recorded_reasoning_prefers_the_body_over_the_summary() {
        let with_both = json!({ "type": "response.completed", "response": {
            "status": "completed", "output": [{
                "type": "reasoning", "id": "rs_1",
                "summary": [{ "type": "summary_text", "text": "the gist" }],
                "content": [{ "type": "reasoning_text", "text": "the working" }],
            }],
        }});
        let Message::Assistant { content, .. } = drive(&[with_both]).message else {
            panic!("assistant")
        };
        let AssistantContent::Reasoning(r) = &content[0] else {
            panic!("reasoning")
        };
        assert_eq!(
            r.content[0],
            ReasoningContent::Text { text: "the working".into(), signature: None }
        );
    }

    #[test]
    fn arguments_that_are_not_json_still_leave_a_balanced_call() {
        let frames = vec![json!({ "type": "response.completed", "response": {
            "status": "completed", "output": [{
                "type": "function_call", "call_id": "c1", "name": "read",
                "arguments": "{\"path\": ",
            }],
        }})];
        let done = drive(&frames);
        assert_eq!(done.invalid.len(), 1);
        assert_eq!(done.invalid[0].call, "c1");
        let Message::Assistant { content, .. } = done.message else {
            panic!("assistant")
        };
        let AssistantContent::ToolCall(c) = &content[0] else {
            panic!("the call still enters the message")
        };
        assert_eq!(c.args, json!({}));
    }

    /// `input` is flat, so a trailing item changes nothing before it and the
    /// cached prefix reaches exactly as far as it did last turn.
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
    fn a_note_becomes_its_own_trailing_item() {
        let req = Request {
            messages: vec![Message::user("go"), Message::assistant_text("done")],
            notes: vec!["[true only this turn]".into()],
            ..Default::default()
        };
        let input = body(req);
        let items = input["input"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[2]["role"], "user");
        assert_eq!(items[2]["content"][0]["text"], "[true only this turn]");
    }

    /// Caching is on by default here, with one implicit breakpoint that moves
    /// with the conversation. Naming `prompt_cache_options.mode = "explicit"`
    /// and then setting no breakpoint turns caching off entirely and says
    /// nothing — so the field is never written at all.
    #[test]
    fn nothing_is_said_about_caching_because_saying_it_can_only_turn_it_off() {
        let out = body(Request {
            messages: vec![Message::user("go")],
            ..Default::default()
        });
        assert!(out.get("prompt_cache_options").is_none());
        assert!(out.get("prompt_cache_key").is_none());
    }

    /// Neither field has a documented default, and the wrong value for either
    /// fails silently: a stored transcript, or a reasoning item with nothing
    /// to replay.
    #[test]
    fn a_stateless_run_says_so_and_asks_for_what_it_will_need_next_turn() {
        let out = body(Request::default());
        assert_eq!(out["store"], json!(false));
        assert_eq!(out["include"], json!(["reasoning.encrypted_content"]));
        // No tools, so neither field belongs on the request.
        assert!(out.get("tool_choice").is_none());
        assert!(out.get("parallel_tool_calls").is_none());
    }
}
