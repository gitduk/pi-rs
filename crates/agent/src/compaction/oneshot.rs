//! One turn, no tools, no reasoning.
//!
//! Shared by both compaction judgements and by memory, so `effort` and
//! `tool_choice` cannot drift between them.

use futures::StreamExt;
use llm::message::Message;
use llm::model::ModelSpec;
use llm::request::{Effort, Request, ToolChoice};
use llm::stream::{Accumulator, Usage};
use llm::transport::Transport;

/// Ask `transport` one question under `system` and read back the text.
pub async fn ask(
    transport: &dyn Transport,
    spec: &ModelSpec,
    system: &str,
    body: String,
    max_tokens: u32,
    // The same leash `attempt` keeps: a provider that stops sending holds a
    // compaction open exactly as it would hold a turn.
    idle: std::time::Duration,
) -> llm::Result<(String, Usage)> {
    let req = Request {
        system: Some(system.to_string()),
        messages: vec![Message::user(body)],
        tools: Vec::new(),
        max_output_tokens: Some(max_tokens.min(spec.max_output_tokens)),
        temperature: None,
        // Reasoning about an answer this size costs more than the answer.
        effort: Effort::Off,
        tool_choice: ToolChoice::None,
    };

    let mut acc = Accumulator::new(spec.model.clone());
    let mut stream = crate::leashed(idle, transport.stream(spec, &req)).await??;
    loop {
        let Some(ev) = crate::leashed(idle, stream.next()).await? else {
            break;
        };
        acc.push(ev?);
    }
    let done = acc.finish();
    Ok((done.message.text(), done.usage))
}
