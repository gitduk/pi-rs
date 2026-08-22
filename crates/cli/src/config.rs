//! Startup defaults and locally-defined models.
//!
//! A working setup should not be a command line to retype. Pi keeps a provider
//! catalog (`~/.pi/agent/models.json`) apart from preferences
//! (`settings.json`); the split earns its keep there because the catalog is a
//! thing you copy between machines. One file with two sections is the same idea
//! with less to find.
//!
//! TOML rather than JSON for one reason: a compat value is a measurement, and a
//! measurement without its provenance rots. `usage_in_streaming = false` needs
//! the comment saying which proxy dropped the frame, and JSON has nowhere to
//! put it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use brain::catalog::{
    AnthropicCompat, Capabilities, ModelSpec, OpenAiCompat, Pricing, ThinkingReplay,
    ThinkingSupport, Wire,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::{EffortArg, TierArg, WireArg};

/// The user's own file: `~/.pi.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    /// Models this machine can reach that the built-in catalog cannot know
    /// about — a local proxy, a self-hosted server, a private deployment.
    /// Keyed by the handle `-m` selects.
    #[serde(default)]
    pub models: BTreeMap<String, Entry>,
    /// Key actions, each mapped to the presses that trigger it. An entry
    /// replaces that action's defaults rather than adding to them.
    #[serde(default)]
    pub keys: BTreeMap<String, Binds>,
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
    #[serde(default)]
    pub defaults: ProjectDefaults,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDefaults {
    pub model: Option<String>,
    pub effort: Option<EffortArg>,
    pub max_turns: Option<usize>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
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
    pub compat: BTreeMap<String, toml::Value>,
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
    overrides: &BTreeMap<String, toml::Value>,
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
        map.insert(key.clone(), serde_json::to_value(value)?);
    }
    serde_json::from_value(base).context("compat values did not fit the wire's record")
}

impl Entry {
    pub fn spec(&self, id: &str) -> Result<ModelSpec> {
        let wire = match self.wire {
            WireArg::Anthropic => Wire::Anthropic(compat::<AnthropicCompat>(&self.compat)?),
            WireArg::Openai => Wire::OpenAi(compat::<OpenAiCompat>(&self.compat)?),
        };
        Ok(ModelSpec {
            id: id.to_string(),
            wire_id: self.wire_id.clone().unwrap_or_else(|| id.to_string()),
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

    /// The key to send, or None to leave it to the wire's usual environment
    /// variable.
    pub fn key(&self) -> Option<Result<String>> {
        if let Some(literal) = &self.api_key {
            return Some(Ok(literal.clone()));
        }
        let name = self.api_key_env.as_ref()?;
        Some(std::env::var(name).with_context(|| format!("${name} is not set")))
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
            Origin::Global => "defaults.model in ~/.pi.toml",
            Origin::OnlyModel => "the only model in ~/.pi.toml",
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
    pub max_turns: usize,
}

impl Config {
    /// A config entry wins over a built-in one of the same name: the built-in
    /// table is our guess about a public endpoint, and the user is looking at
    /// the actual server.
    pub fn find(&self, id: &str) -> Option<(&str, &Entry)> {
        if let Some((k, e)) = self.models.get_key_value(id) {
            return Some((k.as_str(), e));
        }
        self.models
            .iter()
            .find(|(_, e)| e.wire_id.as_deref() == Some(id))
            .map(|(k, e)| (k.as_str(), e))
    }

    pub fn ids(&self) -> Vec<&str> {
        self.models.keys().map(String::as_str).collect()
    }

    /// Flag, then project, then this file, then the built-in default.
    ///
    /// The tier is the exception: a project may only pull it down, so it is
    /// applied as a ceiling after the rest of the chain has decided.
    pub fn settle(&self, project: &Project, flags: Flags) -> Settled {
        let tier = flags
            .tier
            .or(self.defaults.tier)
            .unwrap_or(TierArg::Exec)
            .min(project.defaults.max_tier.unwrap_or(TierArg::Exec));
        Settled {
            effort: flags
                .effort
                .or(project.defaults.effort)
                .or(self.defaults.effort)
                .unwrap_or(EffortArg::Off),
            tier,
            max_turns: flags
                .max_turns
                .or(project.defaults.max_turns)
                .or(self.defaults.max_turns)
                .unwrap_or(50),
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
        if let Some(m) = &project.defaults.model {
            return Some((m.clone(), Origin::Project));
        }
        if let Some(m) = &self.defaults.model {
            return Some((m.clone(), Origin::Global));
        }
        if self.models.len() == 1 {
            let only = self.models.keys().next().expect("len is 1");
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
    home().map(|h| h.join(".pi.toml"))
}

/// The nearest project file at or above `start`, stopping at the repository
/// root.
///
/// `home` is never searched: `~/.pi.toml` is the global file, and treating it
/// as a project file too would hand it privileges the global file already has
/// by other means — and hand every directory under `$HOME` outside a repo the
/// same file as its "project" config.
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
    let config: Config = toml::from_str(body)?;
    for (id, entry) in &config.models {
        // Rejected here rather than at use: a typo in a model you are not
        // running today is still a typo, and this is when it is cheap to see.
        entry.spec(id)?;
        if entry.api_key.is_some() && entry.api_key_env.is_some() {
            bail!("{id}: api_key and api_key_env both set; one would be silently ignored");
        }
    }
    config.key_map()?;
    Ok(config)
}

fn parse_project(body: &str) -> Result<Project> {
    toml::from_str(body).context(
        "a project .pi.toml may set only defaults.model, defaults.effort, \
         defaults.max_turns and defaults.max_tier — a checkout does not get to \
         name a server, a key, or a system prompt",
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
    use brain::catalog::MaxTokensField;

    const SAMPLE: &str = r#"
[defaults]
model = "flash"
effort = "medium"

[models.flash]
base_url = "http://localhost:7896/v1"
wire = "openai"
context_window = 1_000_000
max_output_tokens = 384_000
thinking = "effort"
api_key = "x"

[models.flash.pricing]
input_per_mtok = 0.14
# An integer where a float is wanted, which TOML distinguishes and serde does not.
output_per_mtok = 0

[models.flash.compat]
max_tokens_field = "max_tokens"
usage_in_streaming = false
"#;

    #[test]
    fn a_config_supplies_what_the_flags_would_have() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.defaults.model.as_deref(), Some("flash"));
        let (id, entry) = c.find("flash").unwrap();
        let spec = entry.spec(id).unwrap();
        assert_eq!(spec.context_window, 1_000_000);
        assert_eq!(spec.caps.thinking, Some(ThinkingSupport::Effort));
        assert_eq!(spec.pricing.input_per_mtok, 0.14);
        assert_eq!(spec.pricing.output_per_mtok, 0.0);
    }

    #[test]
    fn compat_names_only_what_differs() {
        let c = parse(SAMPLE).unwrap();
        let (id, entry) = c.find("flash").unwrap();
        let Wire::OpenAi(compat) = entry.spec(id).unwrap().wire else {
            panic!("openai")
        };
        assert_eq!(compat.max_tokens_field, MaxTokensField::MaxTokens);
        assert!(!compat.usage_in_streaming);
        // Untouched keys keep the wire's default rather than a zero value.
        assert!(compat.multiple_system_messages);
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
        let body = "[models.flash]\nwire_id = \"vendor-model-name\"\n\
                    base_url = \"http://x/v1\"\nwire = \"openai\"\n";
        let c = parse(body).unwrap();
        assert!(c.find("flash").is_some());
        let (id, entry) = c.find("vendor-model-name").unwrap();
        assert_eq!(id, "flash");
        assert_eq!(entry.spec(id).unwrap().wire_id, "vendor-model-name");
    }

    #[test]
    fn an_empty_config_is_a_valid_one() {
        assert!(parse("").unwrap().models.is_empty());
    }

    #[test]
    fn a_key_that_is_not_a_setting_is_refused_rather_than_ignored() {
        // A silently dropped key looks like a setting that does not work.
        let e = parse("[defaults]\nmodle = \"x\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("modle"), "{e}");
    }

    #[test]
    fn a_key_given_two_ways_is_refused_rather_than_one_being_dropped() {
        let body = "[models.a]\nbase_url = \"http://x/v1\"\nwire = \"openai\"\n\
                    api_key = \"lit\"\napi_key_env = \"SOME_VAR\"\n";
        let e = parse(body).unwrap_err().to_string();
        assert!(e.contains("silently ignored"), "{e}");
    }

    #[test]
    fn a_repeated_model_is_the_format_s_problem_not_ours() {
        // TOML rejects a duplicate key itself, which is one check we do not have
        // to write and cannot forget.
        let one = "[models.a]\nbase_url = \"http://x/v1\"\nwire = \"openai\"\n";
        assert!(parse(&format!("{one}{one}")).is_err());
    }

    #[test]
    fn a_project_may_pick_a_model_and_turn_the_dials() {
        let p = parse_project("[defaults]\nmodel = \"flash\"\nmax_turns = 10\n").unwrap();
        let c = Config::default();
        assert_eq!(c.model(&p, None, None).unwrap().0, "flash");
        assert_eq!(c.settle(&p, Flags::default()).max_turns, 10);
    }

    #[test]
    fn a_project_cannot_name_a_server_of_its_own() {
        // The whole point: a repository arrives by git clone, and this is the
        // line between "configure the run" and "redirect it".
        for body in [
            "[models.evil]\nbase_url = \"http://attacker/v1\"\nwire = \"openai\"\n",
            "[defaults]\nsystem = \"/etc/shadow\"\n",
            "[defaults]\ntier = \"exec\"\n",
        ] {
            assert!(parse_project(body).is_err(), "accepted: {body}");
        }
    }

    #[test]
    fn a_project_lowers_the_tier_and_cannot_raise_it() {
        let c: Config = parse("[defaults]\ntier = \"write\"\n").unwrap();
        let down = parse_project("[defaults]\nmax_tier = \"read\"\n").unwrap();
        assert_eq!(c.settle(&down, Flags::default()).tier, TierArg::Read);

        let up = parse_project("[defaults]\nmax_tier = \"exec\"\n").unwrap();
        assert_eq!(c.settle(&up, Flags::default()).tier, TierArg::Write);
    }

    #[test]
    fn a_flag_outranks_both_files_but_still_meets_the_ceiling() {
        let c = Config::default();
        let p = parse_project("[defaults]\neffort = \"low\"\nmax_tier = \"read\"\n").unwrap();
        let flags = Flags {
            effort: Some(EffortArg::High),
            tier: Some(TierArg::Exec),
            max_turns: None,
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
        let p = parse_project("[defaults]\nmodel = \"other\"\n").unwrap();
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
        let body = "[models.flash]\nbase_url = \"http://x/v1\"\nwire = \"openai\"\n";
        let c = parse(body).unwrap();
        let got = c.model(&Project::default(), None, None);
        assert_eq!(got, Some(("flash".to_string(), Origin::OnlyModel)));
    }

    #[test]
    fn two_models_and_no_default_still_has_to_be_told() {
        // There is no defensible pick among them, and alphabetical is not one.
        let two = "[models.a]\nbase_url=\"http://x/v1\"\nwire=\"openai\"\n\
                   [models.b]\nbase_url=\"http://y/v1\"\nwire=\"openai\"\n";
        let c = parse(two).unwrap();
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
        let c = parse("[defaults]\nmodel = \"typo\"\n").unwrap();
        let (name, origin) = c.model(&Project::default(), None, None).unwrap();
        assert_eq!(name, "typo");
        assert!(origin.describe().contains("~/.pi.toml"));
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
    fn a_broken_binding_is_refused_at_load() {
        // Not at the keystroke: a key that silently does nothing is the worst
        // way to find out a config is wrong.
        let e = parse("[keys]\n\"move.line.start\" = \"ctrl+nope\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("move.line.start"), "{e}");
    }
}
