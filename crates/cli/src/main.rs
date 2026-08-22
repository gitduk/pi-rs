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
mod line;
mod render;
mod repl;
mod session;
mod tui;

/// The three below are both flags and config values, so a config file names a
/// tier the same way the command line does.
#[derive(Debug, Clone, Copy, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum WireArg {
    Anthropic,
    Openai,
}

/// Ordered, so a project file can lower the ceiling without being able to
/// raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum TierArg {
    Read,
    Write,
    Exec,
}

#[derive(Debug, Clone, Copy, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum EffortArg {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Parser, Debug)]
#[command(name = "pi", about = "A coding agent that stays inside one directory.")]
struct Args {
    /// The prompt. Reads stdin when omitted.
    prompt: Option<String>,

    /// Defaults and locally-defined models. Defaults to ~/.pi.toml.
    #[arg(long, value_name = "FILE", env = "PI_CONFIG")]
    config: Option<String>,

    /// A model defined in ~/.pi.toml, or the upstream id together with --wire.
    /// Defaults to the resumed session's model, else the config's.
    #[arg(short, long)]
    model: Option<String>,

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

    /// Defaults to 50.
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
}

/// A model described entirely by flags. Endpoints that mimic a known wire are
/// not worth a catalog entry each until their quirks have been measured.
fn ad_hoc(args: &Args, model: &str, wire: WireArg) -> Result<ModelSpec> {
    let base_url = args
        .base_url
        .clone()
        .context("--wire needs --base-url, e.g. --base-url http://localhost:8000/v1")?;
    let (wire, thinking) = match wire {
        WireArg::Anthropic => (
            Wire::Anthropic(AnthropicCompat::default()),
            Some(ThinkingSupport::Budget),
        ),
        WireArg::Openai => (
            Wire::OpenAi(OpenAiCompat::default()),
            Some(ThinkingSupport::Effort),
        ),
    };
    Ok(ModelSpec {
        id: model.to_string(),
        wire_id: model.to_string(),
        base_url,
        wire,
        context_window: 128_000,
        max_output_tokens: 8_192,
        caps: Capabilities {
            tools: true,
            parallel_tool_calls: true,
            vision: false,
            thinking,
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

/// Renders events by printing them, for every surface that is not the terminal
/// one. Its own task so a slow write never holds the run up.
fn paint(
    mut rx: mpsc::UnboundedReceiver<agent::Event>,
    quiet: bool,
    structured: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet, structured);
        while let Some(event) = rx.recv().await {
            r.on(event);
        }
        r.finish();
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let prompt = read_prompt(&args)?;
    let config = config::load(args.config.as_deref())?;

    let workspace = tools::Workspace::new(&args.cwd)
        .with_context(|| format!("cannot use {} as a workspace", args.cwd))?;
    let project = config::load_project(workspace.root())?;

    let store = session::Store::default();
    let prior = match (&args.resume, args.continue_last) {
        (Some(id), _) => Some(store.load(id)?),
        (None, true) => Some(store.latest(workspace.root())?),
        _ => None,
    };

    let named = config.model(
        &project,
        args.model.as_deref(),
        prior.as_ref().map(|p| p.model.as_str()),
    );

    let mut spec = match (args.wire, &named) {
        // A one-off against an endpoint not worth writing down yet.
        (Some(wire), Some((model, _))) => ad_hoc(&args, model, wire)?,
        (Some(_), None) => bail!("--wire needs -m to say what the endpoint calls the model"),
        (None, Some((model, named_by))) => match config.find(model) {
            Some((id, entry)) => entry.spec(id)?,
            None => {
                // The table name is the wire name, easy to miss when the two
                // were written at different times: say the edit, not just the
                // mismatch.
                match config.ids().as_slice() {
                    [] => bail!(
                        "unknown model `{model}`, named by {}, and ~/.pi.toml defines \
                         none — see examples/pi.toml.",
                        named_by.describe()
                    ),
                    [only] => bail!(
                        "unknown model `{model}`, named by {}; the only one defined is \
                         `{only}`. Rename [models.{only}] to [models.{model}], or point \
                         {} at `{only}`.",
                        named_by.describe(),
                        named_by.describe()
                    ),
                    ids => bail!(
                        "unknown model `{model}`, named by {}; defined: {}.",
                        named_by.describe(),
                        ids.join(", ")
                    ),
                }
            }
        },
        (None, None) => bail!(
            "no model to run. Define one in ~/.pi.toml — see examples/pi.toml — \
             or name an endpoint with --wire and --base-url."
        ),
    };
    let entry = named
        .as_ref()
        .and_then(|(m, _)| config.find(m))
        .map(|(_, e)| e);
    let model = spec.id.clone();
    if let Some(url) = &args.base_url {
        spec.base_url = url.clone();
    }
    if let Some(window) = args.context {
        spec.context_window = window;
    }
    // An ad-hoc spec is a guess. Saying which guess lets the user correct the
    // one that matters instead of debugging a 400 later.
    if args.wire.is_some() && !args.quiet {
        eprintln!(
            "\x1b[2massuming a {}-token window, {} max output, and no pricing for `{}`. \
             Set --context if the server's window differs.\x1b[0m",
            spec.context_window, spec.max_output_tokens, spec.id
        );
    }

    let mut registry = tools::Registry::builtin();
    if !args.tools.is_empty() {
        registry = registry.restrict(&args.tools).map_err(|bad| {
            anyhow::anyhow!(
                "no tool named `{bad}`; known: {}",
                tools::Registry::builtin().names().join(", ")
            )
        })?;
    }

    if !args.no_skills {
        let found = tools::skills::discover(workspace.root());
        // Said once, at startup: a skill that silently fails to appear is one
        // the user goes looking for in the wrong place.
        for problem in &found.problems {
            eprintln!("\x1b[2mskill skipped — {problem}\x1b[0m");
        }
        let tool = tools::skill::SkillTool::new(found.skills);
        if !tool.is_empty() {
            registry = registry.with(tool);
        }
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
        &project,
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
        let loaded = context::load(workspace.root());
        if !args.quiet && !loaded.files.is_empty() {
            let names: Vec<String> = loaded
                .files
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            let warn = if loaded.oversized() {
                format!(" — {}KB on every request", loaded.bytes / 1024)
            } else {
                String::new()
            };
            eprintln!("\x1b[2minstructions: {}{warn}\x1b[0m", names.join(", "));
        }
        system.push_str(&loaded.text);
    }

    // Captured before the spec and workspace move into the agent and context.
    let root = workspace.root().to_path_buf();
    let model_id = spec.id.clone();

    let key = match entry.and_then(config::Entry::key) {
        Some(k) => Some(k.with_context(|| format!("the key for `{model}`"))?),
        None => None,
    };
    if entry.is_some_and(|e| e.api_key.is_some())
        && let Some(path) = args
            .config
            .clone()
            .map(std::path::PathBuf::from)
            .or_else(config::global_path)
    {
        config::warn_if_exposed(&path);
    }
    let transport = transport_for(&spec, key)?;
    let mut ag = agent::Agent::new(transport, spec);
    ag.registry = registry;
    ag.approver = Arc::new(agent::Ceiling(tier));
    ag.system = system;
    ag.effort = effort;
    ag.max_turns = settled.max_turns;
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

    let id = prior
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(session::new_id);
    let carried = prior.map(|p| p.into_log()).unwrap_or_default();
    let resumed = carried.context().len();

    let Some(prompt) = prompt else {
        let core = repl::Repl {
            agent: ag,
            store,
            model: model_id,
            session: agent::Session { log: carried },
            id,
            ctx: tools::Ctx::new(workspace),
        };
        // The live region needs the terminal at both ends: keys come in one
        // side and the repaint goes out the other. Missing either, there is
        // nothing to hold still, and printing a line at a time is right.
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return tui::Tui::new(core)?.run(tx, rx).await;
        }
        let painter = paint(rx, quiet, structured);
        let out = line::run(core, tx).await;
        let _ = painter.await;
        return out;
    };

    let painter = paint(rx, quiet, structured);
    let ctx = tools::Ctx::new(workspace).with_cancel(agent::cancel_on_interrupt());

    // Always through the log: a loaded session whose view happens to be empty
    // still has history worth keeping, and `resume` handles an empty log.
    let mut session = agent::Session::resumed(carried, prompt);
    let outcome = ag.run(&mut session, &ctx, &tx).await;

    drop(tx);
    let _ = painter.await;

    // Saved whichever way the run ended: an aborted turn is exactly the one
    // worth resuming.
    match store.save(&id, &root, &model_id, &session.log) {
        Ok(_) if !args.quiet => {
            let carried = if resumed > 0 {
                format!(" · resumed {resumed} messages")
            } else {
                String::new()
            };
            eprintln!("session {id}{carried} — continue with `pi -c` or `pi --resume {id}`");
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

    outcome?;
    Ok(())
}
