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

mod render;
mod session;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WireArg {
    Anthropic,
    Openai,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum TierArg {
    Read,
    Write,
    Exec,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EffortArg {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Parser, Debug)]
#[command(
    name = "pir",
    about = "A coding agent that stays inside one directory."
)]
struct Args {
    /// The prompt. Reads stdin when omitted.
    prompt: Option<String>,

    /// Catalog id, or the upstream id together with --wire. Defaults to the
    /// resumed session's model, else opus-5.
    #[arg(short, long)]
    model: Option<String>,

    /// Continue a saved session by id.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,

    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,

    /// Treat --model as an upstream id on this wire rather than a catalog entry.
    #[arg(long, value_enum)]
    wire: Option<WireArg>,

    /// Overrides the model's base url. Required with --wire.
    #[arg(long)]
    base_url: Option<String>,

    /// Directory the agent may touch. Nothing outside it is reachable.
    #[arg(short = 'C', long, default_value = ".")]
    cwd: String,

    /// Highest tool tier this run may use.
    #[arg(long, value_enum, default_value = "exec")]
    tier: TierArg,

    /// Restrict the tool set, e.g. --tools read,bash
    #[arg(long, value_delimiter = ',')]
    tools: Vec<String>,

    #[arg(long, value_enum, default_value = "off")]
    effort: EffortArg,

    #[arg(long, default_value_t = 50)]
    max_turns: usize,

    /// Override the model's context window. Useful against a proxy whose real
    /// window is smaller than the catalog claims.
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

fn transport_for(spec: &ModelSpec) -> Result<Arc<dyn Transport>> {
    match spec.wire {
        Wire::Anthropic(_) => {
            let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY is not set")?;
            Ok(Arc::new(Anthropic::new(key)))
        }
        Wire::OpenAi(_) => {
            // A local server usually wants no key at all.
            let key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "sk-none".into());
            Ok(Arc::new(OpenAi::new(key)))
        }
    }
}

fn read_prompt(args: &Args) -> Result<String> {
    if let Some(p) = &args.prompt {
        return Ok(p.clone());
    }
    // Without this, a bare `pir` in a terminal blocks on a tty nobody is typing into.
    if std::io::stdin().is_terminal() {
        bail!("no prompt given; pass one as an argument or pipe it in");
    }
    let mut body = String::new();
    std::io::stdin().read_to_string(&mut body)?;
    if body.trim().is_empty() {
        bail!("no prompt given, and stdin was empty");
    }
    Ok(body)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let prompt = read_prompt(&args)?;

    let workspace = tools::Workspace::new(&args.cwd)
        .with_context(|| format!("cannot use {} as a workspace", args.cwd))?;

    let store = session::Store::default();
    let prior = match (&args.resume, args.continue_last) {
        (Some(id), _) => Some(store.load(id)?),
        (None, true) => Some(store.latest(workspace.root())?),
        _ => None,
    };

    // An explicit -m wins; otherwise a resumed run stays on the model that
    // produced the transcript, whose reasoning blocks only replay to itself.
    let model = args
        .model
        .clone()
        .or_else(|| prior.as_ref().map(|p| p.model.clone()))
        .unwrap_or_else(|| "opus-5".to_string());

    let mut spec = if let Some(wire) = args.wire {
        ad_hoc(&args, &model, wire)?
    } else {
        brain::catalog::find(&model).with_context(|| {
            format!(
                "unknown model `{model}`; known: {}. Use --wire for anything else.",
                brain::catalog::builtin()
                    .iter()
                    .map(|m| m.id.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?
    };
    if let Some(url) = &args.base_url {
        spec.base_url = url.clone();
    }
    if let Some(window) = args.context {
        spec.context_window = window;
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

    if let Some(path) = &args.schema {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("cannot read schema {path}"))?;
        let schema: serde_json::Value =
            serde_json::from_str(&body).with_context(|| format!("{path} is not valid JSON"))?;
        tools::finish::check(&schema).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
        registry = registry.with(tools::finish::Yield::new(schema));
    }

    let tier = match args.tier {
        TierArg::Read => tools::Tier::Read,
        TierArg::Write => tools::Tier::Write,
        TierArg::Exec => tools::Tier::Exec,
    };
    let effort = match args.effort {
        EffortArg::Off => Effort::Off,
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
    };
    let system = match &args.system {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read system prompt {path}"))?,
        None => agent::DEFAULT_SYSTEM.to_string(),
    };

    // Captured before the spec and workspace move into the agent and context.
    let root = workspace.root().to_path_buf();
    let model_id = spec.id.clone();

    let transport = transport_for(&spec)?;
    let mut ag = agent::Agent::new(transport, spec);
    ag.registry = registry;
    ag.approver = Arc::new(agent::Ceiling(tier));
    ag.system = system;
    ag.effort = effort;
    ag.max_turns = args.max_turns;
    if args.no_compact {
        ag.compaction = None;
    }
    ag.summarize = !args.no_summary;
    ag.retry.attempts = args.retries;
    ag.retry.idle = std::time::Duration::from_secs(args.idle_timeout.max(1));
    if args.schema.is_some() {
        ag.finish_tool = Some(tools::finish::NAME.to_string());
    }

    let ctx = tools::Ctx {
        workspace,
        cancel: agent::cancel_on_interrupt(),
        todos: Default::default(),
        yielded: Default::default(),
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let quiet = args.quiet;
    let structured = args.schema.is_some();
    let painter = tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet, structured);
        while let Some(event) = rx.recv().await {
            r.on(event);
        }
        r.finish();
    });

    let id = prior
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(session::new_id);
    let carried = prior.map(|p| p.into_log()).unwrap_or_default();
    let resumed = carried.context().len();
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
            eprintln!("session {id}{carried} — continue with `pir -c` or `pir --resume {id}`");
        }
        Err(e) => eprintln!("warning: the transcript was not saved: {e}"),
        _ => {}
    }

    // stdout carries the result and nothing else, so it pipes into jq.
    if let Some(value) = outcome.as_ref().ok().and_then(|o| o.yielded.as_ref()) {
        println!("{}", serde_json::to_string_pretty(value)?);
    }

    outcome?;
    Ok(())
}
