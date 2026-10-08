//! The values a flag and a config file both take, spelled the same in each.

use clap::ValueEnum;
use llm::model::{CacheControl, Format};

#[derive(Debug, Clone, Copy, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatArg {
    Anthropic,
    Openai,
    Chat,
}

impl FormatArg {
    // Caching stays off: nothing on this path was measured, and an
    // unknown top-level field is a 400 on some servers.
    fn format(self) -> Format {
        match self {
            FormatArg::Anthropic => Format::Anthropic {
                cache_control: CacheControl::Off,
            },
            FormatArg::Openai => Format::OpenAi,
            FormatArg::Chat => Format::Chat,
        }
    }

    /// Delegated rather than matched again, so a model `/model` lists and one
    /// it has just switched to cannot print two names for the same protocol.
    pub fn name(self) -> &'static str {
        self.format().name()
    }
}

/// `tool::Tier` as the command line and the config files spell it. Not
/// ordered, because the tiers are not: see `tool::Tier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TierArg {
    Read,
    Write,
    Exec,
    Net,
}

impl TierArg {
    /// As the command line and the config files spell it.
    pub fn name(self) -> String {
        self.to_possible_value()
            .map(|v| v.get_name().to_string())
            .unwrap_or_default()
    }
}

impl From<TierArg> for tool::Tier {
    fn from(arg: TierArg) -> Self {
        match arg {
            TierArg::Read => tool::Tier::Read,
            TierArg::Write => tool::Tier::Write,
            TierArg::Exec => tool::Tier::Exec,
            TierArg::Net => tool::Tier::Net,
        }
    }
}

impl From<tool::Tier> for TierArg {
    fn from(tier: tool::Tier) -> Self {
        match tier {
            tool::Tier::Read => TierArg::Read,
            tool::Tier::Write => TierArg::Write,
            tool::Tier::Exec => TierArg::Exec,
            tool::Tier::Net => TierArg::Net,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortArg {
    Off,
    Low,
    Medium,
    High,
}

impl EffortArg {
    /// Every level as typed, in order: what `/effort` offers and names.
    pub fn names() -> Vec<String> {
        Self::value_variants()
            .iter()
            .filter_map(|v| Some(v.to_possible_value()?.get_name().to_string()))
            .collect()
    }
}
