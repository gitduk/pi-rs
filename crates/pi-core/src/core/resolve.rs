//! What the config and the workspace decide for one checkout: the tools, the
//! prompt, the ceiling, the commands. Startup, a reload and every new lane
//! come through here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use llm::request::Effort;

use crate::args::Pinned;
use crate::input::commands::{Command, commands};
use agent::context;
use pi_store::args::EffortArg;
use pi_store::settings::Settings;
use pi_store::{config, journal};

/// Everything the config and the workspace decide, as opposed to what the
/// command line fixed for the whole run. A reload recomputes exactly this.
#[derive(Clone)]
pub struct Resolved {
    /// What the agent runs on: the tools, the prompt, the ceiling, the effort.
    /// A subagent derives its own from this one.
    pub brief: std::sync::Arc<agent::Briefing>,
    /// The tail of the prompt that belongs to the run rather than to the
    /// assistant: the workspace anchor, what the run is, and the instruction
    /// files. Kept apart because the subagent has its own prompt but the same
    /// tree, the same machine and the same tier.
    pub standing: std::sync::Arc<str>,
    /// How far this run may reach. The lane needs it to decide whether the
    /// subagent, offered after this set is cut, belongs in it.
    pub ceiling: tool::Tier,
    /// The key table this tree asked for, defaults included.
    pub keys: std::sync::Arc<pi_store::keys::Keys>,
    /// The built-ins plus one command per skill. Here rather than in the Core
    /// because a skill discovered at reload has to reach the prompt the same
    /// way everything else the config decides does.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// Every file this was built from, as it stood: when one moves, the lane
    /// is resolved again. Skills and scripts are read live and are not here.
    pub watched: Vec<Stamp>,
    /// Where skills are read from, live; `None` under `--no-skills`.
    pub shelf: Option<std::sync::Arc<skills::Shelf>>,
    /// Which state of the shelf `commands` was built from.
    pub shelf_seen: u64,
    /// Worth saying once, at startup and at each reload.
    pub notes: Vec<String>,
    /// The instruction files folded into the system prompt, named as a person
    /// would. Shown under the banner rather than said as a note: it is what
    /// this run is standing on, not news.
    pub context: Vec<String>,
    /// The memory files folded into the prompt, by the name an edit uses.
    pub memory: Vec<String>,
    /// Where requests go and which source said so, for the banner and
    /// `/status`: two files can each name one now.
    pub endpoint: Option<String>,
}

/// Offer `tool` to the set, saying so when an earlier source holds its name.
fn offer(
    registry: &mut tool::Registry,
    notes: &mut Vec<String>,
    source: &str,
    tool: Arc<dyn tool::Tool>,
) {
    let name = tool.name().to_string();
    if !registry.offer(tool) {
        notes.push(format!("tool skipped — {source} {name}: the name is taken"));
    }
}

/// Fails whole or not at all. A half-applied config is worse than a stale one,
/// which is why a reload computes all of this before touching anything.
pub fn resolve(
    pinned: &Pinned,
    workspace: &tool::Workspace,
    config: &config::Config,
    settings: &Settings,
) -> Result<Resolved> {
    let root = workspace.root();
    let mut notes = Vec::new();
    // Before any of it is read, so a write landing meanwhile reads as a change.
    let read_from = watched(pinned, config, root);

    // Sources offer their tools in order, built-ins first.
    let mut registry = toolbox::builtin();
    // Read live, like the scripts: a skill written while pi runs can be loaded
    // on the next turn, and typed once `Core::refresh_skills` has seen it.
    let shelf = pi_store::dir()
        .filter(|_| !pinned.no_skills)
        .map(|pi| Arc::new(skills::Shelf::new(pi.join("skills"))));
    let (found, shelf_seen) = shelf.as_ref().map(|s| s.now()).unwrap_or_default();
    // A skill that silently fails to appear is one the user goes looking
    // for in the wrong place.
    notes.extend(
        found
            .problems
            .iter()
            .map(|p| format!("skill skipped — {p}")),
    );
    // A skill is two things at once, a command the user can type and a body
    // the model can load, and both read the same list.
    let commands = commands(&found.skills, &mut notes);
    if let Some(shelf) = &shelf {
        if registry.get(skills::Load::NAME).is_some() {
            notes.push("tool skipped — skill: the name is taken".to_string());
        }
        registry.read(shelf.clone());
    }

    // A judgment endpoint is opt-in by section: no `[judge]` in the file, no
    // tool in the set.
    if let Some(judge) = &config.judge {
        let judge = toolbox::judge::Judge::new(judge.endpoint(), judge.key(), judge.model.clone());
        offer(&mut registry, &mut notes, "judge", Arc::new(judge));
    }

    // Read live: a script written while pi runs is a tool from the next turn.
    let scripts =
        pi_store::dir().map(|root| Arc::new(toolbox::scripts::Dir::new(root.join("tools"))));
    if let Some(scripts) = &scripts {
        notes.extend(
            scripts
                .skipped()
                .iter()
                .map(|p| format!("tool skipped — {p}")),
        );
        // Offered tools win a name, the subagent among them though it is
        // offered later, per lane.
        for script in tool::Source::tools(&**scripts) {
            if registry.get(script.name()).is_some() || script.name() == subagent::Subagent::NAME {
                notes.push(format!(
                    "tool skipped — user script {}: the name is taken",
                    script.name()
                ));
            }
        }
    }
    // Offered per lane after all of the above, so a holder of its name is
    // known here: say so once, not on every re-arm.
    if registry.get(subagent::Subagent::NAME).is_some() {
        notes.push("tool skipped — subagent: the name is taken".to_string());
    }
    if let Some(scripts) = scripts {
        registry.read(scripts);
    }

    let settled = config.settle(config::Flags {
        effort: pinned.effort,
        tier: pinned.tier,
    });
    let tier = tool::Tier::from(settled.tier);
    let effort = match settled.effort {
        EffortArg::Off => Effort::Off,
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
    };

    let mut system = match pinned.system.as_ref().or(config.system.as_ref()) {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read system prompt {path}"))?,
        None => agent::DEFAULT_SYSTEM.to_string(),
    };
    // The system prompt's "relative to it" needs the workspace named.
    let stamp = journal::rfc3339(std::time::SystemTime::now());
    let mut standing = context::workspace(root);
    standing.push_str(&context::boundary(workspace, tier));
    standing.push_str(&context::env(&stamp, tier));
    // Appended rather than sent as a message: standing instructions don't
    // change within a run, and the system prompt is what a provider caches.
    let mut context = Vec::new();
    let mut memory = Vec::new();
    if !pinned.no_context_files {
        let loaded = context::load(root, pi_store::dir().as_deref());
        context = loaded
            .files
            .iter()
            .map(|p| context::short(p, root))
            .collect();
        standing.push_str(&loaded.text);
        // Last: what was learned is read in light of what the user wrote down.
        let (block, files) = crate::core::memory::block(&pi_store::memory::Memory::default(), root);
        standing.push_str(&block);
        memory = files;
    }
    system.push_str(&standing);

    Ok(Resolved {
        brief: std::sync::Arc::new(agent::Briefing {
            registry: registry.within(tier),
            system,
            effort,
            approver: std::sync::Arc::new(agent::Ceiling(tier)),
            subagent_deadline: config
                .subagent_deadline
                .map(|s| std::time::Duration::from_secs(s.max(1))),
        }),
        standing: standing.into(),
        ceiling: tier,
        keys: std::sync::Arc::new(config.key_map()?),
        commands: std::sync::Arc::new(commands),
        watched: read_from,
        shelf,
        shelf_seen,
        notes,
        context,
        memory,
        endpoint: endpoint(pinned, config, settings, root),
    })
}

/// A file as last seen: its path, time and size.
pub type Stamp = (PathBuf, Option<std::time::SystemTime>, u64);

/// The files a resolve in `root` reads, as they stand now: the settings, the
/// system prompt, the instructions and memory. Missing ones are left out, so
/// one appearing is a change too.
pub fn watched(pinned: &Pinned, config: &config::Config, root: &Path) -> Vec<Stamp> {
    let mut paths: Vec<PathBuf> = Vec::new();
    paths.extend(
        pinned
            .config
            .as_ref()
            .map(PathBuf::from)
            .or_else(config::global_path),
    );
    paths.extend(config::project_file(root));
    paths.extend(
        pinned
            .system
            .as_ref()
            .or(config.system.as_ref())
            .map(PathBuf::from),
    );
    if !pinned.no_context_files {
        let pi = pi_store::dir();
        paths.extend(context::paths(
            root,
            context::home().as_deref(),
            pi.as_deref(),
        ));
        let memory = pi_store::memory::Memory::default();
        paths.extend(memory.paths(&crate::core::worktree::main_root(root)));
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            Some((path, meta.modified().ok(), meta.len()))
        })
        .collect()
}

// The url requests go to, and the one source it came from, in the order
// they rank: the flag, the environment, the files.
fn endpoint(
    pinned: &Pinned,
    config: &config::Config,
    settings: &Settings,
    root: &Path,
) -> Option<String> {
    let (url, from) = match &pinned.base_url {
        Some(url) => (config::expand_base_url(url), "--base-url".to_string()),
        None => {
            let url = config.base_url.clone()?;
            let from = if let Some(var) = config::endpoint_env() {
                format!("${var}")
            } else if let Some(file) = settings.project_sets("base_url") {
                context::short(file, root)
            } else {
                match &pinned.config {
                    Some(file) => file.clone(),
                    None => config::global_path()
                        .map_or_else(|| "settings.toml".into(), |p| context::short(&p, root)),
                }
            };
            (url, from)
        }
    };
    Some(format!("endpoint: {url} ({from})"))
}
