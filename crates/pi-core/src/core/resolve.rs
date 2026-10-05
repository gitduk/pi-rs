//! What the config and the workspace decide for one checkout: the tools, the
//! prompt, the ceiling, the commands. Startup, `/reload` and every new lane
//! come through here.

use std::path::Path;
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
/// command line fixed for the whole run. `/reload` recomputes exactly this.
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
/// which is why `/reload` computes all of this before touching anything.
pub fn resolve(
    pinned: &Pinned,
    workspace: &tool::Workspace,
    config: &config::Config,
    settings: &Settings,
) -> Result<Resolved> {
    let root = workspace.root();
    let mut notes = Vec::new();

    // Sources offer their tools in order, built-ins first.
    let mut registry = toolbox::builtin();
    let skills = if pinned.no_skills {
        Vec::new()
    } else {
        let found = pi_store::dir()
            .map(|pi| skills::discover(&pi.join("skills")))
            .unwrap_or_default();
        // A skill that silently fails to appear is one the user goes looking
        // for in the wrong place.
        notes.extend(
            found
                .problems
                .iter()
                .map(|p| format!("skill skipped — {p}")),
        );
        found.skills
    };
    // Before the move: a skill is two things at once, a command the user can
    // type and a body the model can load, and both read the same list.
    let commands = commands(&skills, &mut notes);
    let tool = skills::Load::new(skills);
    if !tool.is_empty() {
        offer(&mut registry, &mut notes, "skill", Arc::new(tool));
    }

    // A judgment endpoint is opt-in by section: no `[judge]` in the file, no
    // tool in the set.
    if let Some(judge) = &config.judge {
        let judge = toolbox::judge::Judge::new(judge.endpoint(), judge.key(), judge.model.clone());
        offer(&mut registry, &mut notes, "judge", Arc::new(judge));
    }

    let (scripts, skipped) = pi_store::dir()
        .map(|root| toolbox::scripts::discover_in(&root.join("tools")))
        .unwrap_or_default();
    notes.extend(skipped.iter().map(|p| format!("tool skipped — {p}")));
    for script in scripts {
        offer(&mut registry, &mut notes, "user script", Arc::new(script));
    }
    // Offered per lane after all of the above, so a holder of its name is
    // known here: say so once, not on every re-arm.
    if registry.get(subagent::Subagent::NAME).is_some() {
        notes.push("tool skipped — subagent: the name is taken".to_string());
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
        notes,
        context,
        memory,
        endpoint: endpoint(pinned, config, settings, root),
    })
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
