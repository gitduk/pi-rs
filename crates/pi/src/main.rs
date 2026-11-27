use std::io::{IsTerminal as _, Read as _};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;

use pi_core::args::Args;
use pi_core::core::dial::{dial, summary_writer};
use pi_core::core::resolve::{fence, resolve};
use pi_core::core::{self, Core, archive, lane, saves, worktree};
use pi_core::input::expand;
use pi_store::icons;
use pi_store::settings::Settings;
use pi_store::{diagnostics, session};
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

// A different pi installed: this process becomes it, on the sessions just
// left, each keeping its own model and name.
fn restart(
    core::Restart {
        lanes,
        front,
        flags,
    }: core::Restart,
) -> Result<ExitCode> {
    let launcher = core::update::launcher().context("cannot tell which pi to start again")?;
    let mut cmd = std::process::Command::new(launcher);
    cmd.args(pi_core::args::restart(&lanes, front, &flags))
        .env(UPDATED_FROM, env!("CARGO_PKG_VERSION"));
    let id = lanes[front].1.as_deref();
    tracing::info!(target: "pi::session", path = %launcher.display(), session = ?id, lanes = lanes.len(), "restarting");
    #[cfg(unix)]
    let e = std::os::unix::process::CommandExt::exec(&mut cmd);
    #[cfg(not(unix))]
    let e = std::io::Error::from(std::io::ErrorKind::Unsupported);
    let again = id.map_or("pi".to_string(), |id| format!("pi --resume {id}"));
    Err(e).with_context(|| format!("cannot start {} — run `{again}`", launcher.display()))
}

// What a restart into this pi says it did, from the version it left.
fn updated(from: Option<&str>) -> Option<String> {
    let now = env!("CARGO_PKG_VERSION");
    Some(match from? {
        was if was == now => format!("restarted into a new build of v{now}"),
        _ => format!("updated to v{now}"),
    })
}

// Prints a one-shot run's events on its own task. Delivery backpressures the
// run once output falls behind the bounded channel.
fn paint(
    mut rx: agent::event::Receiver,
    quiet: bool,
    theme: std::sync::Arc<pi_store::theme::Theme>,
    status: Vec<pi_store::statusline::Field>,
    model_name: String,
    meter: std::sync::Arc<llm::meter::Meter>,
    worktree: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut r = render::Renderer::new(quiet, theme, status, model_name, meter, worktree);
        while let Some(event) = rx.recv().await {
            r.on(event);
        }
        r.finish();
    })
}

// The version a pi restarting into this one ran, for this one to say so.
// The environment, not a flag: an older pi would refuse a flag it lacks.
const UPDATED_FROM: &str = "PI_UPDATED_FROM";

enum Ended {
    Exit(ExitCode),
    Restart(core::Restart),
}

fn main() -> Result<ExitCode> {
    let updated_from = std::env::var(UPDATED_FROM).ok();
    // SAFETY: no other thread exists yet to read the environment.
    unsafe { std::env::remove_var(UPDATED_FROM) };
    match drive(run(updated_from))? {
        Ended::Exit(code) => Ok(code),
        Ended::Restart(next) => restart(next),
    }
}

// Drop the runtime before acting on a restart: exec does not drop tasks,
// so their process-group guards would otherwise leave background commands alive.
fn drive(run: impl std::future::Future<Output = Result<Ended>>) -> Result<Ended> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run)
}

async fn run(updated_from: Option<String>) -> Result<Ended> {
    let args = Arc::new(Args::parse());
    // Before stdin: a machine with no settings yet has no prompt to read.
    if args.example {
        print!("{}", pi_store::config::EXAMPLE_SETTINGS);
        return Ok(Ended::Exit(ExitCode::SUCCESS));
    }
    match diagnostics::install(diagnostics::level_from_env()) {
        Ok(Some(path)) => eprintln!("diagnostics: {}", path.display()),
        Ok(None) => {}
        Err(error) => eprintln!("diagnostics unavailable: {error}"),
    }
    // A load outlives the run that made it, so it is filed where the pi root
    // keeps what crosses sessions.
    skills::delivered(pi_store::state::skill_loaded);
    let prompt = read_prompt(&args)?;
    let start = start(args, prompt.is_none()).await?;
    match prompt {
        None => interactive(start, updated_from).await,
        Some(prompt) => once(start, prompt).await,
    }
}

// Everything a run starts with, whichever surface it then runs on.
struct Start {
    args: Arc<Args>,
    settings: Settings,
    config: Arc<pi_store::config::Config>,
    catalog: core::catalog::Catalog,
    pinned: pi_core::args::Pinned,
    store: session::Store,
    workspace: tool::Workspace,
    worktree: Option<String>,
    // What this run keeps: an interactive session, or one the user named with
    // `-c`/`--resume`. A one-shot prompt keeps no session archive.
    keeps: bool,
    id: String,
    claim: Option<session::Claim>,
    name: Option<String>,
    // What a model called it, when the session came back from disk.
    title: Option<String>,
    created: u64,
    carried: agent::session::Session,
    // How many messages the session came back with.
    resumed: usize,
    words: &'static [pi_core::input::commands::Command],
    ag: agent::Agent,
    resolved: Arc<pi_core::core::resolve::Resolved>,
    mcp: core::mcp::Live,
}

// Prepare the execution context and claim any session kept by this process.
async fn start(args: Arc<Args>, interactive: bool) -> Result<Start> {
    let (workspace, settings, config) = open_workspace(&args)?;

    let store = session::Store::default();
    let (prior, mut claim) = take_prior(&args, &store, workspace.root())?;
    pi_store::memory::Memory::default().lift_projects();
    sweep(&store, config.keep(), prior.as_ref().map(|p| p.id.clone()));

    let keeps = prior.is_some() || interactive;

    // The one surface needs the terminal at both ends.
    if interactive && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!(
            "interactive mode needs a terminal on both stdin and stdout \
             — give a prompt to run once instead"
        );
    }

    let id = prior
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(session::new_id);
    if keeps && claim.is_none() {
        claim = Some(store.claim(workspace.root(), &id)?);
    }

    let mut pinned = args.pinned();
    pinned.skills = ui::skills::builtins();
    let from_project = settings.project_sets("model").is_some();
    let resume_model = prior.as_ref().map(|p| p.model.as_str());
    let catalog = core::catalog::Catalog::persistent();
    let config = Arc::new(catalog.enrich(&settings, &pinned, config)?);
    let (named, named_by) = config
        .model(args.model.as_deref(), resume_model, from_project)
        .ok_or_else(|| no_model(&args))?;
    // A session's model is a preference, not a fact: the connection it named
    // may be gone since. What `model` selects now stands in, and says so.
    let (dialled, replaced) = match dial(&pinned, &config, &named, named_by) {
        Err(why) if named_by == pi_store::config::ModelOrigin::Resumed => {
            let (other, by) = config.model(None, None, from_project).ok_or(why)?;
            let dialled = dial(&pinned, &config, &other, by)?;
            let said = format!("no longer reaching `{named}`; on `{other}` instead");
            (dialled, Some(said))
        }
        picked => (picked?, None),
    };
    // Only the terminal answers its own words; a one-shot run has none.
    let words: &'static [_] = if interactive { tui::WORDS } else { &[] };
    let mcp = core::mcp::Live::default();
    mcp.sync(&config, workspace.root());
    let mut resolved = resolve(&pinned, &workspace, &config, &settings, None, words, &mcp)?;
    resolved.notes.extend(replaced.clone());
    pi_core::core::warm_rtk();
    if interactive {
        // Said under the banner: printed here, they would wait behind the
        // screen and surface only at exit, after the session they were for.
        let said = dialled.warning.iter().chain(&dialled.assumed).cloned();
        resolved.notes.splice(0..0, said);
    } else {
        // Ahead of the quiet check on purpose: neither a key's exposure nor
        // which model runs is progress chatter.
        for line in dialled.warning.iter().chain(&replaced) {
            eprintln!("\x1b[{}m{line}\x1b[0m", config.theme.muted.codes());
        }
    }
    // Captured before the spec and workspace move into the agent and context.
    let root = workspace.root().to_path_buf();
    // Asked once: it shells out to git, and three startups of that was the
    // pause `Lists` exists to avoid.
    let worktree = worktree::current(&root);

    let mut ag = agent::Agent::new(dialled.transport, dialled.spec);
    if let Some(prior) = &prior {
        ag.meter.reset(prior.spent);
    }
    // Resolved here rather than lazily: a name that does not exist should be a
    // startup error, not a surprise once compaction first needs it.
    let writer = summary_writer(&pinned, &config)?;
    diagnostics::opening(
        &id,
        args.config.as_deref(),
        &config,
        settings.project(),
        workspace.root(),
        prior.as_ref(),
    );
    let retry = core::retry(&config);
    // Installed last: the compactor watches its stream by the idle timeout
    // `Config::retry` just settled. Without one the transcript is never shrunk.
    ag.compactor = Arc::new(agent::Summarizing::new(writer, retry.idle));
    let archive = if keeps {
        archive::Filed::armed(store.clone(), root.clone())
    } else {
        archive::nowhere()
    };
    // Armed here rather than field by field: the brief the lane keeps and the
    // one the agent runs on are the same value, subagent tool and all.
    let resolved = lane::arm(&mut ag, Arc::new(resolved), archive, retry, None);
    if !interactive && !args.quiet {
        for note in dialled.assumed.iter().chain(&resolved.notes) {
            let note: String = note
                .lines()
                .map(pi_store::text::screen_line)
                .collect::<Vec<_>>()
                .join("\n");
            eprintln!("\x1b[{}m{note}\x1b[0m", config.theme.muted.codes());
        }
    }

    // An explicit --name renames a resumed session; otherwise it keeps its own.
    let name = args
        .name
        .clone()
        .or_else(|| prior.as_ref().and_then(|p| p.name.clone()));
    // A title is the row's own, so it rides with the session it was written
    // for; the user's name, when there is one, is what shows instead.
    let title = prior.as_ref().and_then(|p| p.title.clone());
    let created = prior
        .as_ref()
        .map(|p| p.created)
        .unwrap_or_else(session::now);
    let carried = prior.map(|p| p.into_session()).unwrap_or_default();
    let resumed = carried.context().len();

    Ok(Start {
        args,
        settings,
        config,
        catalog,
        pinned,
        store,
        workspace,
        worktree,
        keeps,
        id,
        claim,
        name,
        title,
        created,
        carried,
        resumed,
        words,
        ag,
        resolved,
        mcp,
    })
}

// The workspace pi was pointed at, fenced by the tier the settings found from
// it may set: the project file is read between the two steps.
fn open_workspace(args: &Args) -> Result<(tool::Workspace, Settings, pi_store::config::Config)> {
    let within = || format!("cannot use {} as a workspace", args.cwd);
    let workspace = tool::Workspace::new(&args.cwd).with_context(within)?;
    let settings = Settings::load(args.config.as_deref(), workspace.root())?;
    let config = settings.config()?;
    let workspace = fence(workspace, &config, &args.pinned()).with_context(within)?;
    Ok((workspace, settings, config))
}

// The session `-c` or `--resume` names, if either does. The claim is held
// for the whole run: two pi saving one session would each erase what the
// other said.
fn take_prior(
    args: &Args,
    store: &session::Store,
    root: &std::path::Path,
) -> Result<(Option<session::Stored>, Option<session::Claim>)> {
    let taken = match (&args.resume, args.continue_last) {
        (Some(id), _) => Some(store.take(root, id)?),
        (None, true) => Some(store.take_latest(root)?),
        _ => None,
    };
    Ok(taken.unzip())
}

// Off the startup path: stats every bucket, almost never has anything to
// take. A run that exits first loses nothing — the next one sweeps.
fn sweep(store: &session::Store, keep: Option<std::time::Duration>, spare: Option<String>) {
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        // Session archives and their artifacts share the configured lifetime.
        store.prune();
        if let Some(keep) = keep {
            store.forget_older_than(keep, spare.as_deref());
            pi_store::forget_images(keep);
            pi_store::forget_spills(keep);
        }
    });
}

// No model named anywhere: where to name one.
fn no_model(args: &Args) -> anyhow::Error {
    let path = args
        .config
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(pi_store::config::global_path);
    let file = path.map_or_else(
        || "a config file".into(),
        |p| core::instructions::home_relative(&p),
    );
    let example = concat!(
        env!("CARGO_PKG_REPOSITORY"),
        "/blob/master/examples/settings.toml"
    );
    anyhow::anyhow!(
        "no model to run. Pass --model <provider/model-id> or set model in {file} — see {example}"
    )
}

async fn interactive(start: Start, updated_from: Option<String>) -> Result<Ended> {
    let Start {
        args,
        settings,
        config,
        catalog,
        pinned,
        store,
        workspace,
        worktree,
        keeps,
        id,
        claim,
        name,
        title,
        created,
        carried,
        words,
        ag,
        resolved,
        mcp,
        ..
    } = start;
    let environment =
        core::environment::Environment::new(config, settings, resolved.commands.clone(), mcp);
    // What the session's row last showed, filed beside its transcript.
    let kept = store.shown(workspace.root(), &id);
    let ctx = tool::Ctx::new(workspace).with_session(&id, pi_store::spill_root());
    let mut first = lane::Lane::opened(lane::Opening {
        id,
        created,
        name,
        title,
        worktree,
        claim,
        kept,
        ..lane::Opening::new(Arc::new(ag), resolved, ctx, environment)
    });
    first.return_session(carried);
    let mut core = Core {
        runtime: Default::default(),
        store,
        catalog,
        pinned,
        lanes: pi_core::core::lanes::Lanes::of(first),
        executions: None,
        services: None,
        naming: keeps,
        removals: Default::default(),
        trees: None,
        surface: words,
    };
    core.restore_messages()?;
    let drivers = pi_core::driver::Drivers::default();
    let services = drivers.services();
    services.arm();
    core.enable_background(drivers.background());
    core.enable_services(services);
    core.reopen(&args.lanes());
    core.revive_commands();
    core::update::watch();
    core::update::check(env!("CARGO_PKG_REPOSITORY"));
    let out = tui::Tui::new(core, drivers, updated(updated_from.as_deref()))?
        .run()
        .await;
    // Subagents handed their transcripts to a background save; wait for
    // those to land before the runtime goes with them.
    leave().await;
    match out? {
        tui::Ended::Quit => Ok(Ended::Exit(ExitCode::SUCCESS)),
        tui::Ended::Restart(next) => Ok(Ended::Restart(next)),
    }
}

async fn once(start: Start, prompt: String) -> Result<Ended> {
    let Start {
        args,
        config,
        store,
        workspace,
        worktree,
        keeps,
        id,
        claim,
        name,
        title,
        created,
        carried,
        resumed,
        mut ag,
        resolved,
        settings,
        catalog,
        pinned,
        words,
        mcp,
        ..
    } = start;
    // One request, so the servers get a moment to list their tools first.
    mcp.settled(std::time::Duration::from_secs(10)).await;

    // Skill names expand into input; host control words stay literal model input.
    let prompt = match expand(&resolved.commands, &prompt) {
        Some(Ok(instructions)) => instructions,
        Some(Err(why)) => bail!("{why}"),
        None => prompt.into(),
    };

    // Only the one-shot path prints what a run emits.
    let (tx, rx) = agent::event::channel();
    let painter = paint(
        rx,
        args.quiet,
        Arc::new(config.theme.clone()),
        config.status.to_vec(),
        ag.spec().wire_model().to_string(),
        ag.meter.clone(),
        worktree.clone(),
    );
    let mut ctx = tool::Ctx::new(workspace).with_cancel(cancel_on_interrupt());
    // Every run names its conversation, kept or not; one that keeps nothing
    // leaves its spills in the temp dir rather than `~/.pi`.
    let spills = if keeps {
        pi_store::spill_root()
    } else {
        ctx.spill_root().to_path_buf()
    };
    ctx = ctx.with_session(&id, spills);
    let stop = ctx.cancel.child_token();
    let _stop = stop.clone().drop_guard();
    ctx = ctx.with_cancel(stop.clone());
    let patch = Arc::make_mut(&mut ag.brief);
    patch.registry = core::shells::foreground(std::mem::take(&mut patch.registry));

    // A transient run uses an isolated archive, so long runs still have bounded
    // message lookup without creating a resumable session in the user's store.
    let temporary = (!keeps).then(tempfile::tempdir).transpose()?;
    let host_store = temporary
        .as_ref()
        .map_or(store, |dir| session::Store::new(dir.path().to_path_buf()));
    let mut first = lane::Lane::opened(lane::Opening {
        id: id.clone(),
        created,
        name: name.clone(),
        title,
        claim,
        worktree,
        ..lane::Opening::new(
            Arc::new(ag),
            resolved.clone(),
            ctx,
            core::environment::Environment::new(
                config.clone(),
                settings,
                resolved.commands.clone(),
                mcp,
            ),
        )
    });
    first.return_session(carried);
    let mut core = Core {
        runtime: Default::default(),
        store: host_store,
        catalog,
        pinned,
        lanes: pi_core::core::lanes::Lanes::of(first),
        executions: None,
        services: None,
        naming: keeps,
        removals: Default::default(),
        trees: None,
        surface: words,
    };
    core.restore_messages()?;
    let input = core
        .lane()
        .messages()
        .admit(pi_core::driver::Origin::Typed, prompt)?;
    let request =
        pi_core::execution::Request::new(pi_core::driver::Origin::Typed, core.lane().token());
    let started = core
        .start_model(input, request)
        .map_err(anyhow::Error::msg)?;
    for notice in started.notices {
        eprintln!("{notice}");
    }
    let outcome = loop {
        use pi_core::execution::host::{Event, Settlement};
        match core.next_runtime_event().await {
            Event::Agent { event, .. } => {
                let _ = tx.send(event).await;
            }
            Event::Settled(Settlement::Returned(settled)) => break settled.result,
            Event::Settled(Settlement::Removed(_)) => {
                break Err(pi_core::execution::RunError::Crashed);
            }
            Event::Rejected { message, .. } => {
                break Err(pi_core::execution::RunError::Failed(message));
            }
            Event::Deferred(deferred) => {
                tracing::warn!(target:"pi::session",lane=deferred.lane,message=%deferred.message,"background input deferred")
            }
            Event::Activity
            | Event::Started(_)
            | Event::Stopping { .. }
            | Event::Settled(Settlement::Ignored) => {}
        }
    };
    stop.cancel();
    core.stop_runtime();
    core.finish_runtime_shutdown("single run completed");
    if tokio::time::timeout(agent::STOP_GRACE, core.wait_background_shutdown())
        .await
        .is_err()
    {
        eprintln!(
            "some background executions did not stop in time; their last save is what is on disk"
        );
    }
    drop(tx);
    let _ = painter.await;
    if keeps {
        saves::flush().await;
        let saved: Result<()> = if core.lane().session().is_none() {
            Err(anyhow::anyhow!("session transcript could not be recovered"))
        } else {
            core.lane()
                .unsaved()
                .map_or(Ok(()), |why| Err(anyhow::anyhow!(why)))
        };
        // The row's name, as the session stands now: a `/name` or a title
        // that arrived while the run was going is what the file holds.
        let called = core.lane().called();
        say_saved(saved, &id, called.as_deref(), resumed, args.quiet);
    }
    // A stopped run is the one whose subagents were cut short with a save
    // in flight.
    leave().await;
    // A run the user stopped is not a failure of the run; scripts should be
    // able to tell the two apart.
    if outcome.as_ref().is_err_and(|error| error.is_cancelled()) {
        return Ok(Ended::Exit(ExitCode::from(INTERRUPTED)));
    }

    if let Err(error) = &outcome {
        tracing::error!(target: "pi::loop", session = %id, %error, "run failed");
    }
    outcome?;
    Ok(Ended::Exit(ExitCode::SUCCESS))
}

// How a one-shot's save went, and how to pick the session up again.
fn say_saved(saved: Result<()>, id: &str, name: Option<&str>, resumed: usize, quiet: bool) {
    match saved {
        Ok(()) if !quiet => {
            let called = name.map_or(String::new(), |n| format!(" “{n}”"));
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

// Settle durable work before reporting optional diagnostic output failures.
async fn leave() {
    settle().await;
    if let Some(why) = diagnostics::failure() {
        eprintln!("diagnostics stopped recording: {why}");
    }
}

// What leaving waits on: transcripts still being saved, and the last events
// of a run still with the hooks that watch them.
async fn settle() {
    saves::flush().await;
    core::hooks::flush().await;
}

#[cfg(test)]
mod tests;
