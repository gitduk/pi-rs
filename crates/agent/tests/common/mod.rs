//! Fixtures shared by the integration tests.
//!
//! `ModelSpec` has no `Default` on purpose — an id, a model and a base url
//! have no sensible empty value in production — so tests build the whole
//! struct here rather than drifting apart across files.

use llm::model::{CacheControl, Format, ModelSpec, Pricing};
use llm::request::Request;
use llm::stream::StreamEvent;
use llm::transport::Transport;
use std::sync::Arc;

/// Put the compactor that ships on an agent: `writer` for a summarizer with a
/// model of its own, `None` for the model doing the work.
///
/// `Agent::new` starts on the identity compactor, which never shrinks
/// anything, so a test about compaction must say which one it means.
pub fn compacting(agent: &mut agent::Agent, writer: Option<(Arc<dyn Transport>, ModelSpec)>) {
    agent.compactor = Arc::new(agent::Summarizing::new(
        writer,
        agent::Retry::default().idle,
    ));
}

pub fn spec() -> ModelSpec {
    ModelSpec {
        model: "test-wire-id".into(),
        base_url: "http://localhost".into(),
        format: Format::Anthropic {
            cache_control: CacheControl::Off,
        },
        context_window: 200_000,
        max_output_tokens: 8_000,
        vision: true,
        accepts_effort: true,
        accepts_temperature: true,
        can_force_tool: true,
        pricing: Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            ..Default::default()
        },
    }
}

/// Our bound on the whole request: system prompt, tools and messages.
#[allow(dead_code)] // not every test binary that includes this module bounds
pub fn bound(req: &Request, spec: &ModelSpec) -> usize {
    req.system_text().map_or(0, llm::estimate::text)
        + llm::estimate::tool_defs(&req.tools)
        + llm::estimate::tokens(&req.messages, spec)
}

/// A reply as a provider whose tokenizer is our bound would send it: the
/// request's size stated on `Done`, as a real wire reports what it took in.
#[allow(dead_code)] // not every test binary that includes this module counts
pub fn counted(mut events: Vec<StreamEvent>, spec: &ModelSpec, req: &Request) -> Vec<StreamEvent> {
    let prompt = bound(req, spec);
    for e in &mut events {
        if let StreamEvent::Done { usage, .. } = e {
            usage.input = prompt as u64;
        }
    }
    events
}
