//! Startup defaults and locally-defined models.
//!
//! TOML rather than JSON: most of what a model entry holds is a measurement,
//! and a measurement without its provenance rots. `max_output_tokens` needs
//! the comment saying which endpoint that was tried against, and JSON has
//! nowhere to put it.
//!
//! Providers own the connection, models own themselves: the endpoint is
//! written once and every model under it hangs off that one entry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use llm::model::{CacheControl, Format, ModelSpec, Pricing};
use serde::{Deserialize, Serialize};

use crate::args::{EffortArg, FormatArg, TierArg};

/// A config file: the user's `~/.pi/settings.toml`, or a repository's
/// `.pi.toml` laid over it key by key.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The endpoint this machine talks to: pi talks to one at a time, so this
    /// is one field, not a map keyed by provider.
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

    /// The model to run, as the endpoint names it: `deepseek-v4-flash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Who writes the summary when history is compacted. Defaults to `model`.
    ///
    /// Named for the job, not a tier, so a later task isn't stuck arguing
    /// whether it counts as "lite".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarize_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortArg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<TierArg>,

    /// Absolute directories every tool above the read tier may reach beyond
    /// the workspace root — `bash` may work in one, not only `write` and
    /// `edit`.
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
    /// The key table `keys` asks for, defaults included: read with the file,
    /// so a binding that cannot stand refuses the file it is in.
    #[serde(skip)]
    pub key_table: std::sync::Arc<crate::keys::Keys>,
    /// The SGR codes behind every colour the terminal uses.
    #[serde(default)]
    pub theme: crate::theme::Theme,
    /// Which parts the status line under the answer shows, running or done.
    #[serde(default)]
    pub status: crate::status::Parts,
    /// Vim keys: on unless a file turns them off.
    #[serde(default)]
    pub vim: Vim,
    /// MCP servers by name, each one's tools offered as `<name>__<tool>`.
    /// Your own file only: a checkout's would start programs on entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServer>,
    /// Commands run on what a run does: around its tool calls, or as it
    /// says each event. A checkout's are named when pi starts, like `[mcp]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hooks: Vec<Hook>,
    /// How many rounds a `/loop` may run before it stops on its own. Defaults
    /// to 10 — the fingerprint and thin-round brakes catch a converging loop,
    /// and this is the floor for the shape they cannot (a round that keeps
    /// changing files forever). Unset reads as 10; the user's stop is always the brake.
    #[serde(
        default = "default_loop_max_rounds",
        skip_serializing_if = "Option::is_none"
    )]
    pub loop_max_rounds: Option<usize>,

    /// How long a subagent may run silent, in seconds, before it is
    /// read as wedged and stopped — the one brake a subagent has of its own,
    /// since nothing bounds one by turns. Clamped up to 1 s: a zero would stop
    /// every subagent the moment it started. Unset reads as 1800.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_deadline: Option<u64>,

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

    /// Days a session, and a pasted image, is kept after it was last worked
    /// on. Unset is 90; 0 keeps everything. Your own file only: a checkout's
    /// would delete the sessions of every other project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_days: Option<u32>,
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
    /// How long the first of a pair of bare characters waits for the second —
    /// `j k` into Normal, `d d`, `g g`. Short, since the same characters are
    /// typed as text; the pairs themselves are `[keys]` bindings.
    #[serde(default = "default_pair_ms")]
    pub pair_timeout_ms: u64,
    /// The cursor in Insert.
    #[serde(default = "default_insert_cursor")]
    pub insert_cursor: Cursor,
    /// The cursor in Normal. Unthemed, it is the only mark of the mode, so
    /// keep it unlike Insert's.
    #[serde(default = "default_normal_cursor")]
    pub normal_cursor: Cursor,
}

/// A cursor shape. `default` is the terminal's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Cursor {
    Block,
    Bar,
    Underline,
    Default,
}

/// A command run on events, handed each as JSON on stdin.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub on: Vec<HookOn>,
    /// The tools a `tool.*` event must be about; empty is every tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Run by `sh -c` in the workspace.
    pub command: String,
}

/// What a hook runs on. The two around a call are gates: before one, exit 2
/// refuses it; after one, what the hook prints is added to the result.
/// `run.start` is waited on too: what it prints is a note to the model; so is
/// `run.stop`, where exit 2 keeps the run going. The rest only watch, and
/// nothing they do reaches the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum HookOn {
    #[serde(rename = "tool.before")]
    ToolBefore,
    #[serde(rename = "tool.after")]
    ToolAfter,
    #[serde(rename = "run.start")]
    RunStart,
    #[serde(rename = "tool.denied")]
    ToolDenied,
    #[serde(rename = "turn.start")]
    TurnStart,
    #[serde(rename = "turn.end")]
    TurnEnd,
    #[serde(rename = "compacted")]
    Compacted,
    #[serde(rename = "retrying")]
    Retrying,
    #[serde(rename = "warning")]
    Warning,
    #[serde(rename = "run.stop")]
    RunStop,
    #[serde(rename = "run.done")]
    RunDone,
    #[serde(rename = "run.failed")]
    RunFailed,
}

/// One MCP server: a command run over stdio, or a URL spoken to over HTTP.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Added to the command's environment; a leading `$` reads pi's own.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Sent with every request; a leading `$` reads the environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

impl McpServer {
    /// The environment as the command gets it, `$NAME` values read; one whose
    /// variable is unset is left out rather than sent empty.
    pub fn env(&self) -> Vec<(String, String)> {
        expanded(&self.env)
    }

    /// The headers as sent, read the same way.
    pub fn headers(&self) -> Vec<(String, String)> {
        expanded(&self.headers)
    }

    fn check(&self, name: &str) -> Result<()> {
        if !(1..=32).contains(&name.len()) || !name.chars().all(tool::name_char) {
            bail!("mcp.{name}: a server name is a-z, A-Z, 0-9, - and _, up to 32");
        }
        match (&self.command, &self.url) {
            (Some(_), None) if self.headers.is_empty() => Ok(()),
            (None, Some(_)) if self.args.is_empty() && self.env.is_empty() => Ok(()),
            (Some(_), Some(_)) | (None, None) => {
                bail!("mcp.{name}: give one of `command` (stdio) or `url` (HTTP)")
            }
            (Some(_), None) => bail!("mcp.{name}: `headers` go with `url`, not `command`"),
            (None, Some(_)) => bail!("mcp.{name}: `args` and `env` go with `command`, not `url`"),
        }
    }
}

fn expanded(map: &BTreeMap<String, String>) -> Vec<(String, String)> {
    map.iter()
        .filter_map(|(k, v)| Some((k.clone(), expand_key(v)?)))
        .collect()
}

fn default_enabled() -> bool {
    true
}

fn default_pair_ms() -> u64 {
    250
}

fn default_insert_cursor() -> Cursor {
    Cursor::Bar
}

fn default_normal_cursor() -> Cursor {
    Cursor::Block
}
fn default_loop_max_rounds() -> Option<usize> {
    Some(DEFAULT_LOOP_MAX_ROUNDS)
}

const DEFAULT_KEEP_DAYS: u32 = 90;

// The default `loop_max_rounds` — see the field doc.
const DEFAULT_LOOP_MAX_ROUNDS: usize = 10;

impl Default for Vim {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            pair_timeout_ms: default_pair_ms(),
            insert_cursor: default_insert_cursor(),
            normal_cursor: default_normal_cursor(),
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
    /// Where runs may write besides the workspace: `write_roots`, and pi's
    /// own home, so a tool or a skill can be written where pi reads it.
    pub fn writable(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = self.write_roots.iter().map(PathBuf::from).collect();
        out.extend(crate::dir().filter(|home| home.is_absolute()));
        out
    }

    fn key_map(&self) -> Result<crate::keys::Keys> {
        let overrides = self
            .keys
            .iter()
            .map(|(id, b)| (id.clone(), b.clone().into_vec()))
            .collect();
        crate::keys::Keys::resolve(&overrides)
    }

    /// The ceiling an unset `loop_max_rounds` reads as — see the field. An
    /// `Option` here mirrors the field, so `None` and the config staying
    /// silent mean the same thing to callers.
    pub fn loop_cap(&self) -> Option<usize> {
        self.loop_max_rounds.or(Some(DEFAULT_LOOP_MAX_ROUNDS))
    }

    /// How long an untouched session or pasted image is kept; `None` keeps
    /// them all.
    pub fn keep(&self) -> Option<std::time::Duration> {
        match self.keep_days.unwrap_or(DEFAULT_KEEP_DAYS) {
            0 => None,
            days => Some(std::time::Duration::from_secs(u64::from(days) * 86_400)),
        }
    }
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
    /// This model's own endpoint, over the file's `base_url`. A model that
    /// names one does not inherit the file's `api_key`: a key is the host's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// This model's wire, over the file's `format`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<FormatArg>,
    /// This model's credential; a leading `$` reads the environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default = "default_context")]
    pub context_window: u32,
    #[serde(default = "default_output")]
    pub max_output_tokens: u32,
    // Read and dropped: `effort` now says thinking on every wire, and a file
    // written before that must still load.
    #[serde(default, skip_serializing)]
    thinking: Option<toml::Value>,
    #[serde(default, skip_serializing)]
    replay_thinking: Option<toml::Value>,
    #[serde(default)]
    pub vision: bool,
    /// `unsupported` for a model that cannot think; no level belongs here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<EffortArg>,
    /// Anthropic 4.6+ and the OpenAI reasoning models reject every value.
    #[serde(default = "yes")]
    pub accepts_temperature: bool,
    /// Fable and Mythos reject a forced `tool_choice`.
    #[serde(default = "yes")]
    pub can_force_tool: bool,
    #[serde(default)]
    pub pricing: Pricing,
}

// Written out rather than derived: `derive(Default)` would zero the
// window, a budget of nothing rather than a stated guess.
impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            base_url: None,
            format: None,
            api_key: None,
            context_window: default_context(),
            max_output_tokens: default_output(),
            thinking: None,
            replay_thinking: None,
            vision: false,
            effort: None,
            accepts_temperature: true,
            can_force_tool: true,
            pricing: Pricing::default(),
        }
    }
}

impl Config {
    // The endpoint's shape, refused rather than guessed: naming the wrong one
    // is a 400 on the first turn, and neither is a safer bet than the other.
    fn format(&self, model: &ModelEntry) -> Result<Format> {
        let named = model
            .format
            .or(self.format)
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
        let format = self.format(model)?;
        // The file's `cache_control` speaks for the file's own wire; a model
        // on another wire of its own does not inherit it.
        let own_wire = model.format.is_some_and(|f| Some(f) != self.format);
        if !own_wire
            && !matches!(format, Format::Anthropic { .. })
            && self.cache_control != CacheControl::Off
        {
            bail!(
                "cache_control is an Anthropic field; the openai format caches by \
                 default and naming it here can only turn that off"
            );
        }
        let base_url = model
            .base_url
            .as_deref()
            .map(|url| expand_base_url(url.trim()))
            .or_else(|| self.base_url.clone())
            .context("`base_url` is required to reach a model")?;
        Ok(ModelSpec {
            model: name.to_string(),
            base_url,
            format,
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            vision: model.vision,
            accepts_effort: model.effort != Some(EffortArg::Unsupported),
            accepts_temperature: model.accepts_temperature,
            can_force_tool: model.can_force_tool,
            pricing: model.pricing,
        })
    }

    // A `$NAME` that names nothing is a typo now and a missing key much later,
    // pointing at the endpoint rather than at the file.
    fn check_key(&self) -> Result<()> {
        let keys = std::iter::once(self.api_key.as_deref())
            .chain(self.models.values().map(|m| m.api_key.as_deref()));
        for name in keys.flatten().filter_map(|k| k.strip_prefix('$')) {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!(
                    "api_key `${name}` does not name an environment variable. \
                     A literal key beginning with `$` cannot be written here — put it in a variable."
                );
            }
        }
        Ok(())
    }
}

/// Who asked for the model.
///
/// Carried alongside the name so that an unknown one can say where it came
/// from: a bare `pi` that fails on a name the user never typed is a mystery
/// with three files to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOrigin {
    Flag,
    Command,
    Resumed,
    Project,
    Global,
    OnlyModel,
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
    /// The spec for a model name. Every name resolves: an entry only ever
    /// supplies facts the defaults get wrong.
    ///
    /// Probing the endpoint for its catalog would cost a round trip and still
    /// not answer the one thing that matters — how wide the window is.
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

    /// The key as written for `model`: its own, or the file's while it is
    /// on the file's endpoint. A model sent elsewhere never carries the
    /// file's key there.
    pub fn written_key(&self, model: &str) -> Option<&str> {
        match self.models.get(model) {
            Some(m) if m.api_key.is_some() => m.api_key.as_deref(),
            Some(m) if m.base_url.is_some() => None,
            _ => self.api_key.as_deref(),
        }
    }

    /// [`Config::written_key`], `$NAME` read: the credential to send, or None
    /// when the endpoint wants none. The shape is checked at load.
    pub fn key_for(&self, model: &str) -> Option<String> {
        self.written_key(model).and_then(expand_key)
    }

    fn apply_env(&mut self) {
        self.apply_env_with(|k| std::env::var(k).ok());
    }

    fn apply_env_with<F>(&mut self, mut lookup: F)
    where
        F: FnMut(&str) -> Option<String>,
    {
        let Some((var, url)) = endpoint_var(&mut lookup) else {
            return;
        };
        let (format, cache) = match var {
            ANTHROPIC_BASE_URL => (FormatArg::Anthropic, None),
            _ => (FormatArg::Openai, Some(CacheControl::Off)),
        };

        self.base_url = Some(expand_base_url(url.trim()));
        self.format = Some(format);
        if let Some(c) = cache {
            self.cache_control = c;
        }
    }

    /// Flag > the files > the built-in default.
    pub fn settle(&self, flags: Flags) -> Settled {
        Settled {
            // What a model told nothing does anyway, so leaving it unset
            // changes nothing; `off` is the choice to stop it.
            effort: flags.effort.or(self.effort).unwrap_or(EffortArg::High),
            tier: flags.tier.or(self.tier).unwrap_or(TierArg::Exec),
        }
    }

    /// Resolves in order: `flag`, `prior`, `self.model`, then the only model
    /// if there is exactly one. `from_project`: whether the project file set it.
    pub fn model(
        &self,
        flag: Option<&str>,
        prior: Option<&str>,
        from_project: bool,
    ) -> Option<(String, ModelOrigin)> {
        if let Some(m) = flag {
            return Some((m.to_string(), ModelOrigin::Flag));
        }
        if let Some(m) = prior {
            return Some((m.to_string(), ModelOrigin::Resumed));
        }
        if let Some(m) = &self.model {
            let origin = match from_project {
                true => ModelOrigin::Project,
                false => ModelOrigin::Global,
            };
            return Some((m.clone(), origin));
        }
        if let [only] = self.names().as_slice() {
            return Some((only.clone(), ModelOrigin::OnlyModel));
        }
        None
    }
}

const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";

fn endpoint_var<F>(lookup: &mut F) -> Option<(&'static str, String)>
where
    F: FnMut(&str) -> Option<String>,
{
    [ANTHROPIC_BASE_URL, OPENAI_BASE_URL]
        .into_iter()
        .find_map(|var| Some((var, lookup(var).filter(|s| !s.trim().is_empty())?)))
}

/// The environment variable `base_url` is taken from, if any.
pub fn endpoint_env() -> Option<&'static str> {
    endpoint_var(&mut |k| std::env::var(k).ok()).map(|(var, _)| var)
}

/// `$NAME` reads that environment variable; anything else is the key itself.
/// The one spelling of the credential contract, for every key in the file.
fn expand_key(raw: &str) -> Option<String> {
    match raw.strip_prefix('$') {
        None => Some(raw.to_string()),
        Some(name) => std::env::var(name).ok(),
    }
}

/// `:7897` and `:7897/v1`, as typed at an input or written in a file: a local
/// port, spelled short.
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
    super::dir().map(|root| root.join("settings.toml"))
}

/// The file that, where it exists, replaces the built-in system prompt.
pub fn system_file() -> Option<PathBuf> {
    super::dir().map(|root| root.join("SYSTEM.md"))
}

/// The file that, where it exists, replaces the built-in prompt `name`.
pub fn prompt_file(name: &str) -> Option<PathBuf> {
    super::dir().map(|root| root.join("prompts").join(format!("{name}.md")))
}

/// The nearest project file at or above `start`, stopping at the repo root.
/// `home` is never searched: a `.pi.toml` there applies outside any repo.
fn project_path(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    nearest(start, home, |dir| {
        Some(dir.join(".pi.toml")).filter(|p| p.is_file())
    })
}

/// A checkout's `.pi/<name>` directory: the nearest one up to the repository
/// root, else where one belongs, at that root; outside a repository, the
/// workspace's own. `None` in `$HOME` itself, whose `.pi` is pi's home.
pub fn project_dir(workspace: &Path, name: &str) -> Option<PathBuf> {
    project_dir_in(workspace, home().as_deref(), name)
}

fn project_dir_in(workspace: &Path, home: Option<&Path>, name: &str) -> Option<PathBuf> {
    let at = |dir: &Path| dir.join(".pi").join(name);
    // With no repository to bound the walk, a parent such as `/tmp` is
    // anyone's to write, and what is found there gets run.
    let Some(repo) = workspace
        .ancestors()
        .take_while(|dir| home != Some(*dir))
        .find(|dir| dir.join(".git").exists())
    else {
        return (home != Some(workspace)).then(|| at(workspace));
    };
    Some(
        nearest(workspace, home, |dir| Some(at(dir)).filter(|p| p.is_dir()))
            .unwrap_or_else(|| at(repo)),
    )
}

// `$HOME` itself is never a project: its `.pi` is pi's home.
fn nearest(
    start: &Path,
    home: Option<&Path>,
    found: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    for dir in start.ancestors() {
        if home == Some(dir) {
            return None;
        }
        if let Some(path) = found(dir) {
            return Some(path);
        }
        // Above the repository root is not this project any more.
        if dir.join(".git").exists() {
            return None;
        }
    }
    None
}

/// Where settings for `workspace` are written: the project file in force, else a
/// new one at the repository root, or in the workspace outside a repository.
/// `None` in `$HOME` itself, whose `.pi.toml` is never read.
pub fn project_target(workspace: &Path) -> Option<PathBuf> {
    let home = home();
    if let Some(found) = project_path(workspace, home.as_deref()) {
        return Some(found);
    }
    let dir = workspace
        .ancestors()
        .take_while(|dir| home.as_deref() != Some(*dir))
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(workspace);
    (home.as_deref() != Some(dir)).then(|| dir.join(".pi.toml"))
}

/// A config file's tree, checked as a config on its own so an error names the
/// file it is in. A missing file is `None` unless it was asked for by name.
fn read_tree(path: &Path, required: bool) -> Result<Option<toml::Value>> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let tree: toml::Value = toml::from_str(&body).with_context(|| format!("{}", path.display()))?;
    Config::from_tree(tree.clone()).with_context(|| format!("{}", path.display()))?;
    Ok(Some(tree))
}

/// The user's file: `explicit` when given, which must then exist, else
/// `settings.toml` under the pi root. Absent, the tree is the empty table.
pub fn load_tree(explicit: Option<&str>) -> Result<toml::Value> {
    let found = match explicit {
        Some(p) => read_tree(Path::new(p), true)?,
        None => match global_path() {
            Some(p) => read_tree(&p, false)?,
            None => None,
        },
    };
    Ok(found.unwrap_or_else(|| toml::Value::Table(Default::default())))
}

/// The nearest `.pi.toml` at or above `workspace`, if there is one.
pub fn project_file(workspace: &Path) -> Option<PathBuf> {
    project_path(workspace, home().as_deref())
}

/// Keys a project file may set that reach past its checkout: they start
/// programs, delete sessions, widen writes, or choose where a key is sent.
const REACHING: &[&str] = &[
    "mcp",
    "hooks",
    "keep_days",
    "write_roots",
    "base_url",
    "api_key",
];

/// The keys of a project `tree` that reach past its checkout, as spelled
/// there, each with its value as shown when it loads.
pub fn reaching(tree: &toml::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = REACHING
        .iter()
        .filter_map(|key| Some((key.to_string(), shown(key, tree.get(*key)?))))
        .collect();
    let models = tree.get("models").and_then(toml::Value::as_table);
    for (name, entry) in models.into_iter().flatten() {
        for field in ["base_url", "api_key"] {
            if let Some(value) = entry.get(field) {
                out.push((format!("models.{name}.{field}"), shown(field, value)));
            }
        }
    }
    out
}

// One line for a reaching key's value; a literal key never reaches the screen.
fn shown(key: &str, value: &toml::Value) -> String {
    use toml::Value as V;
    match value {
        V::String(s) if key == "api_key" && !s.starts_with('$') => "(hidden)".into(),
        V::String(s) => s.clone(),
        V::Table(t) => t.keys().cloned().collect::<Vec<_>>().join(", "),
        V::Array(items) => items
            .iter()
            .map(|item| match item {
                V::String(s) => s.clone(),
                other => other
                    .get("command")
                    .and_then(V::as_str)
                    .map_or_else(|| other.to_string(), str::to_string),
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// The nearest `.pi.toml` at or above `workspace`, and its tree.
pub fn load_project(workspace: &Path) -> Result<Option<(PathBuf, toml::Value)>> {
    let Some(path) = project_file(workspace) else {
        return Ok(None);
    };
    Ok(read_tree(&path, true)?.map(|tree| (path, tree)))
}

impl Config {
    /// A tree as a config, with everything a file can get wrong refused now:
    /// a typo in a model you are not running today is still a typo.
    pub fn from_tree(tree: toml::Value) -> Result<Config> {
        let mut config: Config = serde_path_to_error::deserialize(tree)?;
        config.base_url = config.base_url.map(|url| expand_base_url(url.trim()));
        config.check_key()?;
        for (name, server) in &config.mcp {
            server.check(name)?;
        }
        if config.effort == Some(EffortArg::Unsupported) {
            bail!("effort = \"unsupported\" describes one model; put it in that model's entry");
        }
        for (model, entry) in &config.models {
            if entry.effort.is_some_and(|e| e != EffortArg::Unsupported) {
                bail!(
                    "models.{model}.effort: only \"unsupported\" belongs here; \
                     a level is set by the top-level `effort` or `/effort`"
                );
            }
            let reachable = config.base_url.is_some() || entry.base_url.is_some();
            if reachable && (config.format.is_some() || entry.format.is_some()) {
                config.spec(model, entry)?;
            }
        }
        config.key_table = std::sync::Arc::new(config.key_map()?);
        Ok(config)
    }

    /// The config a run uses: the tree, the environment's endpoint over it,
    /// and every model checked against the endpoint it ends up with. Startup,
    /// a reload and an edit all come through here, so none of them
    /// accepts what another would refuse.
    pub fn in_force(tree: toml::Value) -> Result<Config> {
        let mut config = Self::from_tree(tree)?;
        config.apply_env();
        for (model, entry) in &config.models {
            config.spec(model, entry)?;
        }
        Ok(config)
    }
}

/// A key written into a file others can read is worth one line of warning.
///
/// Returned rather than printed: the same check runs mid-session, where the
/// terminal may be in raw mode and a stray `eprintln!` lands wherever the cursor
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

    // The example is documentation nothing else reads, so this is what keeps
    // a renamed or removed key from rotting in it.
    // A server is one of two shapes; anything that mixes them, or names
    // itself what a tool name cannot carry, is refused at load.
    #[test]
    fn an_mcp_server_is_a_command_or_a_url() {
        assert!(parse("[mcp.a]\ncommand = \"x\"\n").is_ok());
        assert!(parse("[mcp.a]\nurl = \"http://h/mcp\"\n").is_ok());
        for bad in [
            "[mcp.a]\n",
            "[mcp.a]\ncommand = \"x\"\nurl = \"http://h\"\n",
            "[mcp.a]\ncommand = \"x\"\nheaders = { A = \"b\" }\n",
            "[mcp.a]\nurl = \"http://h\"\nargs = [\"y\"]\n",
            "[mcp.\"a b\"]\ncommand = \"x\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    // A checkout may set what it likes; what reaches past it is said aloud.
    #[test]
    fn a_project_file_may_set_anything_and_what_reaches_past_it_is_named() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let body = "keep_days = 1\ntier = \"read\"\napi_key = \"sk-literal\"\n\
                    [mcp.docs]\ncommand = \"x\"\n\
                    [models.home]\nbase_url = \"https://elsewhere\"\ncontext_window = 1000\n";
        std::fs::write(dir.path().join(".pi.toml"), body).unwrap();
        let (_, tree) = load_project(dir.path()).unwrap().unwrap();
        let said = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            reaching(&tree),
            [
                said("mcp", "docs"),
                said("keep_days", "1"),
                said("api_key", "(hidden)"),
                said("models.home.base_url", "https://elsewhere"),
            ],
            "tier and the numbers stay within the checkout"
        );
    }

    #[test]
    fn the_shipped_example_is_a_valid_config() {
        let example = include_str!("../../../examples/pi.toml");
        let config = Config::from_tree(toml::from_str(example).unwrap());
        assert!(config.is_ok(), "{:?}", config.err());
    }

    const SAMPLE: &str = r#"
base_url = "http://localhost:7896/v1"
format = "openai"
api_key = "x"
model = "flash"
effort = "medium"

[models.flash]
context_window = 1_000_000
max_output_tokens = 384_000

[models.flash.pricing]
input_per_mtok = 0.14
# An integer where a float is wanted, which TOML distinguishes and serde does not.
output_per_mtok = 0
"#;

    #[test]
    fn the_environment_endpoint_beats_the_files() {
        let mut c = parse("base_url = \"http://file\"\nformat = \"openai\"\n").unwrap();
        c.apply_env_with(|k| match k {
            "ANTHROPIC_BASE_URL" => Some("https://anthropic.example.com".into()),
            _ => None,
        });
        assert_eq!(c.base_url.as_deref(), Some("https://anthropic.example.com"));
        assert_eq!(c.format, Some(FormatArg::Anthropic));
    }

    fn parse(body: &str) -> Result<Config> {
        Config::from_tree(toml::from_str(body)?)
    }

    // A model on its own host must never carry the file's key there.
    #[test]
    fn a_model_on_its_own_endpoint_keeps_its_own_key() {
        let c = parse(
            r#"
base_url = "https://api.anthropic.com"
format = "anthropic"
cache_control = "standard"
api_key = "file-key"

[models.home]

[models.gpt]
base_url = "https://api.openai.com/v1"
format = "chat"

[models.local]
base_url = ":8080/v1"
format = "chat"
api_key = "local-key"
"#,
        )
        .unwrap();
        assert_eq!(c.key_for("home").as_deref(), Some("file-key"));
        assert_eq!(c.key_for("unlisted").as_deref(), Some("file-key"));
        assert_eq!(c.key_for("gpt"), None, "the file's key stays with its host");
        assert_eq!(c.key_for("local").as_deref(), Some("local-key"));

        let gpt = c.find("gpt").unwrap();
        assert_eq!(gpt.base_url, "https://api.openai.com/v1");
        assert!(
            matches!(gpt.format, Format::Chat),
            "cache_control is not inherited"
        );
        assert_eq!(
            c.find("local").unwrap().base_url,
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            c.find("home").unwrap().base_url,
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn a_port_in_the_file_is_a_local_url() {
        let c = parse("base_url = \":2/v1\"\n").unwrap();
        assert_eq!(c.base_url.as_deref(), Some("http://127.0.0.1:2/v1"));
    }

    // A file is checked as a whole config: a section that no longer exists
    // is refused rather than read as nothing.
    #[test]
    fn a_retired_section_is_refused() {
        assert!(parse("[defaults]\nmodel = \"flash\"\n").is_err());
    }

    #[test]
    fn a_flag_outranks_the_files() {
        let c = parse("effort = \"low\"\ntier = \"read\"\n").unwrap();
        let flags = Flags {
            effort: Some(EffortArg::High),
            tier: Some(TierArg::Exec),
        };
        let s = c.settle(flags);
        assert!(matches!(s.effort, EffortArg::High));
        assert_eq!(s.tier, TierArg::Exec);
    }

    #[test]
    fn the_resumed_model_outranks_the_files() {
        // Resuming means resuming, project default or not. Moving the session
        // is `/model`'s job, and it says so when it happens.
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.model(None, Some("resumed"), true).unwrap().0, "resumed");
        assert_eq!(
            c.model(Some("flag"), Some("resumed"), true).unwrap().0,
            "flag"
        );
        assert_eq!(
            c.model(None, None, true),
            Some(("flash".into(), ModelOrigin::Project))
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
    fn a_project_dir_outside_a_repository_is_only_the_workspace_own() {
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path();
        std::fs::create_dir_all(shared.join(".pi/tools")).unwrap();
        let deep = shared.join("scratch/today");
        std::fs::create_dir_all(&deep).unwrap();
        let own = deep.join(".pi/tools");
        assert_eq!(
            project_dir_in(&deep, None, "tools"),
            Some(own),
            "climbed out"
        );
        assert_eq!(
            project_dir_in(shared, Some(shared), "tools"),
            None,
            "pi's home"
        );

        // Inside a repository: its root's until a nearer one exists, and
        // never one above it.
        std::fs::create_dir(shared.join("scratch/.git")).unwrap();
        let root = shared.join("scratch/.pi/tools");
        assert_eq!(project_dir_in(&deep, None, "tools"), Some(root));
        let near = deep.join(".pi/tools");
        std::fs::create_dir_all(&near).unwrap();
        assert_eq!(project_dir_in(&deep, None, "tools"), Some(near));
    }
}
