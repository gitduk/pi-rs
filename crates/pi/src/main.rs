use std::io::{IsTerminal as _, Read as _};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::sync::mpsc;

use pi_core::args::Args;
use pi_core::core::dial::{dial, summary_writer};
use pi_core::core::resolve::resolve;
use pi_core::core::{self, Core, lane, worktree};
use pi_core::input::expand;
use pi_store::icons;
use pi_store::settings::Settings;
use pi_store::{archive, journal, session};
use ui::{render, tui};

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

// Conventional exit code for a process killed by SIGINT.
const INTERRUPTED: u8 = 130;

// Ctrl-C stops the run, and pi then leaves the one way it always does.
// Later presses are swallowed: `ctrl_c` has replaced SIGINT's default.
fn cancel_on_interrupt() -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        child.cancel();
        eprintln!("\ninterrupting — saving what the run has done");
        while tokio::signal::ctrl_c().await.is_ok() {}
    });
    token
}

// Memory is distilled once pi has gone, by a process of its own, so leaving
// waits for nothing. In its own process group, a closed terminal misses it.
fn distill_after(args: &Args, ended: &str, model: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut child = std::process::Command::new(exe);
        child
            .args(["--distill", ended, "--model", model, "--cwd", &args.cwd])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if let Some(config) = &args.config {
            child.args(["--config", config]);
        }
        if let Some(url) = &args.base_url {
            child.args(["--base-url", url]);
        }
        if let Some(window) = args.context {
            child.args(["--context", &window.to_string()]);
        }
        if let Err(e) = child.spawn() {
            tracing::warn!(target: "pi::memory", error = %e, "distiller not started");
        }
    }
}

// The process `distill_after` starts. Nobody is watching it, so what happens
// goes to the ended session's journal and nowhere else.
async fn distill_alone(args: &Args, store: session::Store, ended: &str) -> Result<()> {
    let workspace = tool::Workspace::new(&args.cwd)?;
    let settings = Settings::load(args.config.as_deref(), workspace.root())?;
    let config = settings.config()?;
    let pinned = args.pinned();
    let from_project = settings.project_sets("model").is_some();
    let (named, by) = config
        .model(args.model.as_deref(), None, from_project)
        .context("no model to distill with")?;
    let dialled = dial(&pinned, &config, &named, by)?;
    let writer = summary_writer(&pinned, &config, &dialled.spec.model)?
        .unwrap_or((dialled.transport, dialled.spec));
    let memory = pi_store::memory::Memory::default();
    core::memory::distill(store, memory, writer, config.retry().idle, ended).await;
    Ok(())
}

// Renders a one-shot run's events by printing them. Its own task so a slow
// write never holds the run up.
fn paint(
    mut rx: mpsc::UnboundedReceiver<agent::Event>,
    quiet: bool,
    theme: std::sync::Arc<pi_store::theme::Theme>,
    status: Vec<pi_store::status::Segment>,
    model: String,
    pricing: llm::model::Pricing,
    worktree: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet, theme, status, model, pricing, worktree);
        while let Some(event) = rx.recv().await {
            r.on(event);
        }
        r.finish();
    })
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let args = std::sync::Arc::new(Args::parse());
    if let Some(ended) = &args.distill {
        let store = session::Store::default();
        let root = std::fs::canonicalize(&args.cwd).unwrap_or_else(|_| args.cwd.clone().into());
        journal::install(&store.journal_path(&root, ended), journal::level_from_env());
        return Ok(match distill_alone(&args, store, ended).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                tracing::error!(target: "pi::memory", error = %format!("{e:#}"), "distiller gave up");
                ExitCode::FAILURE
            }
        });
    }
    let prompt = read_prompt(&args)?;
    let within = || format!("cannot use {} as a workspace", args.cwd);
    let workspace = tool::Workspace::new(&args.cwd).with_context(within)?;
    // The project file is found from the workspace, and the write roots it may
    // name widen that same workspace: read between the two steps.
    let settings = Settings::load(args.config.as_deref(), workspace.root())?;
    let config = Arc::new(settings.config()?);
    let workspace = workspace
        .with_write_roots(&config.writable())
        .with_context(within)?;

    let store = session::Store::default();
    // Off the startup path: stats every bucket, almost never has anything to
    // take. A run that exits first loses nothing — the next one sweeps.
    tokio::task::spawn_blocking({
        let store = store.clone();
        move || {
            // One tree, two prune rules: transcripts by reach, journals by
            // age — a run worth reading back is a fortnight old at most.
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

    // The one surface needs the terminal at both ends. Asked before the
    // journal and session directory are made, so a failed start leaves neither.
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
        args.model.as_deref(),
        prior.as_ref().map(|p| p.model.as_str()),
        settings.project_sets("model").is_some(),
    ) else {
        bail!("no model to run. Define one in ~/.pi/settings.toml — see examples/pi.toml.");
    };
    let pinned = args.pinned();
    let dialled = dial(&pinned, &config, &named, named_by)?;

    let resolved = resolve(&pinned, &workspace, &config, &settings)?;
    pi_core::core::mcp::sync(&config, workspace.root());
    // Ahead of the quiet check on purpose: see `Dialled::warning`.
    if let Some(warning) = &dialled.warning {
        eprintln!("\x1b[{}m{warning}\x1b[0m", config.theme.muted.codes());
    }
    if !args.quiet {
        for note in dialled.assumed.iter().chain(&resolved.notes) {
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
    // startup error, not a surprise once compaction first needs it.
    let writer = summary_writer(&pinned, &config, &model_id)?;
    // A resumed session shares its journal file across runs. Installed after
    // every step that can refuse to start: a refused start leaves no directory.
    if keeps {
        journal::install(
            &store.journal_path(workspace.root(), &id),
            journal::level_from_env(),
        );
        journal::opening(
            &id,
            args.config.as_deref(),
            &config,
            settings.project(),
            workspace.root(),
            prior.as_ref(),
        );
    }
    let retry = config.retry();
    // Installed last: the compactor watches its stream by the idle timeout
    // `Config::retry` just settled. Without one the transcript is never shrunk.
    ag.compactor = Arc::new(agent::Summarizing::new(writer, retry.idle));
    let archive = if keeps {
        archive::Filed::armed(store.clone(), root.clone(), model_id.clone())
    } else {
        archive::nowhere()
    };
    // Armed here rather than field by field: the brief the lane keeps and the
    // one the agent runs on are the same value, subagent tool and all.
    let resolved = lane::arm(&mut ag, Arc::new(resolved), archive, retry, None);
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
        // Before `id` moves into the Core, since `ctx` still needs to borrow it.
        // `commands` is the Core's copy for the front lane; `resolved` keeps its own.
        let commands = resolved.commands.clone();
        let ended = id.clone();
        let ctx = tool::Ctx::new(workspace).with_session(&id, pi_store::spill_root());
        let mut first = lane::Lane::opened(lane::Opening {
            id,
            created,
            name,
            worktree,
            ..lane::Opening::new(Arc::new(ag), resolved, ctx)
        });
        first.return_session(carried);
        let mut core = Core {
            store,
            keys: key_map.clone(),
            config: config.clone(),
            pinned: pinned.clone(),
            commands,
            channels: Vec::new(),
            settings,
            current: 0,
            lanes: vec![first],
            refused: Default::default(),
            later: None,
        };
        let (saved, keep) = pi_store::private_file("wechat.json");
        let channels: Vec<Arc<dyn channel::Channel>> = vec![Arc::new(wechat::WeChat::new(
            saved.as_deref(),
            Arc::new(keep),
        ))];
        core.add_channels(&channels);
        let drivers = pi_core::driver::Drivers::new(channels);
        core.enable_later(drivers.later());
        let out = tui::Tui::new(core, key_map, drivers)?.run().await;
        // Subagents handed their transcripts to a background save; wait
        // for those to land before the runtime goes with them.
        archive::flush().await;
        distill_after(&args, &ended, &model_id);
        return out.map(|()| ExitCode::SUCCESS);
    };

    // One request, so the servers get a moment to list their tools first.
    pi_core::core::mcp::settled(std::time::Duration::from_secs(10)).await;

    // A skill command is a prompt, so it means the same here as at the
    // terminal; built-ins aren't — they operate on a session this run has none of.
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
        config.status.to_vec(),
        model_id.clone(),
        ag.spec().pricing,
        worktree.clone(),
    );
    let mut ctx = tool::Ctx::new(workspace).with_cancel(cancel_on_interrupt());
    // Without a session the spills land in the temp dir rather than `~/.pi`.
    if keeps {
        ctx = ctx.with_session(&id, pi_store::spill_root());
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

    // A stopped run is the one whose subagents were cut short with a save
    // in flight.
    archive::flush().await;
    if keeps {
        distill_after(&args, &id, &model_id);
    }

    // A run the user stopped is not a failure of the run; scripts should be
    // able to tell the two apart.
    if matches!(outcome, Err(agent::AgentError::Cancelled)) {
        return Ok(ExitCode::from(INTERRUPTED));
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
    Ok(ExitCode::SUCCESS)
}
