use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use llm::model::{CacheControl, Format, ModelSpec};
use llm::request::Effort;
use llm::transport::{Transport, anthropic::Anthropic, chat::ChatCompletions, openai::OpenAi};
use tokio::sync::mpsc;

use crate::core::{Core, lane, subagent, wechat, worktree};
use crate::input::commands::{Command, commands};
use crate::input::expand;
use crate::store::icons;
use crate::store::settings::Settings;
use crate::store::{config, journal, session};
use crate::ui::{render, tui};
use agent::context;

mod core;
mod input;
mod store;
mod ui;

/// The three below are both flags and config values, so a config file names a
/// tier the same way the command line does.
#[derive(Debug, Clone, Copy, PartialEq, ValueEnum, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatArg {
    Anthropic,
    Openai,
    Chat,
}

impl FormatArg {
    // The format this names. Caching stays off: nothing on this path was
    // measured, and an unknown top-level field is a 400 on some servers.
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

impl TierArg {
    /// A project ceiling applied downward. `tool::Tier` owns the rule, which
    /// is not `min`: `write` and `net` have no order between them.
    pub fn capped_by(self, other: Self) -> Self {
        tool::Tier::from(self).capped_by(other.into()).into()
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

#[derive(Parser, Debug)]
#[command(
    name = "pi",
    about = "A coding agent that stays inside one directory.",
    version = env!("CARGO_PKG_VERSION"),
    disable_version_flag = true
)]
pub struct Args {
    /// Print the version and exit.
    #[arg(short = 'v', long, action = clap::ArgAction::Version)]
    version: Option<bool>,
    /// The prompt. Reads stdin when omitted. Runs once and keeps nothing.
    prompt: Option<String>,

    /// Defaults and locally-defined models. Defaults to ~/.pi/settings.toml.
    #[arg(long, value_name = "FILE", env = "PI_CONFIG")]
    config: Option<String>,

    /// What the endpoint calls the model. A name ~/.pi/settings.toml does not
    /// describe is passed through with default numbers. Defaults to the
    /// resumed session's model, else the config's.
    #[arg(short, long)]
    model: Option<String>,

    /// Call this session something you will recognise later.
    #[arg(long, value_name = "TEXT")]
    name: Option<String>,

    /// Continue a saved session by id.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,

    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,

    /// Overrides the base url the model's provider names, for pointing a
    /// configured model at a different host.
    #[arg(long)]
    base_url: Option<String>,

    /// Directory the agent may touch. Nothing outside it is reachable.
    #[arg(short = 'C', long, default_value = ".")]
    cwd: String,

    /// What this run may reach. read, write and exec each reach further
    /// into this machine; net reaches the web and nothing else. Defaults to
    /// exec, which covers net.
    #[arg(long, value_enum)]
    tier: Option<TierArg>,

    #[arg(long, value_enum)]
    effort: Option<EffortArg>,

    /// Override the model's context window, for a proxy whose real window is
    /// smaller than the config says.
    #[arg(long, value_name = "TOKENS")]
    context: Option<u32>,

    /// Replace the built-in system prompt.
    #[arg(long)]
    system: Option<String>,

    /// Ignore the skills on disk.
    #[arg(long)]
    no_skills: bool,

    /// Ignore ~/.pi/AGENTS.md and the project's.
    #[arg(long)]
    no_context_files: bool,

    /// Answer only; no progress, no usage line.
    #[arg(short, long)]
    quiet: bool,
}

// `configured` is the config's `api_key`; the environment variable is the
// fallback. The two OpenAI-family wires share `OPENAI_API_KEY`; Anthropic
// takes `ANTHROPIC_API_KEY`. A key is never required: whichever is set rides
// along, and an endpoint that needs one answers with its own refusal.
fn transport_for(spec: &ModelSpec, configured: Option<String>) -> Arc<dyn Transport> {
    let key = configured.or_else(|| {
        let var = match spec.format {
            Format::Chat | Format::OpenAi => "OPENAI_API_KEY",
            Format::Anthropic { .. } => "ANTHROPIC_API_KEY",
        };
        std::env::var(var).ok()
    });
    match spec.format {
        Format::Anthropic { .. } => Arc::new(Anthropic::new(key)),
        Format::OpenAi => Arc::new(OpenAi::new(key)),
        Format::Chat => Arc::new(ChatCompletions::new(key)),
    }
}

/// One model, ready to talk to: what to send, and the client to send it with.
pub struct Dialled {
    pub spec: ModelSpec,
    pub transport: Arc<dyn Transport>,
    /// Worth saying once — at startup, and again at every `/model`. Startup
    /// drops these under `--quiet`, which asks for the answer and nothing
    /// around it. `/model` prints them either way: the user typed a command
    /// whose whole purpose is to report, and a silent one would read as broken.
    pub notes: Vec<String>,
    /// Said even under `--quiet`, which is why it is not one of the notes. An
    /// exposed key is a fact about the machine rather than progress chatter,
    /// and the run that asked for silence is the scripted one nobody is
    /// watching — exactly the one that would never hear it again.
    pub warning: Option<String>,
}

// Why this run wanted that model, for an endpoint that cannot serve it.
//
// Every name resolves now — an unlisted one is passed through with default
// numbers — so the only way this fails is a config with no endpoint to send
// it to, and the useful half of that message is who asked.
fn unknown(model: &str, named_by: config::Origin) -> String {
    format!(
        "`{model}`, named by {}, cannot be reached — see examples/pi.toml",
        named_by.describe()
    )
}

/// Resolve a model name into something that can be talked to.
///
/// Startup and `/model` share this so the two cannot decide differently about
/// the same name. The config is the only way in: a model worth talking to is
/// worth four lines naming its endpoint and protocol, and every other field
/// already defaults to claiming nothing.
pub fn dial(
    args: &Args,
    config: &config::Config,
    model: &str,
    named_by: config::Origin,
) -> Result<Dialled> {
    let mut spec = config
        .find(model)
        .with_context(|| unknown(model, named_by))?;
    if let Some(url) = &args.base_url {
        spec.base_url = config::expand_base_url(url);
    }
    if let Some(window) = args.context {
        spec.context_window = window;
    }

    let mut notes = Vec::new();
    // A passed-through model is a guess. Saying which guess lets the user
    // correct the one that matters instead of debugging a 400 later.
    if !config.is_written(model) {
        notes.push(format!(
            "assuming a {}-token window, {} max output, no pricing, and no \
             thinking for `{}`. Set --context if the server's window differs; \
             --effort needs a config entry naming the model's thinking shape.",
            spec.context_window, spec.max_output_tokens, spec.model
        ));
    }
    let key = config.key();
    let warning = config
        .api_key
        .as_deref()
        .filter(|k| !k.starts_with('$'))
        .and_then(|_| {
            args.config
                .clone()
                .map(std::path::PathBuf::from)
                .or_else(config::global_path)
        })
        .and_then(|path| config::warn_if_exposed(&path));
    let transport = transport_for(&spec, key);
    Ok(Dialled {
        spec,
        transport,
        notes,
        warning,
    })
}

/// The summarizer's own connection, when the config names a model for it: a
/// second dial, because a summary is a model call like any other and a cheaper
/// model for it is the point of the setting.
pub fn summary_writer(
    args: &Args,
    config: &config::Config,
    working: &str,
) -> Result<Option<(Arc<dyn Transport>, ModelSpec)>> {
    match &config.summarize_model {
        Some(name) if name != working => {
            let summarizer = dial(args, config, name, config::Origin::Global)
                .with_context(|| format!("summarize_model = \"{name}\""))?;
            Ok(Some((summarizer.transport, summarizer.spec)))
        }
        _ => Ok(None),
    }
}

// The prompt, or None when the run should ask for one.
fn read_prompt(args: &Args) -> Result<Option<String>> {
    if let Some(p) = &args.prompt {
        // An ask with no text in it is a message the provider refuses, and its
        // wording helps nobody. Said here, where the prompt still can be.
        if p.trim().is_empty() {
            bail!("the prompt is empty — say what to do");
        }
        return Ok(Some(p.clone()));
    }
    // A bare `pi` at a terminal means "talk to me"; piped in, it means the
    // prompt is on stdin. Interactive needs a terminal, which `main` checks.
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut body = String::new();
    std::io::stdin().read_to_string(&mut body)?;
    if body.trim().is_empty() {
        bail!("no prompt given, and stdin was empty");
    }
    Ok(Some(body))
}

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
    /// The key table this tree asked for, defaults included.
    pub keys: std::sync::Arc<crate::store::keys::Keys>,
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
    args: &Args,
    workspace: &tool::Workspace,
    config: &config::Config,
    project: &config::Project,
    claimed: &BTreeMap<String, toml::Value>,
) -> Result<Resolved> {
    let root = workspace.root();
    let mut notes = Vec::new();

    // Sources offer their tools in order, built-ins first.
    let mut registry = toolbox::builtin();
    let skills = if args.no_skills {
        Vec::new()
    } else {
        let found = skills::discover(root);
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
        let endpoint = judge.endpoint();
        // Named at startup because this one leaves the machine and costs
        // money: a tool in the set the user did not ask for is worth saying
        // out loud, and the endpoint says which account is paying.
        notes.push(format!("judge: snap judgments via {endpoint}"));
        let judge = toolbox::judge::Judge::new(endpoint, judge.key(), judge.model.clone());
        offer(&mut registry, &mut notes, "judge", Arc::new(judge));
    }

    let (scripts, skipped) = context::home()
        .map(|home| toolbox::scripts::discover_in(&home.join(".pi/tools")))
        .unwrap_or_default();
    notes.extend(skipped.iter().map(|p| format!("tool skipped — {p}")));
    for script in scripts {
        offer(&mut registry, &mut notes, "user script", Arc::new(script));
    }

    let settled = config.settle(
        &project.clone(),
        config::Flags {
            effort: args.effort,
            tier: args.tier,
        },
        claimed,
    );
    let tier = tool::Tier::from(settled.tier);
    let effort = match settled.effort {
        EffortArg::Off => Effort::Off,
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
    };

    let mut system = match args.system.as_ref().or(config.system.as_ref()) {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read system prompt {path}"))?,
        None => agent::DEFAULT_SYSTEM.to_string(),
    };
    // The system prompt's "relative to it" needs the workspace named.
    let stamp = journal::rfc3339(std::time::SystemTime::now());
    let mut standing = context::workspace(root);
    standing.push_str(&context::boundary(workspace, tier));
    standing.push_str(&context::env(&stamp, tier));
    // Appended rather than sent as a message: these are standing instructions,
    // they do not change within a run, and the system prompt is the part of the
    // request a provider will cache.
    let mut context = Vec::new();
    if !args.no_context_files {
        let loaded = context::load(root);
        context = loaded
            .files
            .iter()
            .map(|p| context::short(p, root))
            .collect();
        standing.push_str(&loaded.text);
    }
    system.push_str(&standing);

    Ok(Resolved {
        brief: std::sync::Arc::new(agent::Briefing {
            registry,
            system,
            effort,
            approver: std::sync::Arc::new(agent::Ceiling(tier)),
            subagent_deadline: config
                .subagent_deadline
                .map(|s| std::time::Duration::from_secs(s.max(1))),
        }),
        standing: standing.into(),
        keys: std::sync::Arc::new(config.key_map()?),
        commands: std::sync::Arc::new(commands),
        notes,
        context,
    })
}

// Conventional exit code for a process killed by SIGINT.
const INTERRUPTED: i32 = 130;

// First Ctrl-C cancels the run; a second one leaves. The terminal's own policy,
// which is why it sits here rather than in the agent: it writes to stderr and
// ends the process.
//
// `tokio::signal::ctrl_c` replaces SIGINT's default action for the whole
// process and never restores it, so a handler that only fires once leaves no
// way out at all — the second press has to do the killing itself.
fn cancel_on_interrupt() -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        child.cancel();
        eprintln!("\ninterrupting — press Ctrl-C again to quit");
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(INTERRUPTED);
        }
    });
    token
}

// Renders a one-shot run's events by printing them. Its own task so a slow
// write never holds the run up.
fn paint(
    mut rx: mpsc::UnboundedReceiver<agent::Event>,
    quiet: bool,
    theme: std::sync::Arc<crate::store::theme::Theme>,
    done: Vec<crate::store::status::Segment>,
    model: String,
    pricing: llm::model::Pricing,
    worktree: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet, theme, done, model, pricing, worktree);
        while let Some(event) = rx.recv().await {
            r.on(event);
        }
        r.finish();
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::sync::Arc::new(Args::parse());
    let prompt = read_prompt(&args)?;
    let config = Arc::new(config::load(args.config.as_deref())?);

    let workspace = tool::Workspace::new(&args.cwd)
        .and_then(|ws| ws.with_write_roots(&config.write_roots))
        .with_context(|| format!("cannot use {} as a workspace", args.cwd))?;
    let project = config::load_project(workspace.root())?;

    let store = session::Store::default();
    // Off the startup path, like the journal's own sweep: it stats every
    // bucket and almost never has anything to take. A run that exits first
    // loses nothing — the next one sweeps.
    tokio::task::spawn_blocking({
        let store = store.clone();
        move || {
            // The journals live in the buckets now, so the two sweeps walk one
            // tree. Transcripts go by reach, journals by age — a run worth
            // reading back is a fortnight old at most, and the work is not.
            journal::prune(store.root());
            store.prune();
        }
    });
    let prior = match (&args.resume, args.continue_last) {
        (Some(id), _) => Some(store.load(id)?),
        (None, true) => Some(store.latest(workspace.root())?),
        _ => None,
    };

    // What this run keeps: an interactive session, or one the user named with
    // `-c`/`--resume`. A one-shot prompt leaves nothing behind, not even a log.
    let keeps = prior.is_some() || prompt.is_none();

    // The one surface needs the terminal at both ends: keys come in one side,
    // the repaint goes out the other. Asked before the journal and the session
    // directory are made, so a run that cannot start leaves neither behind.
    if prompt.is_none() && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!(
            "interactive mode needs a terminal on both stdin and stdout \
             — give a prompt to run once instead"
        );
    }

    let id = prior
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(session::new_id);

    let Some((named, named_by)) = config.model(
        &project,
        args.model.as_deref(),
        prior.as_ref().map(|p| p.model.as_str()),
    ) else {
        bail!("no model to run. Define one in ~/.pi/settings.toml — see examples/pi.toml.");
    };
    let dialled = dial(&args, &config, &named, named_by)?;

    let resolved = resolve(&args, &workspace, &config, &project, &BTreeMap::new())?;
    // Ahead of the quiet check on purpose: see `Dialled::warning`.
    if let Some(warning) = &dialled.warning {
        eprintln!("\x1b[{}m{warning}\x1b[0m", config.theme.muted.codes());
    }
    if !args.quiet {
        for note in dialled.notes.iter().chain(&resolved.notes) {
            eprintln!("\x1b[{}m{note}\x1b[0m", config.theme.muted.codes());
        }
    }
    // Captured before the spec and workspace move into the agent and context.
    let root = workspace.root().to_path_buf();
    // Asked once: it shells out to git, and three startups of that was the
    // pause `Lists` exists to avoid.
    let worktree = worktree::current(&root);
    let model_id = dialled.spec.model.clone();

    let mut ag = agent::Agent::new(dialled.transport, dialled.spec);
    // Resolved here rather than lazily: a name that does not exist should be a
    // startup error, not a surprise the first time history gets long enough to
    // compact.
    let writer = summary_writer(&args, &config, &model_id)?;
    // A resumed session keeps its journal too, so the whole of it reads as one
    // file however many runs it took. Installed after every step that can
    // refuse to start and before the run: a start that refuses leaves no
    // session directory behind, and one that goes ahead has its log from the
    // first turn.
    if keeps {
        journal::install(
            &store.journal_path(workspace.root(), &id),
            journal::level_from_env(),
        );
        journal::opening(
            &id,
            &args,
            &config,
            &project,
            workspace.root(),
            prior.as_ref(),
        );
    }
    let retry = config.retry();
    // Installed last: the compactor watches its own stream by the run's idle
    // timeout, which `Config::retry` just settled. Without one the transcript
    // is never shrunk — see `agent::Compactor`.
    ag.compactor = Arc::new(agent::Summarizing::new(writer, retry.idle));
    let home = if keeps {
        subagent::Filed::armed(store.clone(), root.clone(), model_id.clone())
    } else {
        subagent::nowhere()
    };
    // Armed here rather than field by field: the brief the lane keeps and the
    // one the agent runs on are the same value, subagent tool and all.
    let resolved = lane::arm(&mut ag, Arc::new(resolved), home, retry);
    let key_map = resolved.keys.clone();

    // An explicit --name renames a resumed session; otherwise it keeps its own.
    let name = args
        .name
        .clone()
        .or_else(|| prior.as_ref().and_then(|p| p.name.clone()));
    let created = prior
        .as_ref()
        .map(|p| p.created)
        .unwrap_or_else(session::now);
    let carried = prior.map(|p| p.into_session()).unwrap_or_default();
    let resumed = carried.context().len();

    let Some(prompt) = prompt else {
        // Before `id` moves into the Core: the context borrows it to name the
        // session its spills belong to. `commands` is what the Core shows for
        // the front lane; the lane's own copy travels in `resolved`.
        let commands = resolved.commands.clone();
        let ctx = tool::Ctx::new(workspace).with_session(&id);
        let mut first = lane::Lane::opened(lane::Opening {
            id,
            created,
            name,
            worktree,
            ..lane::Opening::new(Arc::new(ag), resolved, ctx)
        });
        first.return_session(carried);
        let core = Core {
            store,
            keys: key_map.clone(),
            config: config.clone(),
            args: args.clone(),
            commands,
            settings: Settings::new(
                config::load_tree(args.config.as_deref())
                    .unwrap_or_else(|_| toml::Value::Table(Default::default())),
            ),
            current: 0,
            lanes: vec![first],
        };
        let out = tui::Tui::new(core, key_map, wechat::Bridge::new())?
            .run()
            .await;
        // Subagents handed their transcripts to a background save; wait
        // for those to land before the runtime goes with them.
        subagent::flush().await;
        return out;
    };

    // A skill command is a prompt, so it means here what it means at the
    // terminal. The built-ins are not: they operate on a session, and a run
    // that answers once has none to operate on.
    let prompt = match expand(&resolved.commands, &prompt) {
        Some(Ok(instructions)) => instructions,
        Some(Err(why)) => bail!("{why}"),
        None => prompt,
    };

    // Built here, past the interactive return above: the one-shot path is the
    // only one that prints what a run emits.
    let (tx, rx) = mpsc::unbounded_channel();
    let quiet = args.quiet;
    let painter = paint(
        rx,
        quiet,
        std::sync::Arc::new(config.theme.clone()),
        config.status.done.clone(),
        model_id.clone(),
        ag.spec().pricing,
        worktree.clone(),
    );
    let mut ctx = tool::Ctx::new(workspace).with_cancel(cancel_on_interrupt());
    // Without a session the spills land in the temp dir rather than `~/.pi`.
    if keeps {
        ctx = ctx.with_session(&id);
    }

    // Always through the log: a loaded session whose view happens to be empty
    // still has history worth keeping, and `resume` handles an empty session.
    let mut session = carried;
    session.send_prompt(prompt, None);
    let outcome = ag.run(&mut session, &ctx, &tx, &retry).await;

    session.note_outcome(&outcome);

    drop(tx);
    let _ = painter.await;

    // Saved whichever way the run ended: an aborted turn is exactly the one
    // worth resuming. A one-shot has no session to save.
    if keeps {
        match store.save(&id, &root, &model_id, name.as_deref(), created, &session) {
            Ok(_) if !args.quiet => {
                let called = name.as_deref().map_or(String::new(), |n| format!(" “{n}”"));
                let carried = if resumed > 0 {
                    format!("{}resumed {resumed} messages", icons::PART_SEP)
                } else {
                    String::new()
                };
                eprintln!(
                    "session {id}{called}{carried} — continue with `pi -c` or `pi --resume {id}`"
                );
            }
            Err(e) => eprintln!("{}", core::not_saved(&e)),
            _ => {}
        }
    }

    // Above both ways out below: one exits the process outright, and a stopped
    // run is the one whose subagents were cut short with a save in flight.
    subagent::flush().await;

    // A run the user stopped is not a failure of the run; scripts should be
    // able to tell the two apart.
    if matches!(outcome, Err(agent::AgentError::Cancelled)) {
        std::process::exit(INTERRUPTED);
    }

    // Said only when there is something to diagnose. A successful run that
    // announced its journal would train everyone to stop reading the line.
    if let (Err(e), Some(path)) = (&outcome, journal::path()) {
        tracing::error!(target: "pi::loop", error = %e, "run failed");
        eprintln!(
            "\x1b[{}mjournal: {}\x1b[0m",
            config.theme.muted.codes(),
            path.display()
        );
    }
    outcome?;
    Ok(())
}
