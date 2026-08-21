use serde::{Deserialize, Serialize};

/// Wire protocol plus its fully-resolved quirk record. One field, so a spec can
/// never pair one protocol's compat with another's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "wire", rename_all = "snake_case")]
pub enum Wire {
    Anthropic(AnthropicCompat),
    OpenAi(OpenAiCompat),
}

impl Wire {
    pub fn transport_name(&self) -> &'static str {
        match self {
            Wire::Anthropic(_) => "anthropic",
            Wire::OpenAi(_) => "openai",
        }
    }
}

/// Anthropic Messages quirks. Every field is materialized at construction;
/// transports read them and never inspect the model id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicCompat {
    /// Opus 4.7+ and Fable/Mythos reject `temperature`/`top_p`/`top_k` with 400.
    pub sampling_params: bool,
    /// Fable/Mythos reject a forced `tool_choice`; forced choices downgrade to auto.
    pub forced_tool_choice: bool,
    /// Z.AI's Anthropic-compatible proxy reads `.id` on tool_result blocks.
    pub tool_result_id_alias: bool,
    /// `cache_control` with `ttl: "1h"`; canonical API only.
    pub long_cache_retention: bool,
    /// Vertex rejects the `adaptive` thinking tag; map it to `enabled`.
    pub adaptive_thinking: bool,
}

impl Default for AnthropicCompat {
    fn default() -> Self {
        Self {
            sampling_params: true,
            forced_tool_choice: true,
            tool_result_id_alias: false,
            long_cache_retention: true,
            adaptive_thinking: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningField {
    ReasoningContent,
    Reasoning,
    ReasoningText,
}

/// OpenAI Chat Completions quirks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCompat {
    pub max_tokens_field: MaxTokensField,
    /// Strict chat templates (vLLM-served Qwen, MiniMax) accept only one leading
    /// system message; false coalesces them, at the cost of KV-cache reuse.
    pub multiple_system_messages: bool,
    /// Mistral requires exactly 9 alphanumeric characters.
    pub mistral_tool_ids: bool,
    /// Some hosts reject a `tool` message answered directly by a `user` message.
    pub assistant_after_tool_result: bool,
    pub tool_result_name: bool,
    pub reasoning_effort: bool,
    pub reasoning_field: ReasoningField,
    /// `stream_options: { include_usage: true }`; absent on some proxies.
    pub usage_in_streaming: bool,
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            multiple_system_messages: true,
            mistral_tool_ids: false,
            assistant_after_tool_result: false,
            tool_result_name: false,
            reasoning_effort: false,
            reasoning_field: ReasoningField::ReasoningContent,
            usage_in_streaming: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingSupport {
    /// Anthropic: an explicit token budget.
    Budget,
    /// OpenAI-family: a coarse effort level.
    Effort,
}

/// What survives of prior-turn reasoning when it is replayed. Belongs to the
/// target model, not the transport: the same endpoint serves models that differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingReplay {
    /// Replay the block with its signature intact.
    Signed,
    /// Replay as bare assistant prose. Anthropic's reasoning_extraction
    /// classifier rejects tag-wrapped prior reasoning.
    BareProse,
    /// Wrap in `<think>` before replaying.
    Tagged,
    /// Target discards it; drop rather than pay the tokens.
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tools: bool,
    pub parallel_tool_calls: bool,
    pub vision: bool,
    pub thinking: Option<ThinkingSupport>,
    /// Honors explicit cache breakpoints rather than automatic prefix caching.
    pub cache_breakpoints: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_write_per_mtok: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Local handle: selection, usage attribution, config all key on this.
    pub id: String,
    /// What goes on the wire when it differs from `id`.
    pub wire_id: String,
    pub base_url: String,
    pub wire: Wire,
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub caps: Capabilities,
    pub thinking_replay: ThinkingReplay,
    pub pricing: Pricing,
}

impl ModelSpec {
    pub fn transport_name(&self) -> &'static str {
        self.wire.transport_name()
    }

    pub fn cost(&self, usage: &crate::stream::Usage) -> f64 {
        let m = 1_000_000.0;
        usage.input as f64 / m * self.pricing.input_per_mtok
            + usage.output as f64 / m * self.pricing.output_per_mtok
            + usage.cache_read as f64 / m * self.pricing.cache_read_per_mtok
            + usage.cache_write as f64 / m * self.pricing.cache_write_per_mtok
    }
}

const ANTHROPIC_BASE: &str = "https://api.anthropic.com";

/// The models we actually run. Hand-written on purpose: a borrowed quirk table
/// carries entries you cannot tell have gone stale.
pub fn builtin() -> Vec<ModelSpec> {
    vec![
        ModelSpec {
            id: "opus-5".into(),
            wire_id: "claude-opus-5".into(),
            base_url: ANTHROPIC_BASE.into(),
            wire: Wire::Anthropic(AnthropicCompat {
                sampling_params: false,
                ..Default::default()
            }),
            context_window: 200_000,
            max_output_tokens: 64_000,
            caps: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                thinking: Some(ThinkingSupport::Budget),
                cache_breakpoints: true,
            },
            thinking_replay: ThinkingReplay::Signed,
            pricing: Pricing {
                input_per_mtok: 5.0,
                output_per_mtok: 25.0,
                cache_read_per_mtok: 0.5,
                cache_write_per_mtok: 6.25,
            },
        },
        ModelSpec {
            id: "sonnet-5".into(),
            wire_id: "claude-sonnet-5".into(),
            base_url: ANTHROPIC_BASE.into(),
            wire: Wire::Anthropic(AnthropicCompat {
                sampling_params: false,
                ..Default::default()
            }),
            context_window: 200_000,
            max_output_tokens: 64_000,
            caps: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                vision: true,
                thinking: Some(ThinkingSupport::Budget),
                cache_breakpoints: true,
            },
            thinking_replay: ThinkingReplay::Signed,
            pricing: Pricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cache_read_per_mtok: 0.3,
                cache_write_per_mtok: 3.75,
            },
        },
    ]
}

pub fn find(id: &str) -> Option<ModelSpec> {
    builtin()
        .into_iter()
        .find(|m| m.id == id || m.wire_id == id)
}
