//! Fixtures shared by the integration tests.
//!
//! `ModelSpec` has no `Default` on purpose — an id, a model and a base url
//! have no sensible empty value in production — so tests build the whole
//! struct here rather than drifting apart across files.

use llm::model::{CacheControl, Format, ModelSpec, Pricing, ReplayThinking, ThinkingControl};
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

/// `replay_thinking` is `Tagged` deliberately: on a spec that drops prior
/// reasoning the estimate counts it as nothing, and a fixture built out of
/// reasoning blocks would weigh zero without saying why.
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
        thinking: Some(ThinkingControl::Budget),
        accepts_temperature: true,
        can_force_tool: true,
        replay_thinking: ReplayThinking::Tagged,
        pricing: Pricing {
            input_per_mtok: 1.0,
            output_per_mtok: 2.0,
            ..Default::default()
        },
    }
}
