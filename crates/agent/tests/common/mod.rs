//! Fixtures shared by the integration tests.
//!
//! `ModelSpec` has no `Default` on purpose — an id, a model and a base url
//! have no sensible empty value in production — so every test file that needs
//! one has to build the whole struct. Here once, rather than drifting apart in
//! each of them.

use brain::model::{
    CacheControl, Format, ModelSpec, Pricing, ReplayThinking, ThinkingControl,
};

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
