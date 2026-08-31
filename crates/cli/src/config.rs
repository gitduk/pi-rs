//! Startup defaults and locally-defined models.
//!
//! A working setup should not be a command line to retype. Pi keeps a provider
//! catalog (`~/.pi/agent/models.json`) apart from preferences
//! (`settings.toml`); the split earns its keep there because the catalog is a
//! thing you copy between machines. One file with two sections is the same idea
//! with less to find.
//!
//! TOML rather than JSON for one reason: most of what a model entry holds is a
//! measurement, and a measurement without its provenance rots. `thinking =
//! "budget"` needs the comment saying which endpoint that was tried against,
//! and JSON has nowhere to put it.
//!
//! Providers own the connection, models own themselves. Seven models behind one
//! endpoint used to mean seven copies of its url and key; now the endpoint is
//! written once and the models hang off it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use brain::model::{
    CacheControl, Format, ModelSpec, Pricing, ReplayThinking, ThinkingControl,
};
use serde::Deserialize;

use crate::{EffortArg, TierArg, FormatArg};

/// The user's own file: `~/.pi/settings.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The endpoint this machine talks to. One, because pi talks to one at a
    /// time: a map of them made every model's name a `provider.model` pair,
    /// and that pair is what the config, the archive and the reasoning stamp
    /// each had to spell in their own way.
    pub base_url: Option<String>,
    pub format: Option<FormatArg>,
    /// `$NAME` reads that environment variable; anything else is the key
    /// itself. A key that genuinely begins with `$` cannot be written literally
    /// — put it in a variable.
    pub api_key: Option<String>,
    /// Anthropic only, and off unless someone measured it: an unknown top-level
    /// field is a 400 on some servers.
    #[serde(default)]
    pub cache_control: CacheControl,

    /// The model to run, as the endpoint names it: `deepseek-v4-flash`.
    pub model: Option<String>,
    /// Who writes the summary when history is compacted. Defaults to `model`.
    ///
    /// Named for the job, not for a tier: `lite_model` would be a category with
    /// one member and no test for membership, and every task added after would
    /// have to argue about whether it qualifies.
    pub summarize_model: Option<String>,
    pub effort: Option<EffortArg>,
    pub tier: Option<TierArg>,
    /// Path to a file replacing the built-in system prompt.
    pub system: Option<String>,

    /// Facts about a model the defaults get wrong, keyed by the model's own
    /// name. Every field has a default, so a model needs an entry only where
    /// it differs — and needs none at all to be usable.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
    /// Key actions, each mapped to the presses that trigger it. An entry
    /// replaces that action's defaults rather than adding to them.
    #[serde(default)]
    pub keys: BTreeMap<String, Binds>,
    /// The SGR codes behind every colour the terminal uses.
    #[serde(default)]
    pub theme: crate::render::Theme,
}

/// One key or several — `"ctrl+g"` and `["ctrl+g", "f5"]` both mean the same
/// thing for an action with a single binding, and requiring the brackets for
/// the common case would be noise.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Binds {
    One(String),
    Many(Vec<String>),
}

impl Binds {
    fn into_vec(self) -> Vec<String> {
        match self {
            Binds::One(s) => vec![s],
            Binds::Many(v) => v,
        }
    }
}

impl Config {
    /// The key table this config asks for, defaults included.
    pub fn key_map(&self) -> Result<crate::keys::Keys> {
        let overrides = self
            .keys
            .iter()
            .map(|(id, b)| (id.clone(), b.clone().into_vec()))
            .collect();
        crate::keys::Keys::resolve(&overrides)
    }
}

/// A `.pi.toml` inside a repository.
///
/// A repository is not a trusted source — it arrives by `git clone` from
/// someone else. Anything that could point the run at a server of its own
/// choosing (a base url, a key, a wire quirk) is absent by construction, and so
/// is `system`, which would let a checkout name any file on disk and have its
/// contents sent to the provider. What is left can only pick among models the
/// user has already defined and turn the dials on how hard the run works.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub model: Option<String>,
    pub effort: Option<EffortArg>,
    /// A ceiling, applied downward only: a checkout may declare itself
    /// read-only, never hand itself the shell.
    pub max_tier: Option<TierArg>,
}

fn default_context() -> u32 {
    128_000
}

fn default_output() -> u32 {
    8_192
}

fn yes() -> bool {
    true
}

/// One model. Every field here travels with the model whoever serves it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    #[serde(default = "default_context")]
    pub context_window: u32,
    #[serde(default = "default_output")]
    pub max_output_tokens: u32,
    /// How the model takes a thinking instruction, absent if it takes none.
    pub thinking: Option<ThinkingControl>,
    #[serde(default)]
    pub replay_thinking: ReplayThinking,
    #[serde(default)]
    pub vision: bool,
    /// Anthropic 4.6+ and the OpenAI reasoning models reject every value.
    #[serde(default = "yes")]
    pub accepts_temperature: bool,
    /// Fable and Mythos reject a forced `tool_choice`.
    #[serde(default = "yes")]
    pub can_force_tool: bool,
    #[serde(default)]
    pub pricing: Pricing,
}

// The shape a model gets when the file does not describe it. Written out
// rather than derived: `derive(Default)` would zero the window, and a zero
// window is a budget of nothing rather than a stated guess.
impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            context_window: default_context(),
            max_output_tokens: default_output(),
            thinking: None,
            replay_thinking: ReplayThinking::default(),
            vision: false,
            accepts_temperature: true,
            can_force_tool: true,
            pricing: Pricing::default(),
        }
    }
}

impl Config {
    /// The endpoint's shape, refused rather than guessed: naming the wrong one
    /// is a 400 on the first turn, and neither is a safer bet than the other.
    /// The endpoint's shape, refused rather than guessed: naming the wrong one
    /// is a 400 on the first turn, and neither is a safer bet than the other.
    fn format(&self) -> Result<Format> {
        let named = self
            .format
            .context("`format` is required: \"anthropic\", \"openai\" or \"chat\"")?;
        Ok(match named {
            FormatArg::Anthropic => Format::Anthropic {
                cache_control: self.cache_control,
            },
            FormatArg::Openai => Format::OpenAi,
            FormatArg::Chat => Format::Chat,
        })
    }

    fn spec(&self, name: &str, model: &ModelEntry) -> Result<ModelSpec> {
        let format = self.format()?;
        if !matches!(format, Format::Anthropic { .. }) && self.cache_control != CacheControl::Off {
            bail!(
                "cache_control is an Anthropic field; the openai format caches by \
                 default and naming it here can only turn that off"
            );
        }
        let base_url = self
            .base_url
            .clone()
            .context("`base_url` is required to reach a model")?;
        Ok(ModelSpec {
            model: name.to_string(),
            base_url,
            format,
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            vision: model.vision,
            thinking: model.thinking,
            replay_thinking: model.replay_thinking,
            accepts_temperature: model.accepts_temperature,
            can_force_tool: model.can_force_tool,
            pricing: model.pricing,
        })
    }

    /// A `$NAME` that names nothing is a typo now and a missing key much later,
    /// pointing at the endpoint rather than at the file.
    fn check_key(&self) -> Result<()> {
        let Some(name) = self.api_key.as_deref().and_then(|k| k.strip_prefix('$')) else {
            return Ok(());
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!(
                "api_key `${name}` does not name an environment variable. \
                 A literal key beginning with `$` cannot be written here — put it in a variable."
            );
        }
        Ok(())
    }

    /// A thinking control the format cannot carry is otherwise accepted, then
    /// silently dropped when the request is built.
    fn check_thinking(&self, name: &str, model: &ModelEntry) -> Result<()> {
        match (self.format, model.thinking) {
            (Some(FormatArg::Anthropic), Some(ThinkingControl::Effort)) => bail!(
                "{name}: thinking = \"effort\" is not an Anthropic control; use \
                 \"adaptive\" (Claude 4.6 and later) or \"budget\" (4.5 and earlier)"
            ),
            (
                Some(FormatArg::Openai | FormatArg::Chat),
                Some(t @ (ThinkingControl::Adaptive | ThinkingControl::Budget)),
            ) => {
                let named = match t {
                    ThinkingControl::Adaptive => "adaptive",
                    _ => "budget",
                };
                bail!(
                    "{name}: thinking = \"{named}\" is Anthropic-only; \
                     this format takes \"effort\""
                )
            }
            _ => Ok(()),
        }
    }
}

/// Who asked for the model.
///
/// Carried alongside the name so that an unknown one can say where it came
/// from: a bare `pi` that fails on a name the user never typed is a mystery
/// with three files to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Flag,
    Command,
    Resumed,
    Project,
    Global,
    OnlyModel,
}

impl Origin {
    pub fn describe(self) -> &'static str {
        match self {
            Origin::Flag => "-m",
            Origin::Command => "/model",
            Origin::Resumed => "the resumed session",
            Origin::Project => "defaults.model in the project's .pi.toml",
            Origin::Global => "defaults.model in ~/.pi/settings.toml",
            Origin::OnlyModel => "the only model in ~/.pi/settings.toml",
        }
    }
}

/// What the flags said, so the chain below can be resolved in one place.
#[derive(Debug, Default, Clone, Copy)]
pub struct Flags {
    pub effort: Option<EffortArg>,
    pub tier: Option<TierArg>,
}

#[derive(Debug, Clone, Copy)]
pub struct Settled {
    pub effort: EffortArg,
    pub tier: TierArg,
}

impl Config {
    /// The spec for a model name. Every name resolves: the endpoint is the
    /// same one either way, and an entry only ever supplies facts the defaults
    /// get wrong.
    ///
    /// A name the file does not list is passed through with default numbers.
    /// Probing the endpoint for its catalog would cost a round trip and still
    /// not answer the one question that matters — how wide the window is — so
    /// the alternative to guessing is writing ten models down to reach one.
    pub fn find(&self, name: &str) -> Result<ModelSpec> {
        match self.models.get(name) {
            Some(model) => self.spec(name, model),
            None => self.spec(name, &ModelEntry::default()),
        }
    }

    /// Whether the file actually describes this model, as against one passed
    /// through with default numbers.
    pub fn is_written(&self, name: &str) -> bool {
        self.models.contains_key(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    /// The credential to send, or None when the endpoint wants none.
    ///
    /// Read at use rather than at load, so a run needs only the variable it
    /// actually reaches for. A `$NAME` that names an unset variable is the
    /// same as no key: the request goes out without one and the endpoint's
    /// response says what it needs. The *shape* is checked at load, because a
    /// malformed `$NAME` is a typo today and every day after.
    pub fn key(&self) -> Option<String> {
        let raw = self.api_key.as_deref()?;
        match raw.strip_prefix('$') {
            None => Some(raw.to_string()),
            Some(name) => std::env::var(name).ok(),
        }
    }

    /// Flag, then project, then this file, then the built-in default.
    ///
    /// The tier is the exception: a project may only pull it down, so it is
    /// applied as a ceiling after the rest of the chain has decided.
    pub fn settle(&self, project: &Project, flags: Flags) -> Settled {
        let tier = flags
            .tier
            .or(self.tier)
            .unwrap_or(TierArg::Exec)
            .min(project.max_tier.unwrap_or(TierArg::Exec));
        Settled {
            effort: flags
                .effort
                .or(project.effort)
                .or(self.effort)
                .unwrap_or(EffortArg::Off),
            tier,
        }
    }

    /// A resumed run stays on the model that produced the transcript, so `prior`
    /// outranks both files: continuing is continuing, and a project default
    /// that quietly moved a half-finished session elsewhere would be a surprise
    /// nobody asked for. `/model` is the deliberate way to move it.
    ///
    /// A config that defines exactly one model and names no default means that
    /// one: there is nothing else it could mean, and making the user write the
    /// name twice only creates the chance to write it differently.
    ///
    /// None when nothing named one. There is no fallback to a model we picked:
    /// a hardcoded name is a claim about what exists, and it goes stale the
    /// week a vendor ships something.
    pub fn model(
        &self,
        project: &Project,
        flag: Option<&str>,
        prior: Option<&str>,
    ) -> Option<(String, Origin)> {
        if let Some(m) = flag {
            return Some((m.to_string(), Origin::Flag));
        }
        if let Some(m) = prior {
            return Some((m.to_string(), Origin::Resumed));
        }
        if let Some(m) = &project.model {
            return Some((m.clone(), Origin::Project));
        }
        if let Some(m) = &self.model {
            return Some((m.clone(), Origin::Global));
        }
        // One model written down: there is nothing else it could mean, and
        // making the name be written twice only creates the chance to write it
        // differently.
        if let [only] = self.names().as_slice() {
            return Some((only.clone(), Origin::OnlyModel));
        }
        None
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Where the user's own config lives when they have not said otherwise.
pub fn global_path() -> Option<PathBuf> {
    tools::state::dir().map(|root| root.join("settings.toml"))
}

/// The nearest project file at or above `start`, stopping at the repository
/// root.
///
/// `home` is never searched: `~/.pi/settings.toml` is the global file, and
/// treating it as a project file too would hand it privileges the global file
/// already has by other means — and hand every directory under `$HOME` outside
/// a repo the same file as its "project" config.
pub fn project_path(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if home == Some(dir) {
            return None;
        }
        let candidate = dir.join(".pi.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        // Above the repository root is not this project any more.
        if dir.join(".git").exists() {
            return None;
        }
    }
    None
}

/// Read the user's config, if there is one.
///
/// A file named explicitly and missing is an error — the user asked for it. The
/// default location missing is the ordinary case and says nothing.
pub fn load(explicit: Option<&str>) -> Result<Config> {
    let (path, required) = match explicit {
        Some(p) => (PathBuf::from(p), true),
        None => match global_path() {
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

pub fn load_project(workspace: &Path) -> Result<Project> {
    let Some(path) = project_path(workspace, home().as_deref()) else {
        return Ok(Project::default());
    };
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    parse_project(&body).with_context(|| format!("{}", path.display()))
}

fn parse(body: &str) -> Result<Config> {
    migrated(body)?;
    retired(body)?;
    let de = toml::de::Deserializer::parse(body)?;
    let config: Config = serde_path_to_error::deserialize(de)?;
    config.check_key()?;
    for (model, entry) in &config.models {
        // Rejected here rather than at use: a typo in a model you are not
        // running today is still a typo, and this is when it is cheap to see.
        config.spec(model, entry)?;
        config.check_thinking(model, entry)?;
    }
    config.key_map()?;
    Ok(config)
}

// A key that was removed rather than renamed. `deny_unknown_fields` refuses it
// already, but only by name — this says why, which is the part a file that
// worked yesterday actually needs.
fn retired(body: &str) -> Result<()> {
    if body
        .lines()
        .any(|l| l.trim_start().starts_with("max_turns"))
    {
        bail!(
            "`max_turns` is gone, and with it the turn cap. A run now ends when \
             the model stops, when you interrupt it, or when the transport gives \
             up — a repeated call is named rather than counted. Delete the line."
        );
    }
    Ok(())
}

// The two shapes that came before this one. `deny_unknown_fields` would refuse
// them too, but with "unknown field `provider`" — true, and no help at all to
// someone holding a file that worked yesterday.
fn migrated(body: &str) -> Result<()> {
    // Matched as keys and tables, never as substrings: a comment that happens to
    // say "wire" is not a config in the old shape, and refusing a valid file is
    // worse than missing an invalid one.
    let retired_key = |line: &str| {
        ["wire", "wire_id", "api_key_env", "thinking_replay"]
            .iter()
            .any(|k| {
                line.strip_prefix(k)
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            })
    };
    // `wire` and friends predate providers by a shape. Both generations land
    // in the same place, so both are pointed there by one message.
    let old_shape = body.lines().map(str::trim_start).any(|line| {
        line.starts_with("[provider")
            || line.starts_with("[defaults]")
            || line.starts_with("[compat]")
            || retired_key(line)
    });
    if !old_shape {
        return Ok(());
    }
    bail!(
        "this config names a provider. pi talks to one endpoint at a time, so
the endpoint is the file itself and a model is just its own name:

  base_url = \"https://…\"
  api_key  = \"$YOUR_KEY\"       # a `$` reads the environment
  format   = \"anthropic\"       # or \"openai\"
  model    = \"claude-sonnet-5\" # what the endpoint calls it

  [models.\"claude-sonnet-5\"]  # only where a default is wrong
  context_window = 200_000

what moved:
  · [provider.p] base_url/format/api_key/cache_control → the top level
  · [defaults] model/effort/tier/system                → the top level
  · [provider.p.models.x]                              → [models.x]
  · defaults.summarize_with                            → summarize_model
  · a model's `model` key is gone: the table name is the model's own name,
    so `-m` and the archive spell it the one way the endpoint does
  · `provider.model` names are gone with it — `-m claude-sonnet-5`, not
    `-m anthropic.sonnet`

from the shape before that:
  · wire                → format
  · api_key_env = \"N\"  → api_key = \"$N\"
  · wire_id             → gone; the table name is the model's own name
  · thinking_replay     → replay_thinking; \"bare_prose\" is \"prose\",
                          \"drop\" is \"off\", and \"signed\" is gone
  · cache_breakpoints + [compat] long_cache_retention
                        → cache_control = \"standard\" | \"long_ttl\"
  · [compat] sampling_params    → accepts_temperature, on the model
  · [compat] forced_tool_choice → can_force_tool, on the model
  · every other [compat] key is gone: pi speaks the native Anthropic and
    OpenAI Responses formats, and an endpoint that needs adjusting belongs
    behind a gateway

A second endpoint is no longer a config shape. Point base_url at the one you
want, or keep two files and pass --config."
    )
}

fn parse_project(body: &str) -> Result<Project> {
    retired(body)?;
    toml::from_str(body).context(
        "a project .pi.toml may set only `model`, `effort` and `max_tier` — a \
         checkout does not get to name a server, a key, or a system prompt",
    )
}

/// A key written into a file others can read is worth one line of warning.
///
/// Returned rather than printed: the same check runs behind `/model`, where the
/// terminal is in raw mode and a stray `eprintln!` lands wherever the cursor
/// happens to be.
pub fn warn_if_exposed(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::metadata(path).ok()?;
        if meta.permissions().mode() & 0o077 != 0 {
            return Some(format!(
                "{} holds a key and is readable by others; chmod 600 it",
                path.display()
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
base_url = "http://localhost:7896/v1"
format = "openai"
api_key = "x"
model = "flash"
effort = "medium"

[models.flash]
context_window = 1_000_000
max_output_tokens = 384_000
thinking = "effort"

[models.flash.pricing]
input_per_mtok = 0.14
# An integer where a float is wanted, which TOML distinguishes and serde does not.
output_per_mtok = 0
"#;

    /// The fewest keys that parse.
    fn one(format: &str, extra: &str) -> String {
        format!("base_url = \"http://x/v1\"\nformat = \"{format}\"\n{extra}")
    }

    #[test]
    fn a_config_supplies_what_the_flags_would_have() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.model.as_deref(), Some("flash"));
        let spec = c.find("flash").unwrap();
        assert_eq!(spec.context_window, 1_000_000);
        assert_eq!(spec.thinking, Some(ThinkingControl::Effort));
        assert_eq!(spec.pricing.input_per_mtok, 0.14);
        assert_eq!(spec.pricing.output_per_mtok, 0.0);
    }

    #[test]
    fn the_shipped_example_parses() {
        // It is the file people copy from; a stale key in it fails on their
        // machine, not in CI.
        parse(include_str!("../../../examples/pi.toml")).unwrap();
    }

    /// The block the README opens with is what a new reader copies first, and
    /// it went on saying `[models.x]` with a `wire` key for a whole release
    /// after the parser stopped accepting either. Read out of the file rather
    /// than retyped here, because a copy is what rotted the first time.
    #[test]
    fn the_readme_config_example_parses() {
        let readme = include_str!("../../../README.md");
        let block = readme
            .split("```toml")
            .nth(1)
            .and_then(|b| b.split("```").next())
            .expect("the README shows a toml config block");
        parse(block).expect("the README's own example must load");
    }

    #[test]
    fn a_thinking_control_the_wire_cannot_carry_is_named_at_parse_time() {
        // Both directions used to be dropped when the request was built: the
        // config parsed, nothing warned, and the model just did not think.
        fn config(wire: &str, thinking: &str) -> String {
            format!(
                "base_url = \"http://x\"\nformat = \"{wire}\"\n\
                 [models.m]\nthinking = \"{thinking}\"\n"
            )
        }

        let err = parse(&config("anthropic", "effort")).unwrap_err().to_string();
        assert!(err.contains("adaptive") && err.contains("budget"), "{err}");

        for name in ["adaptive", "budget"] {
            let err = parse(&config("openai", name)).unwrap_err().to_string();
            assert!(err.contains(name) && err.contains("effort"), "{err}");
        }

        for (wire, thinking) in [("anthropic", "adaptive"), ("anthropic", "budget"), ("openai", "effort")] {
            assert!(parse(&config(wire, thinking)).is_ok(), "{wire}/{thinking}");
        }
    }

    #[test]
    fn a_setting_the_file_did_not_name_keeps_its_default() {
        let body = one(
            "anthropic",
            "cache_control = \"long_ttl\"\n\n[models.m]\naccepts_temperature = false\n",
        );
        let spec = parse(&body).unwrap().find("m").unwrap();
        assert!(!spec.accepts_temperature);
        // What the file did not name keeps the default that is true, which a
        // zero-valued record would silently have turned off.
        assert!(spec.can_force_tool);
        assert_eq!(
            spec.format,
            Format::Anthropic {
                cache_control: CacheControl::LongTtl
            }
        );
    }

    #[test]
    fn a_misspelled_key_is_refused_rather_than_ignored() {
        // Otherwise the quirk quietly stays at its default and the 400 arrives
        // much later, pointing at nothing.
        let body = one("anthropic", "[models.m]\naccepts_temperatur = false\n");
        let e = parse(&body).unwrap_err().to_string();
        assert!(e.contains("accepts_temperatur"), "{e}");
    }

    /// The old shapes are found by their keys and tables. Said in prose — a
    /// comment, a system-prompt path, a model whose name contains one — they
    /// are just words, and refusing a valid file over them is the worse error.
    #[test]
    fn a_valid_config_that_merely_mentions_the_old_words_still_loads() {
        let body = "# the wire this speaks, and what [compat] used to hold\n\
                    base_url = \"http://x/v1\"\n\
                    format = \"openai\"\n\
                    model = \"wire_id-thinking_replay\"\n";
        assert!(parse(body).is_ok(), "{:?}", parse(body).err());
    }

    /// A file that worked yesterday must say what to do, not "unknown field".
    #[test]
    fn the_shapes_before_this_one_are_named_and_pointed_somewhere() {
        // One shape back: the endpoint was a provider table.
        let provider = "[provider.p]\nbase_url = \"http://x/v1\"\nformat = \"openai\"\n";
        let e = parse(provider).unwrap_err().to_string();
        assert!(e.contains("[models.") && e.contains("base_url"), "{e}");

        // Two shapes back: no providers, and `wire` for the format.
        let older = "[models.flash]\nbase_url = \"http://x/v1\"\nwire = \"openai\"\n";
        let e = parse(older).unwrap_err().to_string();
        // Every rename it might be holding is named, not just the section.
        for hint in ["wire", "api_key_env", "wire_id", "thinking_replay", "cache_control"] {
            assert!(e.contains(hint), "{hint} unmentioned: {e}");
        }
    }

    /// Each of these configured a shape only Chat Completions had. There is no
    /// quirk record left to put them in, so they fail here rather than being
    /// accepted and then quietly ignored on the wire.
    #[test]
    fn a_quirk_the_responses_api_never_had_is_refused_rather_than_ignored() {
        for key in [
            "max_tokens_field = \"max_tokens\"",
            "usage_in_streaming = false",
            "multiple_system_messages = false",
            "tool_result_name = true",
            "reasoning_field = \"reasoning\"",
            "sampling_params = false",
        ] {
            let body = one("openai", &format!("[models.m]\n{key}\n"));
            assert!(parse(&body).is_err(), "{key} was accepted");
        }
    }

    #[test]
    fn a_model_the_file_does_not_list_is_passed_through_to_its_provider() {
        // Asking the endpoint for its catalog costs a round trip and still
        // cannot answer the one question that matters — how wide the window is.
        let c = parse(SAMPLE).unwrap();
        let spec = c.find("some-other-model").unwrap();
        assert_eq!(spec.model, "some-other-model");
        assert_eq!(spec.base_url, "http://localhost:7896/v1");
        assert_eq!(spec.context_window, 128_000, "the default, not flash's");
        assert!(!c.is_written("some-other-model"));
        assert!(c.is_written("flash"));
    }

    #[test]
    fn a_key_may_be_a_literal_or_name_a_variable() {
        let lit = parse(&one("openai", "")).unwrap();
        assert!(lit.key().is_none(), "no api_key means none");

        let env = parse(&one("openai", "api_key = \"$PI_TEST_KEY\"\n")).unwrap();
        unsafe { std::env::set_var("PI_TEST_KEY", "secret") };
        assert_eq!(env.key().unwrap(), "secret");

        let raw = parse(&one("openai", "api_key = \"literal\"\n")).unwrap();
        assert_eq!(raw.key().unwrap(), "literal");

        // The shape is a typo today and every day after, so it is caught at
        // load; the value is not read until the provider is actually used.
        let bad = parse(&one("openai", "api_key = \"$not a name\"\n"));
        assert!(bad.unwrap_err().to_string().contains("environment variable"));

        // A `$NAME` that is not set means no key: the request goes out without
        // one, and the endpoint's response is what says so.
        let missing = parse(&one("openai", "api_key = \"$PI_NOT_SET_ANYWHERE\"\n")).unwrap();
        assert!(missing.key().is_none());
    }

    #[test]
    fn an_empty_config_is_a_valid_one() {
        assert!(parse("").unwrap().models.is_empty());
    }


    #[test]
    fn a_key_that_is_not_a_setting_is_refused_rather_than_ignored() {
        // A silently dropped key looks like a setting that does not work.
        let e = parse("modle = \"x\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("modle"), "{e}");
    }

    #[test]
    fn a_repeated_model_is_the_format_s_problem_not_ours() {
        // TOML rejects a duplicate key itself, which is one check we do not have
        // to write and cannot forget.
        let dup = "[provider.p]\nbase_url = \"http://x/v1\"\nformat = \"openai\"\n";
        assert!(parse(&format!("{dup}{dup}")).is_err());
    }

    #[test]
    fn a_project_may_pick_a_model_and_turn_the_dials() {
        let p = parse_project("model = \"flash\"\neffort = \"low\"\n").unwrap();
        let c = Config::default();
        assert_eq!(c.model(&p, None, None).unwrap().0, "flash");
        assert!(matches!(c.settle(&p, Flags::default()).effort, EffortArg::Low));
    }

    #[test]
    fn a_config_still_capping_turns_is_told_the_cap_is_gone() {
        // `deny_unknown_fields` would refuse it either way; what is asserted
        // here is that the message says what happened to the key.
        for body in [
            "max_turns = 100\n",
            "model = \"flash\"\nmax_turns = 100\n",
        ] {
            for e in [
                parse(body).unwrap_err().to_string(),
                parse_project(body).unwrap_err().to_string(),
            ] {
                assert!(e.contains("`max_turns` is gone"), "{e}");
            }
        }
    }

    #[test]
    fn a_project_cannot_name_a_server_of_its_own() {
        // The whole point: a repository arrives by git clone, and this is the
        // line between "configure the run" and "redirect it".
        for body in [
            "[provider.evil]\nbase_url = \"http://attacker/v1\"\nformat = \"openai\"\n",
            "[defaults]\nsystem = \"/etc/shadow\"\n",
            "[defaults]\ntier = \"exec\"\n",
        ] {
            assert!(parse_project(body).is_err(), "accepted: {body}");
        }
    }

    #[test]
    fn a_project_lowers_the_tier_and_cannot_raise_it() {
        let c: Config = parse("tier = \"write\"\n").unwrap();
        let down = parse_project("max_tier = \"read\"\n").unwrap();
        assert_eq!(c.settle(&down, Flags::default()).tier, TierArg::Read);

        let up = parse_project("max_tier = \"exec\"\n").unwrap();
        assert_eq!(c.settle(&up, Flags::default()).tier, TierArg::Write);
    }

    #[test]
    fn a_flag_outranks_both_files_but_still_meets_the_ceiling() {
        let c = Config::default();
        let p = parse_project("effort = \"low\"\nmax_tier = \"read\"\n").unwrap();
        let flags = Flags {
            effort: Some(EffortArg::High),
            tier: Some(TierArg::Exec),
        };
        let s = c.settle(&p, flags);
        assert!(matches!(s.effort, EffortArg::High));
        // Not even --tier exec gets past a checkout that declared itself
        // read-only; passing --tier is not reading the repository's file.
        assert_eq!(s.tier, TierArg::Read);
    }

    #[test]
    fn the_resumed_model_outranks_a_project_that_wants_another() {
        // Resuming means resuming, project default or not. Moving the session
        // is `/model`'s job, and it says so when it happens.
        let c = parse(SAMPLE).unwrap();
        let p = parse_project("model = \"other\"\n").unwrap();
        assert_eq!(c.model(&p, None, Some("resumed")).unwrap().0, "resumed");
        assert_eq!(
            c.model(&p, Some("flag"), Some("resumed")).unwrap().0,
            "flag"
        );
        assert_eq!(
            c.model(&p, None, None),
            Some(("other".into(), Origin::Project))
        );
    }

    #[test]
    fn the_home_file_is_never_read_as_a_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::write(home.join(".pi.toml"), "").unwrap();
        let deep = home.join("notes/today");
        std::fs::create_dir_all(&deep).unwrap();
        // Walking up from a directory under $HOME must stop at $HOME, or every
        // stray folder inherits the global file as its project config.
        assert_eq!(project_path(&deep, Some(home)), None);
    }

    #[test]
    fn the_search_stops_at_the_repository_root() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path();
        std::fs::write(outside.join(".pi.toml"), "").unwrap();
        let repo = outside.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("packages/web");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_path(&deep, None), None, "leaked past the repo root");

        // One inside the repo is found from any depth below it.
        let inside = repo.join(".pi.toml");
        std::fs::write(&inside, "").unwrap();
        assert_eq!(project_path(&deep, None), Some(inside));
    }

    #[test]
    fn one_model_and_no_default_needs_no_name() {
        // Writing the id twice is only a chance to write it differently, which
        // is exactly what happens.
        let body = one("openai", "[models.flash]\n");
        let c = parse(&body).unwrap();
        let got = c.model(&Project::default(), None, None);
        assert_eq!(got, Some(("flash".to_string(), Origin::OnlyModel)));
    }

    #[test]
    fn two_models_and_no_default_still_has_to_be_told() {
        // There is no defensible pick among them, and alphabetical is not one.
        let two = one(
            "openai",
            "[models.a]\n[models.b]\n",
        );
        let c = parse(&two).unwrap();
        assert_eq!(c.model(&Project::default(), None, None), None);
    }

    #[test]
    fn an_empty_config_names_no_model_rather_than_one_we_invented() {
        // A hardcoded fallback is a claim about what exists, and it goes stale
        // the week a vendor ships something.
        let c = Config::default();
        assert_eq!(c.model(&Project::default(), None, None), None);
    }

    #[test]
    fn an_unknown_model_can_say_which_file_asked_for_it() {
        let c = parse("model = \"typo\"\n").unwrap();
        let (name, origin) = c.model(&Project::default(), None, None).unwrap();
        assert_eq!(name, "typo");
        assert!(origin.describe().contains("~/.pi/settings.toml"));
    }
    #[test]
    fn a_key_may_be_given_alone_or_in_a_list() {
        let c = parse(
            "[keys]\n\"app.clear-screen\" = \"ctrl+g\"\n\
             \"move.line.start\" = [\"home\", \"f5\"]\n",
        )
        .unwrap();
        let k = c.key_map().unwrap();
        let press = crate::keys::parse;
        assert_eq!(
            k.action(press("ctrl+g").unwrap(), false, false),
            Some(crate::keys::Action::AppClearScreen)
        );
        assert_eq!(
            k.action(press("f5").unwrap(), false, false),
            Some(crate::keys::Action::MoveLineStart)
        );
        // Replaced, so the default it displaced is gone.
        assert_eq!(k.action(press("ctrl+l").unwrap(), false, false), None);
    }

    #[test]
    fn a_theme_defaults_when_not_named() {
        let c = parse("").unwrap();
        assert_eq!(c.theme.muted.codes(), "2");
        assert_eq!(c.theme.code.codes(), "38;2;88;166;255");
        assert_eq!(c.theme.menu.selected.codes(), "7");
        assert_eq!(c.theme.prompt.icon, "›");
        assert_eq!(c.theme.input.codes(), "");
    }

    #[test]
    fn a_theme_reads_hex_colours_into_truecolour_sgr() {
        let c = parse("[theme]\ncode = \"#dd80ff\"\nprompt.color = \"#f80\"\n").unwrap();
        assert_eq!(c.theme.code.codes(), "38;2;221;128;255");
        assert_eq!(c.theme.prompt.color.codes(), "38;2;255;136;0");
    }

    #[test]
    fn an_input_reads_a_foreground_and_attributes() {
        let c = parse("[theme]\ninput = { color = \"#f80\", sgr = [\"bold\"] }\n").unwrap();
        assert_eq!(c.theme.input.codes(), "1;38;2;255;136;0");
    }

    #[test]
    fn a_theme_rejects_a_colour_that_is_neither_hex_nor_ansi() {
        let e = parse("[theme]\ncode = \"#ddxxff\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("theme.code"), "{e}");
        let e = parse("[theme]\nmuted = \"hotpink\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("theme.muted"), "{e}");
        let e = parse("[theme]\ncode = { color = \"48;5;208\", sgr = [] }\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("theme.code.color"), "{e}");
        let e = parse("[theme]\ncode = { color = \"999\", sgr = [] }\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("theme.code.color"), "{e}");
    }
    #[test]
    fn a_theme_combines_attributes_with_a_colour() {
        let c =
            parse("[theme]\ncode = { color = \"#f80\", sgr = [\"bold\", \"italic\", \"8\"] }\n")
                .unwrap();
        assert_eq!(c.theme.code.codes(), "1;3;8;38;2;255;136;0");
    }

    #[test]
    fn a_broken_binding_is_refused_at_load() {
        // Not at the keystroke: a key that silently does nothing is the worst
        // way to find out a config is wrong.
        let e = parse("[keys]\n\"move.line.start\" = \"ctrl+nope\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("move.line.start"), "{e}");
    }
}
