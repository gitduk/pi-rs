//! Startup defaults and locally-defined models.
//!
//! A working setup should not be a command line to retype. Pi splits this into
//! a provider catalog (`models.json`) and preferences (`settings.json`); the
//! split earns its keep there because the catalog is a thing you copy between
//! machines. One file with two sections is the same idea with less to find.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use brain::catalog::{
    AnthropicCompat, Capabilities, ModelSpec, OpenAiCompat, Pricing, ThinkingReplay,
    ThinkingSupport, Wire,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{EffortArg, TierArg, WireArg};

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    /// Models this machine can reach that the built-in catalog cannot know
    /// about — a local proxy, a self-hosted server, a private deployment.
    #[serde(default)]
    pub models: Vec<Entry>,
}

/// What a flag would have said. Every one is optional: an absent key leaves the
/// flag's own default in place.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    pub model: Option<String>,
    pub effort: Option<EffortArg>,
    pub tier: Option<TierArg>,
    pub max_turns: Option<usize>,
    /// Path to a file replacing the built-in system prompt.
    pub system: Option<String>,
}

fn default_context() -> u32 {
    128_000
}

fn default_output() -> u32 {
    8_192
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// The handle `-m` selects by.
    pub id: String,
    /// What goes on the wire, when the host names the model differently.
    pub wire_id: Option<String>,
    pub base_url: String,
    pub wire: WireArg,
    #[serde(default = "default_context")]
    pub context_window: u32,
    #[serde(default = "default_output")]
    pub max_output_tokens: u32,
    /// How the model takes a thinking instruction, absent if it takes none.
    pub thinking: Option<ThinkingSupport>,
    pub thinking_replay: Option<ThinkingReplay>,
    #[serde(default)]
    pub vision: bool,
    /// Honors explicit cache breakpoints rather than caching a prefix by itself.
    #[serde(default)]
    pub cache_breakpoints: bool,
    #[serde(default)]
    pub pricing: Pricing,
    /// Only the quirks that differ from the wire's defaults.
    #[serde(default)]
    pub compat: serde_json::Map<String, serde_json::Value>,
    /// Environment variable holding the key. Defaults to the wire's usual one.
    pub api_key_env: Option<String>,
    /// A key written into the file. For a local server that only wants some
    /// non-empty string; anything actually secret belongs in `api_key_env`,
    /// because this file is as readable as its permissions make it.
    pub api_key: Option<String>,
}

/// The wire's defaults with the entry's overrides on top, so a config names
/// only the quirk it needs to change.
fn compat<T: DeserializeOwned + serde::Serialize + Default>(
    overrides: &serde_json::Map<String, serde_json::Value>,
) -> Result<T> {
    let mut base = serde_json::to_value(T::default())?;
    let serde_json::Value::Object(map) = &mut base else {
        unreachable!("a compat record is a struct")
    };
    for (key, value) in overrides {
        if !map.contains_key(key) {
            // Naming the alternatives: a typo here is otherwise a quirk that
            // silently stays at its default and a 400 much later.
            let known: Vec<&str> = map.keys().map(String::as_str).collect();
            bail!(
                "unknown compat key `{key}`; this wire has {}",
                known.join(", ")
            );
        }
        map.insert(key.clone(), value.clone());
    }
    serde_json::from_value(base).context("compat values did not fit the wire's record")
}

impl Entry {
    pub fn spec(&self) -> Result<ModelSpec> {
        let wire = match self.wire {
            WireArg::Anthropic => Wire::Anthropic(compat::<AnthropicCompat>(&self.compat)?),
            WireArg::Openai => Wire::OpenAi(compat::<OpenAiCompat>(&self.compat)?),
        };
        Ok(ModelSpec {
            id: self.id.clone(),
            wire_id: self.wire_id.clone().unwrap_or_else(|| self.id.clone()),
            base_url: self.base_url.clone(),
            wire,
            context_window: self.context_window,
            max_output_tokens: self.max_output_tokens,
            caps: Capabilities {
                tools: true,
                parallel_tool_calls: true,
                vision: self.vision,
                thinking: self.thinking,
                cache_breakpoints: self.cache_breakpoints,
            },
            // The conservative one: a model that discards tagged reasoning is
            // cheaper to be wrong about than one that rejects the request.
            thinking_replay: self.thinking_replay.unwrap_or(ThinkingReplay::Tagged),
            pricing: self.pricing,
        })
    }

    /// The key to send, and where it came from, or None to leave it to the
    /// wire's usual environment variable.
    pub fn key(&self) -> Option<Result<String>> {
        if let Some(literal) = &self.api_key {
            return Some(Ok(literal.clone()));
        }
        let name = self.api_key_env.as_ref()?;
        Some(
            std::env::var(name)
                .with_context(|| format!("{} names ${name}, which is not set", self.id)),
        )
    }
}

impl Config {
    /// A config entry wins over a built-in one of the same name: the built-in
    /// table is our guess about a public endpoint, and the user is looking at
    /// the actual server.
    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.models
            .iter()
            .find(|m| m.id == id || m.wire_id.as_deref() == Some(id))
    }

    pub fn ids(&self) -> Vec<&str> {
        self.models.iter().map(|m| m.id.as_str()).collect()
    }
}

/// Where the config lives when the user has not said.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("pi/config.json"))
}

/// Read the config, if there is one.
///
/// A file named explicitly and missing is an error — the user asked for it. The
/// default location missing is the ordinary case and says nothing.
pub fn load(explicit: Option<&str>) -> Result<Config> {
    let (path, required) = match explicit {
        Some(p) => (PathBuf::from(p), true),
        None => match default_path() {
            Some(p) => (p, false),
            None => return Ok(Config::default()),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(body) => parse(&body).with_context(|| format!("{}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn parse(body: &str) -> Result<Config> {
    let config: Config = serde_json::from_str(body)?;
    let mut seen = std::collections::HashSet::new();
    for entry in &config.models {
        // Rejected here rather than at use: a typo in a model you are not
        // running today is still a typo, and this is when it is cheap to see.
        entry.spec()?;
        if entry.api_key.is_some() && entry.api_key_env.is_some() {
            bail!(
                "{}: api_key and api_key_env both set; one of them would be \
                 silently ignored",
                entry.id
            );
        }
        if !seen.insert(&entry.id) {
            // Otherwise the second one is dead config that looks live.
            bail!("two models are called `{}`", entry.id);
        }
    }
    Ok(config)
}

/// A key written into a file others can read is worth one line of warning.
pub fn warn_if_exposed(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        if meta.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "\x1b[2m{} holds a key and is readable by others; chmod 600 it\x1b[0m",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, parse};
    use brain::catalog::{MaxTokensField, ThinkingSupport, Wire};

    const SAMPLE: &str = r#"{
      "defaults": { "model": "flash", "effort": "medium" },
      "models": [{
        "id": "flash",
        "base_url": "http://localhost:7896/v1",
        "wire": "openai",
        "context_window": 1000000,
        "max_output_tokens": 384000,
        "thinking": "effort",
        "api_key": "x",
        "compat": { "max_tokens_field": "max_tokens", "usage_in_streaming": false }
      }]
    }"#;

    #[test]
    fn a_config_supplies_what_the_flags_would_have() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.defaults.model.as_deref(), Some("flash"));
        let spec = c.find("flash").unwrap().spec().unwrap();
        assert_eq!(spec.context_window, 1_000_000);
        assert_eq!(spec.caps.thinking, Some(ThinkingSupport::Effort));
    }

    #[test]
    fn compat_names_only_what_differs() {
        let spec = parse(SAMPLE)
            .unwrap()
            .find("flash")
            .unwrap()
            .spec()
            .unwrap();
        let Wire::OpenAi(c) = spec.wire else {
            panic!("openai")
        };
        assert_eq!(c.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(!c.usage_in_streaming);
        // Untouched keys keep the wire's default rather than a zero value.
        assert!(c.multiple_system_messages);
    }

    #[test]
    fn a_misspelled_compat_key_is_named_along_with_the_real_ones() {
        // Otherwise the quirk quietly stays at its default and the 400 arrives
        // much later, pointing at nothing.
        let body = SAMPLE.replace("usage_in_streaming", "usage_in_stream");
        let e = parse(&body).unwrap_err().to_string();
        assert!(e.contains("usage_in_stream"), "{e}");
        assert!(e.contains("usage_in_streaming"), "{e}");
    }

    #[test]
    fn an_id_the_wire_spells_differently_is_still_findable() {
        let body = r#"{"models":[{"id":"flash","wire_id":"deepseek-v4-flash",
          "base_url":"http://x/v1","wire":"openai"}]}"#;
        let c = parse(body).unwrap();
        assert!(c.find("flash").is_some());
        assert!(c.find("deepseek-v4-flash").is_some());
        assert_eq!(
            c.find("flash").unwrap().spec().unwrap().wire_id,
            "deepseek-v4-flash"
        );
    }

    #[test]
    fn an_empty_config_is_a_valid_one() {
        assert!(parse("{}").unwrap().models.is_empty());
        assert!(Config::default().defaults.model.is_none());
    }

    #[test]
    fn a_key_that_is_not_a_setting_is_refused_rather_than_ignored() {
        // A silently dropped key looks like a setting that does not work.
        let e = parse(r#"{"defaults":{"modle":"x"}}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("modle"), "{e}");
    }

    #[test]
    fn a_key_given_two_ways_is_refused_rather_than_one_being_dropped() {
        let body = r#"{"models":[{"id":"a","base_url":"http://x/v1","wire":"openai",
          "api_key":"lit","api_key_env":"SOME_VAR"}]}"#;
        let e = parse(body).unwrap_err().to_string();
        assert!(e.contains("silently ignored"), "{e}");
    }

    #[test]
    fn a_repeated_id_is_refused_rather_than_the_second_going_dead() {
        let one = r#"{"id":"a","base_url":"http://x/v1","wire":"openai"}"#;
        let body = format!(r#"{{"models":[{one},{one}]}}"#);
        assert!(parse(&body).unwrap_err().to_string().contains("two models"));
    }
}
