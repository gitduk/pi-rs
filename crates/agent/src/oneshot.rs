//! One turn, no tools, no reasoning.
//!
//! Both judgements a compaction makes about the span it is dropping have this
//! shape, and it is inherent rather than incidental: a small question with a
//! small answer, asked while something is on its way out. Written once so the
//! two cannot drift — an `effort` or a `tool_choice` changed in one of them
//! and not the other would be a difference nobody chose.

use brain::message::Message;
use brain::model::ModelSpec;
use brain::request::{Effort, Request, ToolChoice};
use brain::stream::{Accumulator, Usage};
use brain::transport::Transport;
use futures::StreamExt;

pub(crate) async fn ask(
    transport: &dyn Transport,
    spec: &ModelSpec,
    system: &str,
    body: String,
    max_tokens: u32,
) -> brain::Result<(String, Usage)> {
    let req = Request {
        system: Some(system.to_string()),
        messages: vec![Message::user(body)],
        notes: Vec::new(),
        tools: Vec::new(),
        max_output_tokens: Some(max_tokens.min(spec.max_output_tokens)),
        temperature: None,
        // Reasoning about an answer this size costs more than the answer.
        effort: Effort::Off,
        tool_choice: ToolChoice::None,
    };

    let mut acc = Accumulator::new(spec.model.clone());
    let mut stream = transport.stream(spec, &req).await?;
    while let Some(ev) = stream.next().await {
        acc.push(ev?);
    }
    let done = acc.finish();
    Ok((done.message.text(), done.usage))
}
