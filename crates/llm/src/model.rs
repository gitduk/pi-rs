//! Everything a model needs before it can be talked to: where it lives, which
//! shape it speaks, how it takes a thinking instruction, and what it costs.
//!
//! No built-in model list: the user writes what they can reach in
//! `~/.pi/settings.toml`; `examples/pi.toml` carries measured starting points.
//!
//! Two axes, kept apart: an **endpoint fact** rides `Format`, a **model
//! fact** (e.g. temperature support) sits on `ModelSpec`.

use serde::{Deserialize, Serialize};

/// Which native format an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum Format {
    // `POST /v1/messages`
    Anthropic { cache_control: CacheControl },
    // `POST /v1/responses`
    OpenAi,
    // `POST /chat/completions`, the OpenAI chat wire
    Chat,
}

impl Format {
    /// The name this format goes by everywhere it is written down: the config
    /// key, the journal's `format` field, an API error's word — not the
    /// credential contract, since OpenAI-family wires share one key.
    pub fn name(&self) -> &'static str {
        match self {
            Format::Anthropic { .. } => "anthropic",
            Format::OpenAi => "openai",
            Format::Chat => "chat",
        }
    }
}

/// Whether to ask an Anthropic endpoint to cache, and for how long.
///
/// Two bools would allow a meaningless state ("say nothing, keep it an
/// hour"); one field is all there is to send — the API places the breakpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControl {
    // Say nothing at all. An endpoint nobody has measured is not told to
    // cache: an unknown top-level field is a 400 on some of them.
    #[default]
    Off,
    // `{"type": "ephemeral"}`, the five-minute window.
    Standard,
    // `{"type": "ephemeral", "ttl": "1h"}`, at twice the write price. The
    // canonical API honors it; a shim may accept it and do nothing.
    LongTtl,
}

/// How a model takes an instruction to think. Not a format discriminant:
/// Anthropic has two of these and OpenAI the third.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingControl {
    // Anthropic 4.6 and later: `thinking: {type: "adaptive"}`, with the depth
    // set by the separate top-level `output_config.effort`.
    Adaptive,
    // Anthropic 4.5 and earlier: `thinking: {type: "enabled", budget_tokens}`.
    // 4.7 and later reject this shape outright.
    Budget,
    // OpenAI Responses: the top-level `reasoning: {effort}` object.
    Effort,
}

/// What becomes of *foreign* reasoning when the transcript goes back out —
/// blocks another model produced, or this one produced unsigned.
///
/// Signed/encrypted reasoning from the target itself always replays as-is;
/// that's decided by comparing origins, not by this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayThinking {
    // Leave it out — what the docs say to do on a model change, and cheapest:
    // a target that ignores a foreign block still bills for reading it.
    #[default]
    Off,
    // Wrapped in `<think>`, for the models trained to read that.
    Tagged,
}

/// `default` so a config can price only the halves it knows: an unstated rate
/// is zero, and a zero rate reports no cost rather than a wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_write_per_mtok: f64,
}

/// One model, resolved: what the provider supplies folded together with what
/// the model itself does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// The model, as the endpoint names it — `deepseek-v4-flash`. Selection,
    /// usage attribution, the archive and the request's own `model` field all
    /// key on this one string; there is no second name for it.
    pub model: String,
    pub base_url: String,
    pub format: Format,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub vision: bool,
    /// Absent when the model takes no thinking instruction at all.
    pub thinking: Option<ThinkingControl>,
    pub replay_thinking: ReplayThinking,
    /// Anthropic 4.6+ and the OpenAI reasoning models reject every value but
    /// the default, so the only request they accept is one that omits it.
    pub accepts_temperature: bool,
    /// Fable and Mythos reject a forced `tool_choice`; it downgrades to auto.
    pub can_force_tool: bool,
    pub pricing: Pricing,
}

impl Pricing {
    /// What this usage costs at these rates. The multiplication lives here
    /// rather than with the loop: the loop has no business knowing money.
    pub fn cost(&self, usage: &crate::stream::Usage) -> f64 {
        let m = 1_000_000.0;
        usage.input as f64 / m * self.input_per_mtok
            + usage.output as f64 / m * self.output_per_mtok
            + usage.cache_read as f64 / m * self.cache_read_per_mtok
            + usage.cache_write as f64 / m * self.cache_write_per_mtok
    }
}

// `ModelSpec` has no `Default` on purpose: an id, model and base url have no
// sensible empty value in production, so tests get this helper instead.
#[cfg(test)]
impl ModelSpec {
    pub(crate) fn test() -> Self {
        ModelSpec {
            model: "test-model".into(),
            base_url: "http://localhost".into(),
            format: Format::Anthropic {
                cache_control: CacheControl::Off,
            },
            context_window: 200_000,
            max_output_tokens: 32_000,
            vision: true,
            thinking: None,
            replay_thinking: ReplayThinking::Tagged,
            accepts_temperature: true,
            can_force_tool: true,
            pricing: Pricing::default(),
        }
    }
}
