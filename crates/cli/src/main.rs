use std::io::{IsTerminal as _, Read as _};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use brain::catalog::{
    Capabilities, ModelSpec, OpenAiCompat, Pricing, ThinkingReplay, ThinkingSupport, Wire,
};
use brain::request::Effort;
use brain::transport::{Transport, anthropic::Anthropic, openai::OpenAi};
use clap::{Parser, ValueEnum};
use tokio::sync::mpsc;

mod render;

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

    /// Catalog id, or the upstream model id together with --openai.
    #[arg(short, long, default_value = "opus-5")]
    model: String,

    /// Treat --model as an OpenAI-compatible endpoint rather than a catalog entry.
    #[arg(long)]
    openai: bool,

    /// Overrides the model's base url. Required with --openai.
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

    /// Replace the built-in system prompt.
    #[arg(long)]
    system: Option<String>,

    /// Answer only; no progress, no usage line.
    #[arg(short, long)]
    quiet: bool,
}

/// A model reached over an OpenAI-compatible endpoint, described entirely by
/// flags: local servers are not worth a catalog entry each.
fn ad_hoc_openai(args: &Args) -> Result<ModelSpec> {
    let base_url = args
        .base_url
        .clone()
        .context("--openai needs --base-url, e.g. http://localhost:8000/v1")?;
    Ok(ModelSpec {
        id: args.model.clone(),
        wire_id: args.model.clone(),
        base_url,
        wire: Wire::OpenAi(OpenAiCompat::default()),
        context_window: 128_000,
        max_output_tokens: 8_192,
        caps: Capabilities {
            tools: true,
            parallel_tool_calls: true,
            vision: false,
            thinking: Some(ThinkingSupport::Effort),
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

    let mut spec = if args.openai {
        ad_hoc_openai(&args)?
    } else {
        brain::catalog::find(&args.model).with_context(|| {
            format!(
                "unknown model `{}`; known: {}. Use --openai for anything else.",
                args.model,
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

    let workspace = tools::Workspace::new(&args.cwd)
        .with_context(|| format!("cannot use {} as a workspace", args.cwd))?;

    let mut registry = tools::Registry::builtin();
    if !args.tools.is_empty() {
        registry = registry.restrict(&args.tools).map_err(|bad| {
            anyhow::anyhow!(
                "no tool named `{bad}`; known: {}",
                tools::Registry::builtin().names().join(", ")
            )
        })?;
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

    let transport = transport_for(&spec)?;
    let mut ag = agent::Agent::new(transport, spec);
    ag.registry = registry;
    ag.approver = Arc::new(agent::Ceiling(tier));
    ag.system = system;
    ag.effort = effort;
    ag.max_turns = args.max_turns;

    let ctx = tools::Ctx {
        workspace,
        cancel: agent::cancel_on_interrupt(),
    };

    let (tx, mut rx) = mpsc::unbounded_channel();
    let quiet = args.quiet;
    let painter = tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet);
        while let Some(event) = rx.recv().await {
            r.on(event);
        }
        r.finish();
    });

    let mut session = agent::Session::with_prompt(prompt);
    let outcome = ag.run(&mut session, &ctx, &tx).await;

    drop(tx);
    let _ = painter.await;

    outcome?;
    Ok(())
}
