//! Startup defaults and locally-defined models.
//!
//! TOML rather than JSON: most of what a model entry holds is a measurement,
//! and a measurement without its provenance rots. `max_output_tokens` needs
//! the comment saying which endpoint that was tried against, and JSON has
//! nowhere to put it.
//!
//! Providers own the connection, models own themselves: the endpoint is
//! written once and every model under it hangs off that one entry.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use llm::model::{ModelSpec, Pricing};
use serde::{Deserialize, Serialize};

use crate::args::{EffortArg, TierArg};
use crate::provider::Provider;

/// The settings path relative to a project directory.
pub const PROJECT_FILE: &str = ".pi/settings.toml";

/// A config file: the user's `~/.pi/settings.toml`, or a repository's
/// `.pi/settings.toml` laid over it key by key.
#[derive(Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Named connections whose model lists are discovered at runtime.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider: BTreeMap<String, Provider>,
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

    /// Facts about a model the defaults get wrong, keyed by the model's own
    /// name. Every field has a default, so a model needs an entry only where
    /// it differs — and needs none at all to be usable.
    #[serde(default)]
    pub models: BTreeMap<String, ModelEntry>,
    /// Endpoint-supplied entries; never persisted as user configuration.
    #[serde(skip)]
    pub discovered: BTreeSet<String>,
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
    /// How many rounds a loop may run before it stops on its own. Defaults
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
    /// Run by `bash -c` in the workspace.
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

pub(crate) fn expanded(map: &BTreeMap<String, String>) -> Vec<(String, String)> {
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
    /// Where runs may write besides the workspace: pi's own home, so a tool
    /// or a skill can be written where pi reads it.
    pub fn writable(&self) -> Vec<PathBuf> {
        crate::dir()
            .filter(|home| home.is_absolute())
            .into_iter()
            .collect()
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// `None` when neither the files nor discovery stated it: the spec then
    /// carries a built-in guess, and saying which is a guess is `dial`'s job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
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

// Written out rather than derived: `derive(Default)` would leave both
// capabilities off, refusing temperature and forced tools on every model.
impl Default for ModelEntry {
    fn default() -> Self {
        Self {
            context_window: None,
            max_output_tokens: None,
            vision: false,
            effort: None,
            accepts_temperature: true,
            can_force_tool: true,
            pricing: Pricing::default(),
        }
    }
}

impl ModelEntry {
    /// The window in force: what a file or the endpoint stated, else the guess.
    pub fn window(&self) -> u32 {
        self.context_window.unwrap_or_else(default_context)
    }

    /// The output budget in force, guessed the same way.
    pub fn output(&self) -> u32 {
        self.max_output_tokens.unwrap_or_else(default_output)
    }
}

impl Config {
    fn spec(&self, name: &str, model: &ModelEntry, provider: &Provider) -> ModelSpec {
        ModelSpec {
            model: name.to_string(),
            wire_id: Some({
                name.split_once('/')
                    .expect("provider identity includes a separator")
                    .1
                    .to_string()
            }),
            base_url: provider.base_url.clone(),
            format: provider.wire_format(),
            headers: provider.headers(),
            session_header: provider.session_header.clone(),
            context_window: model.window(),
            max_output_tokens: model.output(),
            vision: model.vision,
            accepts_effort: model.effort != Some(EffortArg::Unsupported),
            accepts_temperature: model.accepts_temperature,
            can_force_tool: model.can_force_tool,
            pricing: model.pricing,
        }
    }

    // A `$NAME` that names nothing is a typo now and a missing key much later,
    // pointing at the endpoint rather than at the file.
    fn check_key(&self) -> Result<()> {
        let keys = self.provider.values().map(|p| p.api_key.as_deref());
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
    Discovered,
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
    /// A provider prefix selects that connection; a sole provider also accepts
    /// bare IDs that contain no provider separator.
    pub fn find(&self, name: &str) -> Result<ModelSpec> {
        let (identity, provider) = self.locate(name)?;
        Ok(
            match self.models.get(name).or_else(|| self.models.get(&identity)) {
                Some(model) => self.spec(&identity, model, provider),
                None => self.spec(&identity, &ModelEntry::default(), provider),
            },
        )
    }

    fn locate(&self, name: &str) -> Result<(String, &Provider)> {
        if let Some((prefix, id)) = name.split_once('/')
            && let Some(provider) = self.provider.get(prefix)
        {
            if id.is_empty() {
                bail!("a model ID is required after provider {prefix:?}");
            }
            return Ok((name.into(), provider));
        }
        if self.provider.is_empty() {
            bail!("no provider is configured to reach model {name:?}");
        }
        if let Some((prefix, _)) = name.split_once('/') {
            bail!("unknown provider {prefix:?} in model {name:?}");
        }
        if let Some((prefix, provider)) = self
            .provider
            .iter()
            .next()
            .filter(|_| self.provider.len() == 1)
        {
            return Ok((format!("{prefix}/{name}"), provider));
        }
        bail!("model {name:?} needs a provider prefix: use provider/model")
    }

    /// The entry in force for `name`, defaulted when nothing names it. A
    /// discovered model has one: the endpoint stated its ID, not its facts.
    pub fn entry(&self, name: &str) -> ModelEntry {
        self.locate(name)
            .map(|(identity, _)| identity)
            .ok()
            .and_then(|identity| self.models.get(&identity).cloned())
            .unwrap_or_default()
    }

    pub fn names(&self) -> Vec<String> {
        self.models.keys().cloned().collect()
    }

    /// The selected provider's credential as written, before environment expansion.
    pub fn written_key(&self, model: &str) -> Option<&str> {
        self.locate(model).ok()?.1.api_key.as_deref()
    }

    /// [`Config::written_key`], `$NAME` read: the credential to send, or None
    /// when the endpoint wants none. The shape is checked at load.
    pub fn key_for(&self, model: &str) -> Option<String> {
        self.written_key(model).and_then(expand_key)
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
        self.discovered
            .first()
            .map(|name| (name.clone(), ModelOrigin::Discovered))
    }
}

/// `$NAME` reads that environment variable; anything else is the key itself.
/// The one spelling of the credential contract, for every key in the file.
pub(crate) fn expand_key(raw: &str) -> Option<String> {
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
/// `home` is never treated as a project directory.
fn project_path(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    nearest(start, home, |dir| {
        Some(dir.join(PROJECT_FILE)).filter(|p| p.is_file())
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
/// `None` in `$HOME` itself, which is not a project directory.
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
    (home.as_deref() != Some(dir)).then(|| dir.join(PROJECT_FILE))
}

/// A file's partial tree. Schema validation follows the settings merge, when
/// inherited fields are present. A named file must exist.
fn read_tree(path: &Path, required: bool) -> Result<Option<toml::Value>> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let tree: toml::Value = toml::from_str(&body).with_context(|| format!("{}", path.display()))?;
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

/// The nearest `.pi/settings.toml` at or above `workspace`, if there is one.
pub fn project_file(workspace: &Path) -> Option<PathBuf> {
    project_path(workspace, home().as_deref())
}

/// Keys a project file may set that reach past its checkout: they start
/// programs, delete sessions, widen writes, or choose where a key is sent.
const REACHING: &[&str] = &["mcp", "hooks", "keep_days"];

/// The keys of a project `tree` that reach past its checkout, as spelled
/// there, each with its value as shown when it loads.
pub fn reaching(tree: &toml::Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = REACHING
        .iter()
        .filter_map(|key| Some((key.to_string(), shown(key, tree.get(*key)?))))
        .collect();
    for (name, entry) in tree
        .get("provider")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
    {
        for field in [
            "base_url",
            "api_key",
            "models_url",
            "headers",
            "session_header",
        ] {
            if let Some(value) = entry.get(field) {
                out.push((format!("provider.{name}.{field}"), shown(field, value)));
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

/// The nearest `.pi/settings.toml` at or above `workspace`, and its tree.
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
        for (name, provider) in &mut config.provider {
            provider.base_url = expand_base_url(provider.base_url.trim());
            provider.models_url = provider
                .models_url
                .take()
                .map(|url| expand_base_url(url.trim()));
            provider.check(name)?;
        }
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
                     a level is set by the top-level `effort` key"
                );
            }
            if !config.provider.is_empty() {
                config.find(model)?;
            }
        }
        config.key_table = std::sync::Arc::new(config.key_map()?);
        Ok(config)
    }

    /// The config a run uses, with model names resolved to their providers. Startup,
    /// a reload and an edit all come through here, so none of them
    /// accepts what another would refuse.
    pub fn in_force(tree: toml::Value) -> Result<Config> {
        let mut config = Self::from_tree(tree)?;
        let identities = config
            .models
            .keys()
            .map(|name| {
                config
                    .locate(name)
                    .map(|(identity, _)| (name.clone(), identity))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut entries = std::mem::take(&mut config.models);
        for (name, identity) in identities {
            let entry = entries
                .remove(&name)
                .expect("identity came from this model table");
            if config.models.insert(identity.clone(), entry).is_some() {
                bail!(
                    "multiple model entries resolve to {identity:?}; use its full provider/model identity once"
                );
            }
        }
        for model in config.models.keys() {
            config.find(model)?;
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
        std::fs::create_dir(dir.path().join(".pi")).unwrap();
        let body = "keep_days = 1\ntier = \"read\"\n\
                    [mcp.docs]\ncommand = \"x\"\n\
                    [provider.home]\nbase_url = \"https://elsewhere\"\nformat = \"chat\"\napi_key = \"sk-literal\"\n\
                    [models.\"home/model\"]\ncontext_window = 1000\n";
        std::fs::write(dir.path().join(".pi/settings.toml"), body).unwrap();
        let (_, tree) = load_project(dir.path()).unwrap().unwrap();
        let said = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            reaching(&tree),
            [
                said("mcp", "docs"),
                said("keep_days", "1"),
                said("provider.home.base_url", "https://elsewhere"),
                said("provider.home.api_key", "(hidden)"),
            ],
            "tier and the numbers stay within the checkout"
        );
    }

    #[test]
    fn the_shipped_example_is_a_valid_config() {
        let example = include_str!("../../../examples/settings.toml");
        let config = Config::from_tree(toml::from_str(example).unwrap());
        assert!(config.is_ok(), "{:?}", config.err());
    }

    #[test]
    fn old_connection_schema_is_refused() {
        for body in [
            "base_url = \"http://localhost/v1\"\n",
            "format = \"chat\"\n",
            "api_key = \"test-key\"\n",
            "cache_control = \"standard\"\n",
            "[models.a]\nbase_url = \"http://localhost/v1\"\n",
            "[models.a]\nformat = \"chat\"\n",
            "[models.a]\napi_key = \"test-key\"\n",
            "[models.a]\nthinking = true\n",
            "[models.a]\nreplay_thinking = true\n",
        ] {
            assert!(
                parse(body).is_err(),
                "retired configuration accepted: {body}"
            );
        }
    }

    #[test]
    fn provider_identity_is_distinct_from_the_model_sent_on_the_wire() {
        let c = parse(
            r#"
[provider.first]
base_url = ":3425/v1"
format = "chat"
api_key = "first-key"
[provider.second]
base_url = ":3426/v1"
format = "openai"
api_key = "second-key"
[models."first/auto/shared"]
context_window = 200000
"#,
        )
        .unwrap();
        let first = c.find("first/auto/shared").unwrap();
        let second = c.find("second/auto/shared").unwrap();
        assert_eq!(first.model, "first/auto/shared");
        assert_eq!(first.wire_model(), "auto/shared");
        assert_eq!(second.wire_model(), "auto/shared");
        assert_ne!(first.base_url, second.base_url);
        assert_eq!(first.context_window, 200000);
        assert_eq!(c.key_for(&first.model).as_deref(), Some("first-key"));
        assert_eq!(c.key_for(&second.model).as_deref(), Some("second-key"));
        assert!(
            c.find("shared").is_err(),
            "several providers require an identity"
        );
    }

    #[test]
    fn a_provider_hands_its_headers_to_every_model() {
        let c = parse(
            "[provider.local]\nbase_url = \":3425/v1\"\nformat = \"chat\"\n\
             [provider.local.headers]\nx-session = \"s1\"\n",
        )
        .unwrap();
        assert_eq!(
            c.find("m").unwrap().headers,
            [("x-session".to_string(), "s1".to_string())]
        );
    }

    #[test]
    fn a_sole_provider_accepts_bare_ids() {
        let c = parse(
            r#"
[provider.local]
base_url = ":3425/v1"
format = "chat"
"#,
        )
        .unwrap();
        let spec = c.find("new-model").unwrap();
        assert_eq!(spec.model, "local/new-model");
        assert_eq!(spec.wire_model(), "new-model");
        assert!(c.key_for("new-model").is_none());
        assert!(
            c.find("removed/new-model").is_err(),
            "a removed provider is not a model ID"
        );
    }

    const SAMPLE: &str = r#"
model = "local/flash"
effort = "medium"

[provider.local]
base_url = "http://localhost:7896/v1"
format = "openai"
api_key = "x"

[models."local/flash"]
context_window = 1_000_000
max_output_tokens = 384_000

[models."local/flash".pricing]
input_per_mtok = 0.14
# An integer where a float is wanted, which TOML distinguishes and serde does not.
output_per_mtok = 0
"#;

    fn parse(body: &str) -> Result<Config> {
        Config::from_tree(toml::from_str(body)?)
    }

    // Models on different providers do not share connection credentials.
    #[test]
    fn each_provider_keeps_its_own_key_and_protocol() {
        let c = parse(
            r#"
[provider.home]
base_url = "https://api.anthropic.com"
format = "anthropic"
cache_control = "standard"
api_key = "file-key"

[provider.gpt]
base_url = "https://api.openai.com/v1"
format = "chat"

[provider.local]
base_url = ":8080/v1"
format = "chat"
api_key = "local-key"
"#,
        )
        .unwrap();
        assert_eq!(c.key_for("home/model").as_deref(), Some("file-key"));
        assert_eq!(c.key_for("home/unlisted").as_deref(), Some("file-key"));
        assert_eq!(
            c.key_for("gpt/model"),
            None,
            "another provider's key stays with its host"
        );
        assert_eq!(c.key_for("local/model").as_deref(), Some("local-key"));

        let gpt = c.find("gpt/model").unwrap();
        assert_eq!(gpt.base_url, "https://api.openai.com/v1");
        assert!(
            matches!(gpt.format, llm::model::Format::Chat),
            "cache_control is not inherited"
        );
        assert_eq!(
            c.find("local/model").unwrap().base_url,
            "http://127.0.0.1:8080/v1"
        );
        assert_eq!(
            c.find("home/model").unwrap().base_url,
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn a_port_in_the_file_is_a_local_url() {
        let c = parse("[provider.local]\nbase_url = \":2/v1\"\nformat = \"chat\"\n").unwrap();
        assert_eq!(c.provider["local"].base_url, "http://127.0.0.1:2/v1");
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
        // is a model switch's job, and it says so when it happens.
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.model(None, Some("resumed"), true).unwrap().0, "resumed");
        assert_eq!(
            c.model(Some("flag"), Some("resumed"), true).unwrap().0,
            "flag"
        );
        assert_eq!(
            c.model(None, None, true),
            Some(("local/flash".into(), ModelOrigin::Project))
        );
    }

    #[test]
    fn the_home_file_is_never_read_as_a_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir(home.join(".pi")).unwrap();
        std::fs::write(home.join(".pi/settings.toml"), "").unwrap();
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
        std::fs::create_dir(outside.join(".pi")).unwrap();
        std::fs::write(outside.join(".pi/settings.toml"), "").unwrap();
        let repo = outside.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("packages/web");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_path(&deep, None), None, "leaked past the repo root");

        // One inside the repo is found from any depth below it.
        let inside = repo.join(".pi/settings.toml");
        std::fs::create_dir(repo.join(".pi")).unwrap();
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
