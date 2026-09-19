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
use brain::model::{CacheControl, Format, ModelSpec, Pricing, ReplayThinking, ThinkingControl};
use serde::{Deserialize, Serialize};

use crate::{EffortArg, FormatArg, TierArg};

/// The user's own file: `~/.pi/settings.toml`.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The endpoint this machine talks to. One, because pi talks to one at a
    /// time: a map of them made every model's name a `provider.model` pair,
    /// and that pair is what the config, the archive and the reasoning stamp
    /// each had to spell in their own way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<FormatArg>,
    /// `$NAME` reads that environment variable; anything else is the key
    /// itself. A key that genuinely begins with `$` cannot be written literally
    /// — put it in a variable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Anthropic only, and off unless someone measured it: an unknown top-level
    /// field is a 400 on some servers.
    #[serde(default)]
    pub cache_control: CacheControl,

    /// The judgment endpoint, when this machine has one. Present, the `judge`
    /// tool is in the set and the model can delegate snap judgments to it;
    /// absent, the tool never exists, so the model never sees a name it
    /// cannot call. Only this user-level file can set it: a judgment endpoint
    /// receives pieces of this machine's state, and a cloned `pi.toml` must
    /// not choose where those go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeSection>,

    /// The model to run, as the endpoint names it: `deepseek-v4-flash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Who writes the summary when history is compacted. Defaults to `model`.
    ///
    /// Named for the job, not for a tier: `lite_model` would be a category with
    /// one member and no test for membership, and every task added after would
    /// have to argue about whether it qualifies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarize_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortArg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<TierArg>,
    /// Path to a file replacing the built-in system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    /// Absolute directories every tool above the read tier may reach beyond
    /// the workspace root — `bash` may work in one, not only `write` and
    /// `edit`. Only this user-level file can set them: a cloned `pi.toml`
    /// must not be able to lower its own boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub write_roots: Vec<String>,

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
    /// Which parts the running and the finished status lines show.
    #[serde(default)]
    pub status: crate::status::Lines,
    /// Vim keys: on unless a file turns them off.
    #[serde(default)]
    pub vim: Vim,
    /// How many rounds a `/loop` may run before it stops on its own. Defaults
    /// to 10 — the fingerprint and thin-round brakes catch a converging loop,
    /// and this is the floor for the shape they cannot (a round that keeps
    /// changing files forever). Unset reads as 10; esc is always the brake.
    #[serde(
        default = "default_loop_max_turns",
        skip_serializing_if = "Option::is_none"
    )]
    pub loop_max_turns: Option<usize>,

    /// Turn ceiling for subagent tasks. Unset reads as 50.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,

    /// How long a subagent task may run silent, in seconds, before it is
    /// read as wedged and stopped. Turns and this stop different things —
    /// turns catch a loop that keeps failing, this catches a call that has
    /// stopped speaking — and either ending is a stop. Clamped up to 1 s: a
    /// zero would stop every task the moment it started. Unset reads as 1800.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_deadline: Option<u64>,

    /// How many times to retry a request the provider could not serve. Unset
    /// is `Retry::default()` — the number lives there, not here, so that an
    /// unset field and a missing config agree by construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<usize>,

    /// Seconds of silence before a stream counts as wedged. Unset is 300,
    /// generous because a reasoning model can think for minutes before its
    /// first token. Clamped up to 1: a zero would call every stream wedged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,
}

/// The modal keys, and the sequence that leaves Insert for Normal.
///
/// On by default, which is payable because the layer is additive: the Normal
/// layer binds bare characters only, and nothing bound before it existed is
/// one. `enabled = false` turns it off.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vim {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Two characters typed inside the window below. Empty turns the sequence
    /// off, and with it the only way into Normal — `esc` keeps all three of
    /// the jobs it already has rather than becoming a mode key.
    #[serde(default = "default_escape")]
    pub escape: String,
    /// How long the first half of a two-press sequence waits for its second —
    /// the escape pair in Insert, and the doubled keys in Normal.
    #[serde(default = "default_escape_ms")]
    pub escape_timeout_ms: u64,
}

impl Vim {
    /// The escape sequence as the exactly-two characters it must be, or `None`
    /// — an empty setting, or any other length, is no sequence at all.
    pub fn escape_pair(&self) -> Option<(char, char)> {
        let mut chars = self.escape.chars();
        match (chars.next(), chars.next(), chars.next()) {
            (Some(a), Some(b), None) => Some((a, b)),
            _ => None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_escape() -> String {
    "jk".to_string()
}

fn default_escape_ms() -> u64 {
    250
}
fn default_loop_max_turns() -> Option<usize> {
    Some(DEFAULT_LOOP_MAX_TURNS)
}

// The ceiling an unset `loop_max_turns` reads as: the floor for a loop that
// keeps changing files without ever repeating itself.
const DEFAULT_LOOP_MAX_TURNS: usize = 10;

impl Default for Vim {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            escape: default_escape(),
            escape_timeout_ms: default_escape_ms(),
        }
    }
}

/// One key or several — `"ctrl+g"` and `["ctrl+g", "f5"]` both mean the same
/// thing for an action with a single binding, and requiring the brackets for
/// the common case would be noise.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
    /// The ceiling an unset `loop_max_turns` reads as — see the field. An
    /// `Option` here mirrors the field, so `None` and the config staying
    /// silent mean the same thing to callers.
    pub fn loop_cap(&self) -> Option<usize> {
        self.loop_max_turns.or(Some(DEFAULT_LOOP_MAX_TURNS))
    }
}

/// The `[judge]` section: where snap judgments are delegated to.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeSection {
    /// The judgment endpoint's origin. The wire path is the tool's business,
    /// not the file's — the same split as the main endpoint, whose path lives
    /// in the transport too.
    pub base_url: String,
    /// `$NAME` reads that environment variable; anything else is the key
    /// itself. Unset, or naming an unset variable, sends the request without
    /// a key and lets the endpoint's response say what it needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// The model to ask, as the endpoint names it. Absent, the request omits
    /// the field and the endpoint serves its default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl JudgeSection {
    /// The origin to post judgments to, `:port` shorthand expanded.
    pub fn endpoint(&self) -> String {
        expand_base_url(self.base_url.trim())
    }

    /// The credential to send, or None when the endpoint wants none. Read at
    /// use rather than at load, for the same reason the main key is.
    pub fn key(&self) -> Option<String> {
        self.api_key.as_deref().and_then(expand_key)
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
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub model: Option<String>,
    pub effort: Option<EffortArg>,
    /// A ceiling, applied downward only: a checkout may declare itself
    /// read-only, never hand itself the shell.
    pub max_tier: Option<TierArg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
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
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    #[serde(default = "default_context")]
    pub context_window: u32,
    #[serde(default = "default_output")]
    pub max_output_tokens: u32,
    /// How the model takes a thinking instruction, absent if it takes none.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    // The endpoint's shape, refused rather than guessed: naming the wrong one
    // is a 400 on the first turn, and neither is a safer bet than the other.
    // The endpoint's shape, refused rather than guessed: naming the wrong one
    // is a 400 on the first turn, and neither is a safer bet than the other.
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

    // A `$NAME` that names nothing is a typo now and a missing key much later,
    // pointing at the endpoint rather than at the file.
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

    // A thinking control the format cannot carry is otherwise accepted, then
    // silently dropped when the request is built.
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
    pub max_turns: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub struct Settled {
    pub effort: EffortArg,
    pub tier: TierArg,
    pub max_turns: Option<usize>,
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
        self.api_key.as_deref().and_then(expand_key)
    }

    pub fn apply_env(&mut self) {
        self.apply_env_unclaimed(&BTreeMap::new());
    }

    pub fn apply_env_unclaimed(&mut self, claimed: &BTreeMap<String, toml::Value>) {
        self.apply_env_with(|k| std::env::var(k).ok(), claimed);
    }

    pub fn apply_env_with<F>(&mut self, mut lookup: F, claimed: &BTreeMap<String, toml::Value>)
    where
        F: FnMut(&str) -> Option<String>,
    {
        let (url, format, cache) =
            if let Some(url) = lookup("ANTHROPIC_BASE_URL").filter(|s| !s.trim().is_empty()) {
                (url, FormatArg::Anthropic, None)
            } else if let Some(url) = lookup("OPENAI_BASE_URL").filter(|s| !s.trim().is_empty()) {
                (url, FormatArg::Openai, Some(CacheControl::Off))
            } else {
                return;
            };

        if !claimed.contains_key("base_url") {
            self.base_url = Some(expand_base_url(url.trim()));
        }
        if !claimed.contains_key("format") {
            self.format = Some(format);
            if let Some(c) = cache {
                self.cache_control = c;
            }
        }
    }

    /// `/settings` > flag > project > this file > the built-in default.
    ///
    /// A key named in `claimed` was set by `/settings` this session, so it
    /// skips the flag and the project: the config tree already carries the
    /// claimed value. The tier keeps its ceiling — a project may only pull
    /// it down, and `/settings` does not open that back door.
    pub fn settle(
        &self,
        project: &Project,
        flags: Flags,
        claimed: &BTreeMap<String, toml::Value>,
    ) -> Settled {
        let effort = if claimed.contains_key("effort") {
            self.effort
        } else {
            flags.effort.or(project.effort).or(self.effort)
        }
        .unwrap_or(EffortArg::Off);
        let tier = if claimed.contains_key("tier") {
            self.tier
        } else {
            flags.tier.or(self.tier)
        }
        .unwrap_or(TierArg::Exec)
        .capped_by(project.max_tier.unwrap_or(TierArg::Exec));
        let max_turns = if claimed.contains_key("max_turns") {
            self.max_turns
        } else {
            flags.max_turns.or(project.max_turns).or(self.max_turns)
        };
        Settled {
            effort,
            tier,
            max_turns,
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

/// `$NAME` reads that environment variable; anything else is the key itself.
/// The one spelling of the credential contract, for every key in the file.
fn expand_key(raw: &str) -> Option<String> {
    match raw.strip_prefix('$') {
        None => Some(raw.to_string()),
        Some(name) => std::env::var(name).ok(),
    }
}

/// `:7897` and `:7897/v1` typed at an input. The file and the running spec
/// store the expanded URL; this is typing, not a dialect.
pub fn expand_base_url(raw: &str) -> String {
    let rest = match raw.strip_prefix(':') {
        Some(rest) => rest,
        None => return raw.to_string(),
    };
    let (port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err()
    {
        return raw.to_string();
    }
    format!("http://127.0.0.1:{port}{path}")
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
    let target = match explicit {
        Some(p) => Some((PathBuf::from(p), true)),
        None => global_path().map(|p| (p, false)),
    };
    let mut config = match target {
        Some((path, required)) => match std::fs::read_to_string(&path) {
            Ok(body) => parse(&body).with_context(|| format!("{}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        },
        None => Config::default(),
    };
    config.apply_env();
    for (model, entry) in &config.models {
        config.spec(model, entry)?;
        config.check_thinking(model, entry)?;
    }
    Ok(config)
}

/// The file's tree, for `/settings` to walk and `/reload` to re-read.
/// `None` when the config file is absent: the tree is then the empty table.
pub fn load_tree(explicit: Option<&str>) -> Result<toml::Value> {
    let (path, required) = match explicit {
        Some(p) => (PathBuf::from(p), true),
        None => match global_path() {
            Some(p) => (p, false),
            None => return Ok(toml::Value::Table(Default::default())),
        },
    };
    match std::fs::read_to_string(&path) {
        Ok(body) => Ok(toml::from_str(&body).with_context(|| format!("{}", path.display()))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
            Ok(toml::Value::Table(Default::default()))
        }
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
    let de = toml::de::Deserializer::parse(body)?;
    let config: Config = serde_path_to_error::deserialize(de)?;
    config.check_key()?;
    for (model, entry) in &config.models {
        // Rejected here rather than at use: a typo in a model you are not
        // running today is still a typo, and this is when it is cheap to see.
        if config.base_url.is_some() && config.format.is_some() {
            config.spec(model, entry)?;
        }
        config.check_thinking(model, entry)?;
    }
    config.key_map()?;
    Ok(config)
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
    toml::from_str(body).context(
        "a project .pi.toml may set only `model`, `effort`, `max_tier` and `max_turns` — \
         a checkout does not get to name a server, a key, or a system prompt",
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

/// Write one value at `path` in `settings.toml`, leaving every other byte —
/// comments, blank lines, the rest of the tree — untouched.
///
/// The panel edits one field at a time, so this never re-serializes the whole
/// file: a DOM round-trip would drop the comments that carry a measurement's
/// provenance.
pub fn write(path: &Path, dotted: &str, value: toml::Value) -> Result<()> {
    let body = std::fs::read_to_string(path)?;
    let mut doc = body
        .parse::<toml_edit::DocumentMut>()
        .context("the config file must stay valid TOML")?;
    let segments = crate::settings::segments(dotted)?;
    // Walk to the parent table, creating intermediate tables as needed.
    let mut table = doc.as_table_mut();
    let last = segments[segments.len() - 1].clone();
    for seg in &segments[..segments.len() - 1] {
        if !table.contains_key(seg) {
            table.insert(seg, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let item = table
            .get_mut(seg)
            .ok_or_else(|| anyhow::anyhow!("no table `{seg}`"))?;
        table = item
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("`{seg}` is not a table"))?;
    }
    table.insert(&last, toml_edit::Item::Value(to_edit_value(&value)));
    // Keys live here; `write_private` is the shared atomic write, pid-suffixed
    // temp and all, so two writers cannot clobber each other's temp file.
    tools::state::write_private(path, doc.to_string().as_bytes())?;
    Ok(())
}

// toml_edit's own value type has no From<toml::Value>, so build it by hand.
// Tables and arrays recurse; scalars map straight across.
fn to_edit_value(value: &toml::Value) -> toml_edit::Value {
    use toml_edit::Value;
    match value {
        toml::Value::String(s) => Value::from(s.as_str()),
        toml::Value::Integer(i) => Value::from(*i),
        toml::Value::Float(f) => Value::from(*f),
        toml::Value::Boolean(b) => Value::from(*b),
        toml::Value::Datetime(d) => Value::from(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(to_edit_value).collect()),
        toml::Value::Table(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in map {
                t.insert(k, to_edit_value(v));
            }
            Value::InlineTable(t)
        }
    }
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

    // A key the /settings panel claimed this session must not be overridden
    // by the environment; the unclaimed format still follows it.
    #[test]
    fn a_claimed_base_url_keeps_the_environment_out() {
        let mut c = Config::default();
        let mut claimed = BTreeMap::new();
        claimed.insert(
            "base_url".into(),
            toml::Value::String("http://claimed".into()),
        );
        c.apply_env_with(
            |k| match k {
                "ANTHROPIC_BASE_URL" => Some("https://anthropic.example.com".into()),
                _ => None,
            },
            &claimed,
        );
        assert_eq!(c.base_url, None);
        assert_eq!(c.format, Some(FormatArg::Anthropic));
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
        assert_eq!(
            c.settle(&down, Flags::default(), &BTreeMap::new()).tier,
            TierArg::Read
        );

        let up = parse_project("max_tier = \"exec\"\n").unwrap();
        assert_eq!(
            c.settle(&up, Flags::default(), &BTreeMap::new()).tier,
            TierArg::Write
        );
    }

    // `max_tier` is a ceiling, and two ceilings with no order between them
    // leave only what they share. A checkout that declared itself read-only
    // plus the web does not thereby hand a `--tier write` run the web.
    #[test]
    fn a_ceiling_beside_the_tier_rather_than_above_it_leaves_read() {
        let c: Config = parse("tier = \"write\"\n").unwrap();
        let net = parse_project("max_tier = \"net\"\n").unwrap();
        assert_eq!(
            c.settle(&net, Flags::default(), &BTreeMap::new()).tier,
            TierArg::Read
        );

        let c: Config = parse("tier = \"net\"\n").unwrap();
        assert_eq!(
            c.settle(&net, Flags::default(), &BTreeMap::new()).tier,
            TierArg::Net
        );
    }

    #[test]
    fn write_roots_parse_from_the_user_config_only() {
        // A cloned pi.toml cannot widen its own write boundary.
        assert!(parse_project("write_roots = [\"/etc\"]\n").is_err());
    }

    #[test]
    fn a_flag_outranks_both_files_but_still_meets_the_ceiling() {
        let c = Config::default();
        let p = parse_project("effort = \"low\"\nmax_tier = \"read\"\n").unwrap();
        let flags = Flags {
            effort: Some(EffortArg::High),
            tier: Some(TierArg::Exec),
            max_turns: None,
        };
        let s = c.settle(&p, flags, &BTreeMap::new());
        assert!(matches!(s.effort, EffortArg::High));
        // Not even --tier exec gets past a checkout that declared itself
        // read-only; passing --tier is not reading the repository's file.
        assert_eq!(s.tier, TierArg::Read);
    }

    #[test]
    fn a_claimed_value_skips_the_flag_and_the_project() {
        let c = parse("effort = \"low\"\ntier = \"write\"\n").unwrap();
        let p = parse_project("effort = \"medium\"\nmax_tier = \"exec\"\n").unwrap();
        let flags = Flags {
            effort: Some(EffortArg::High),
            tier: Some(TierArg::Exec),
            max_turns: None,
        };
        // The panel claims the key this session: the tree already
        // carries it, so the flag and the project must both stand down.
        let mut claimed = BTreeMap::new();
        claimed.insert("effort".into(), toml::Value::String("low".into()));
        claimed.insert("tier".into(), toml::Value::String("write".into()));
        let s = c.settle(&p, flags, &claimed);
        assert!(matches!(s.effort, EffortArg::Low));
        assert_eq!(s.tier, TierArg::Write);
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
}
