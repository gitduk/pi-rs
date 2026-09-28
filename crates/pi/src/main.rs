use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use tokio::sync::mpsc;

use crate::args::Args;
use crate::core::dial::{dial, summary_writer};
use crate::core::resolve::resolve;
use crate::core::{Core, lane, worktree};
use crate::input::expand;
use crate::store::icons;
use crate::store::settings::Settings;
use crate::store::{config, home, journal, session};
use crate::ui::{render, tui};

mod args;
mod core;
mod driver;
mod input;
mod store;
mod ui;

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
    let pinned = args.pinned();
    let dialled = dial(&pinned, &config, &named, named_by)?;

    let resolved = resolve(&pinned, &workspace, &config, &project, &BTreeMap::new())?;
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
    // startup error, not a surprise the first time history gets long enough to
    // compact.
    let writer = summary_writer(&pinned, &config, &model_id)?;
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
        home::Filed::armed(store.clone(), root.clone(), model_id.clone())
    } else {
        home::nowhere()
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
        let ctx = tool::Ctx::new(workspace).with_session(&id, store::spill_root());
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
            pinned: pinned.clone(),
            commands,
            settings: Settings::new(
                config::load_tree(args.config.as_deref())
                    .unwrap_or_else(|_| toml::Value::Table(Default::default())),
            ),
            current: 0,
            lanes: vec![first],
        };
        let drivers = driver::Drivers::new(vec![Arc::new(wechat::WeChat::new(
            store::dir().map(|d| d.join("wechat.json")),
        ))]);
        let out = tui::Tui::new(core, key_map, drivers)?.run().await;
        // Subagents handed their transcripts to a background save; wait
        // for those to land before the runtime goes with them.
        home::flush().await;
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
        ctx = ctx.with_session(&id, store::spill_root());
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
    home::flush().await;

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
