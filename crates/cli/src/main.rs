use std::io::{IsTerminal as _, Read as _};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use brain::catalog::{
    AnthropicCompat, Capabilities, ModelSpec, OpenAiCompat, Pricing, ThinkingReplay,
    ThinkingSupport, Wire,
};
use brain::request::Effort;
use brain::transport::{Transport, anthropic::Anthropic, openai::OpenAi};
use clap::{Parser, ValueEnum};
use tokio::sync::mpsc;

mod config;
mod context;
mod journal;
mod keys;
mod line;
mod render;
mod repl;
mod session;
mod tui;

/// The three below are both flags and config values, so a config file names a
/// tier the same way the command line does.
#[derive(Debug, Clone, Copy, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireArg {
    Anthropic,
    Openai,
}

impl WireArg {
    /// The wire this names, carrying the protocol's own defaults for its
    /// quirks. A config entry that states quirks builds its own instead.
    fn wire(self) -> Wire {
        match self {
            WireArg::Anthropic => Wire::Anthropic(AnthropicCompat::default()),
            WireArg::Openai => Wire::OpenAi(OpenAiCompat::default()),
        }
    }

    /// Delegated rather than matched again, so a model `/model` lists and one
    /// it has just switched to cannot print two names for the same protocol.
    pub fn transport_name(self) -> &'static str {
        self.wire().transport_name()
    }
}

/// Ordered, so a project file can lower the ceiling without being able to
/// raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TierArg {
    Read,
    Write,
    Exec,
}

#[derive(Debug, Clone, Copy, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortArg {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Parser, Debug)]
#[command(name = "pi", about = "A coding agent that stays inside one directory.")]
pub struct Args {
    /// The prompt. Reads stdin when omitted.
    prompt: Option<String>,

    /// Defaults and locally-defined models. Defaults to ~/.pi.toml.
    #[arg(long, value_name = "FILE", env = "PI_CONFIG")]
    config: Option<String>,

    /// A model defined in ~/.pi.toml, or the upstream id together with --wire.
    /// Defaults to the resumed session's model, else the config's.
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

    /// Treat --model as the endpoint's own id on this wire, for a one-off
    /// against a server not worth writing into the config yet.
    #[arg(long, value_enum)]
    wire: Option<WireArg>,

    /// Overrides the model's base url. Required with --wire.
    #[arg(long)]
    base_url: Option<String>,

    /// Directory the agent may touch. Nothing outside it is reachable.
    #[arg(short = 'C', long, default_value = ".")]
    cwd: String,

    /// Highest tool tier this run may use. Defaults to exec.
    #[arg(long, value_enum)]
    tier: Option<TierArg>,

    /// Restrict the tool set, e.g. --tools read,bash
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,

    #[arg(long, value_enum)]
    effort: Option<EffortArg>,

    /// Defaults to 100.
    #[arg(long)]
    max_turns: Option<usize>,

    /// Override the model's context window, for a proxy whose real window is
    /// smaller than the config says.
    #[arg(long, value_name = "TOKENS")]
    context: Option<u32>,

    /// Send the transcript untouched and let the provider refuse it.
    #[arg(long)]
    no_compact: bool,

    /// Drop old history outright instead of summarizing it first.
    #[arg(long)]
    no_summary: bool,

    /// How many times to retry a request the provider could not serve.
    #[arg(long, default_value_t = 4)]
    retries: usize,

    /// Give up on a stream that has sent nothing for this long.
    #[arg(long, value_name = "SECONDS", default_value_t = 300)]
    idle_timeout: u64,

    /// Replace the built-in system prompt.
    #[arg(long)]
    system: Option<String>,

    /// Keep the conversation open instead of running once. Implied by a bare
    /// `pi` at a terminal.
    #[arg(short, long)]
    interactive: bool,

    /// Ignore the skills on disk.
    #[arg(long)]
    no_skills: bool,

    /// Ignore ~/.pi.md and the project's AGENTS.md.
    #[arg(long)]
    no_context_files: bool,

    /// A JSON Schema file. The run must end by calling `yield` with a matching
    /// object, which is printed to stdout instead of prose.
    #[arg(long, value_name = "FILE")]
    schema: Option<String>,

    /// Answer only; no progress, no usage line.
    #[arg(short, long)]
    quiet: bool,

    /// How much this run writes to its journal. `debug` adds the payloads —
    /// request bodies, patches, tool arguments in full. `off` writes nothing.
    #[arg(
        long,
        value_name = "LEVEL",
        value_enum,
        env = "PI_LOG",
        default_value = "info"
    )]
    log: journal::LogLevel,
}

/// A model described entirely by flags. Endpoints that mimic a known wire are
/// not worth a catalog entry each until their quirks have been measured.
fn ad_hoc(args: &Args, model: &str, wire: WireArg) -> Result<ModelSpec> {
    let base_url = args
        .base_url
        .clone()
        .context("--wire needs --base-url, e.g. --base-url http://localhost:8000/v1")?;
    let thinking = match wire {
        WireArg::Anthropic => ThinkingSupport::Budget,
        WireArg::Openai => ThinkingSupport::Effort,
    };
    Ok(ModelSpec {
        id: model.to_string(),
        wire_id: model.to_string(),
        base_url,
        wire: wire.wire(),
        context_window: 128_000,
        max_output_tokens: 8_192,
        caps: Capabilities {
            tools: true,
            parallel_tool_calls: true,
            vision: false,
            thinking: Some(thinking),
            cache_breakpoints: false,
        },
        thinking_replay: ThinkingReplay::Tagged,
        pricing: Pricing::default(),
    })
}

/// `configured` is what the config entry supplied, if the model came from one.
fn transport_for(spec: &ModelSpec, configured: Option<String>) -> Result<Arc<dyn Transport>> {
    match spec.wire {
        Wire::Anthropic(_) => {
            let key = match configured {
                Some(k) => k,
                None => {
                    std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?
                }
            };
            Ok(Arc::new(Anthropic::new(key)))
        }
        Wire::OpenAi(_) => {
            // A local server usually wants no key at all.
            let key = configured
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .unwrap_or_else(|| "sk-none".into());
            Ok(Arc::new(OpenAi::new(key)))
        }
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

/// The table name is the wire name, easy to miss when the two were written at
/// different times: say the edit, not just the mismatch.
fn unknown(config: &config::Config, model: &str, named_by: config::Origin) -> anyhow::Error {
    let by = named_by.describe();
    match config.ids().as_slice() {
        [] => anyhow::anyhow!(
            "unknown model `{model}`, named by {by}, and ~/.pi.toml defines none — \
             see examples/pi.toml."
        ),
        [only] => anyhow::anyhow!(
            "unknown model `{model}`, named by {by}; the only one defined is `{only}`. \
             Rename [models.{only}] to [models.{model}], or point {by} at `{only}`."
        ),
        ids => anyhow::anyhow!(
            "unknown model `{model}`, named by {by}; defined: {}.",
            ids.join(", ")
        ),
    }
}

/// Resolve a model name into something that can be talked to.
///
/// Startup and `/model` share this so the two cannot decide differently about
/// the same name. `--wire` short-circuits the config in both: it says the
/// command line has named one endpoint and one protocol for the whole run, and
/// a config entry of the same name would name a different server. Both cannot
/// hold, and the flag wins here as flags do everywhere else.
pub fn dial(
    args: &Args,
    config: &config::Config,
    model: &str,
    named_by: config::Origin,
) -> Result<Dialled> {
    let entry = args.wire.is_none().then(|| config.find(model)).flatten();
    let mut spec = match (args.wire, entry) {
        (Some(wire), _) => ad_hoc(args, model, wire)?,
        (None, Some((id, e))) => e.spec(id)?,
        (None, None) => return Err(unknown(config, model, named_by)),
    };
    if let Some(url) = &args.base_url {
        spec.base_url = url.clone();
    }
    if let Some(window) = args.context {
        spec.context_window = window;
    }

    let mut notes = Vec::new();
    // An ad-hoc spec is a guess. Saying which guess lets the user correct the
    // one that matters instead of debugging a 400 later.
    if args.wire.is_some() {
        notes.push(format!(
            "assuming a {}-token window, {} max output, and no pricing for `{}`. \
             Set --context if the server's window differs.",
            spec.context_window, spec.max_output_tokens, spec.id
        ));
    }

    // The table name was only needed to build the spec; what is left is the
    // entry itself.
    let entry = entry.map(|(_, e)| e);
    let key = match entry.and_then(config::Entry::key) {
        Some(k) => Some(k.with_context(|| format!("the key for `{model}`"))?),
        None => None,
    };
    let warning = entry
        .filter(|e| e.api_key.is_some())
        .and_then(|_| {
            args.config
                .clone()
                .map(std::path::PathBuf::from)
                .or_else(config::global_path)
        })
        .and_then(|path| config::warn_if_exposed(&path));

    let transport = transport_for(&spec, key)?;
    Ok(Dialled {
        spec,
        transport,
        notes,
        warning,
    })
}

/// The prompt, or None when the run should ask for one.
fn read_prompt(args: &Args) -> Result<Option<String>> {
    if let Some(p) = &args.prompt {
        return Ok(Some(p.clone()));
    }
    // A bare `pi` at a terminal means "talk to me"; piped in, it means the
    // prompt is on stdin.
    if args.interactive || std::io::stdin().is_terminal() {
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
pub struct Resolved {
    pub registry: tools::Registry,
    pub system: String,
    pub tier: tools::Tier,
    pub effort: Effort,
    pub max_turns: usize,
    pub keys: keys::Keys,
    /// The built-ins plus one command per skill. Here rather than in the Repl
    /// because a skill discovered at reload has to reach the prompt the same
    /// way everything else the config decides does.
    pub commands: Vec<repl::Command>,
    /// Worth saying once, at startup and at each reload.
    pub notes: Vec<String>,
}

/// Fails whole or not at all. A half-applied config is worse than a stale one,
/// which is why `/reload` computes all of this before touching anything.
pub fn resolve(
    args: &Args,
    root: &std::path::Path,
    config: &config::Config,
    project: &config::Project,
) -> Result<Resolved> {
    let mut notes = Vec::new();

    let mut registry = tools::Registry::builtin();
    if !args.tools.is_empty() {
        registry = registry.restrict(&args.tools).map_err(|bad| {
            anyhow::anyhow!(
                "no tool named `{bad}`; known: {}",
                tools::Registry::builtin().names().join(", ")
            )
        })?;
    }
    let skills = if args.no_skills {
        Vec::new()
    } else {
        let found = tools::skills::discover(root);
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
    let commands = repl::commands(&skills, &mut notes);
    let tool = tools::skill::SkillTool::new(skills);
    if !tool.is_empty() {
        registry = registry.with(tool);
    }
    if let Some(path) = &args.schema {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("cannot read schema {path}"))?;
        let schema: serde_json::Value =
            serde_json::from_str(&body).with_context(|| format!("{path} is not valid JSON"))?;
        tools::finish::check(&schema).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        registry = registry.with(tools::finish::Yield::new(schema));
    }

    let settled = config.settle(
        &project.clone(),
        config::Flags {
            effort: args.effort,
            tier: args.tier,
            max_turns: args.max_turns,
        },
    );
    let tier = match settled.tier {
        TierArg::Read => tools::Tier::Read,
        TierArg::Write => tools::Tier::Write,
        TierArg::Exec => tools::Tier::Exec,
    };
    let effort = match settled.effort {
        EffortArg::Off => Effort::Off,
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
    };

    let mut system = match args.system.as_ref().or(config.defaults.system.as_ref()) {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read system prompt {path}"))?,
        None => agent::DEFAULT_SYSTEM.to_string(),
    };
    // Appended rather than sent as a message: these are standing instructions,
    // they do not change within a run, and the system prompt is the part of the
    // request a provider will cache.
    if !args.no_context_files {
        let loaded = context::load(root);
        system.push_str(&loaded.text);
    }

    Ok(Resolved {
        registry,
        system,
        tier,
        effort,
        max_turns: settled.max_turns,
        keys: config.key_map()?,
        commands,
        notes,
    })
}

/// Renders events by printing them, for every surface that is not the terminal
/// one. Its own task so a slow write never holds the run up.
fn paint(
    mut rx: mpsc::UnboundedReceiver<agent::Event>,
    quiet: bool,
    structured: bool,
    theme: std::sync::Arc<render::Theme>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet, structured, theme);
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

    let workspace = tools::Workspace::new(&args.cwd)
        .with_context(|| format!("cannot use {} as a workspace", args.cwd))?;
    let project = config::load_project(workspace.root())?;

    let store = session::Store::default();
    let prior = match (&args.resume, args.continue_last) {
        (Some(id), _) => Some(store.load(id)?),
        (None, true) => Some(store.latest(workspace.root())?),
        _ => None,
    };

    // A resumed session keeps its journal too, so the whole of it reads as one
    // file however many runs it took.
    let id = prior
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(session::new_id);
    journal::install(&id, args.log);
    journal::opening(
        &id,
        &args,
        &config,
        &project,
        workspace.root(),
        prior.as_ref(),
    );

    let Some((named, named_by)) = config.model(
        &project,
        args.model.as_deref(),
        prior.as_ref().map(|p| p.model.as_str()),
    ) else {
        if args.wire.is_some() {
            bail!("--wire needs -m to say what the endpoint calls the model");
        }
        bail!(
            "no model to run. Define one in ~/.pi.toml — see examples/pi.toml — \
             or name an endpoint with --wire and --base-url."
        );
    };
    let dialled = dial(&args, &config, &named, named_by)?;

    let resolved = resolve(&args, workspace.root(), &config, &project)?;
    // Ahead of the quiet check on purpose: see `Dialled::warning`.
    if let Some(warning) = &dialled.warning {
        eprintln!("\x1b[{}m{warning}\x1b[0m", config.theme.muted.codes());
    }
    if !args.quiet {
        for note in dialled.notes.iter().chain(&resolved.notes) {
            eprintln!("\x1b[{}m{note}\x1b[0m", config.theme.muted.codes());
        }
    }
    let key_map = std::sync::Arc::new(resolved.keys);

    // Captured before the spec and workspace move into the agent and context.
    let root = workspace.root().to_path_buf();
    let model_id = dialled.spec.id.clone();

    let mut ag = agent::Agent::new(dialled.transport, dialled.spec);
    ag.registry = resolved.registry;
    ag.approver = Arc::new(agent::Ceiling(resolved.tier));
    ag.system = resolved.system;
    ag.effort = resolved.effort;
    ag.max_turns = resolved.max_turns;
    if args.no_compact {
        ag.compaction = None;
    }
    ag.summarize = !args.no_summary;
    ag.retry.attempts = args.retries;
    ag.retry.idle = std::time::Duration::from_secs(args.idle_timeout.max(1));
    if args.schema.is_some() {
        ag.finish_tool = Some(tools::finish::NAME.to_string());
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let quiet = args.quiet;
    let structured = args.schema.is_some();

    // An explicit --name renames a resumed session; otherwise it keeps its own.
    let name = args
        .name
        .clone()
        .or_else(|| prior.as_ref().and_then(|p| p.name.clone()));
    let carried = prior.map(|p| p.into_log()).unwrap_or_default();
    let resumed = carried.context().len();

    let Some(prompt) = prompt else {
        // Before `id` moves into the Repl: the context borrows it to name the
        // session its spills belong to.
        let ctx = tools::Ctx::new(workspace).with_session(&id);
        let core = repl::Repl {
            agent: ag,
            store,
            session: agent::Session { log: carried },
            id,
            name,
            keys: key_map.clone(),
            config: config.clone(),
            args: args.clone(),
            commands: std::sync::Arc::new(resolved.commands),
            ctx,
        };
        // The live region needs the terminal at both ends: keys come in one
        // side and the repaint goes out the other. Missing either, there is
        // nothing to hold still, and printing a line at a time is right.
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return tui::Tui::new(core, key_map)?.run(tx, rx).await;
        }
        let painter = paint(
            rx,
            quiet,
            structured,
            std::sync::Arc::new(config.theme.clone()),
        );
        let out = line::run(core, tx).await;
        let _ = painter.await;
        return out;
    };

    // A skill command is a prompt, so it means here what it means at the
    // terminal. The built-ins are not: they operate on a session, and a run
    // that answers once has none to operate on.
    let prompt = match repl::expand(&resolved.commands, &prompt) {
        Some(Ok(instructions)) => instructions,
        Some(Err(why)) => bail!("{why}"),
        None => prompt,
    };

    let painter = paint(
        rx,
        quiet,
        structured,
        std::sync::Arc::new(config.theme.clone()),
    );
    let ctx = tools::Ctx::new(workspace)
        .with_session(&id)
        .with_cancel(agent::cancel_on_interrupt());

    // Always through the log: a loaded session whose view happens to be empty
    // still has history worth keeping, and `resume` handles an empty log.
    let mut session = agent::Session::resumed(carried, prompt);
    let outcome = ag.run(&mut session, &ctx, &tx).await;

    drop(tx);
    let _ = painter.await;

    // Saved whichever way the run ended: an aborted turn is exactly the one
    // worth resuming.
    match store.save(&id, &root, &model_id, name.as_deref(), &session.log) {
        Ok(_) if !args.quiet => {
            let called = name.as_deref().map_or(String::new(), |n| format!(" “{n}”"));
            let carried = if resumed > 0 {
                format!(" · resumed {resumed} messages")
            } else {
                String::new()
            };
            eprintln!(
                "session {id}{called}{carried} — continue with `pi -c` or `pi --resume {id}`"
            );
        }
        Err(e) => eprintln!("warning: the transcript was not saved: {e}"),
        _ => {}
    }

    // stdout carries the result and nothing else, so it pipes into jq.
    if let Some(value) = outcome.as_ref().ok().and_then(|o| o.yielded.as_ref()) {
        println!("{}", serde_json::to_string_pretty(value)?);
    }

    // A run the user stopped is not a failure of the run; scripts should be
    // able to tell the two apart.
    if matches!(outcome, Err(agent::AgentError::Cancelled)) {
        std::process::exit(130);
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
