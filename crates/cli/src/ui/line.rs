//! The surface for when there is no terminal to own.
//!
//! `pi -i < script` and `pi | tee log` both reach interactive mode without a
//! terminal worth repainting. There are no keys to read and no region to hold
//! still, so a line comes off stdin at a time and the renderer prints as it
//! does for a one-shot run.

use std::io::{BufRead, IsTerminal, Write};

use agent::{AgentError, Event};
use anyhow::Result;
use llm::stream::Usage;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::App;
use crate::app::meter::Rates;
use crate::input::Step;

pub async fn run(mut core: App, tx: UnboundedSender<Event>, rates: Rates) -> Result<()> {
    let mut buffer = String::new();

    // Worth writing when a person is watching stderr — `pi | tee` reaches here
    // with a terminal still on the other stream — and pure noise in a log.
    let prompt = std::io::stderr().is_terminal();

    loop {
        if prompt {
            eprint!("{} ", crate::store::icons::PIPE_SIGIL);
            let _ = std::io::stderr().flush();
        }
        buffer.clear();
        // Off the worker: `read_line` parks in the kernel for as long as the
        // pipe is quiet, and a worker parked there is one no turn can run on.
        let read = tokio::task::spawn_blocking({
            let mut buffer = std::mem::take(&mut buffer);
            move || {
                let n = std::io::stdin().lock().read_line(&mut buffer);
                (buffer, n)
            }
        })
        .await;
        let (read, eof) = read.map_err(|e| anyhow::anyhow!("the stdin reader stopped: {e}"))?;
        buffer = read;
        if eof? == 0 {
            break;
        }
        let line = buffer.trim();
        if line.is_empty() {
            continue;
        }

        match core.dispatch(crate::input::read(line, &core.commands)) {
            Step::Quit => break,
            Step::Bash(command) => {
                // Awaited in place: this surface has nothing else to serve
                // while it runs, where the TUI spawns it and keeps drawing.
                let ctx = core
                    .lane_mut()
                    .ctx
                    .clone()
                    .with_cancel(agent::cancel_on_interrupt());
                let out = crate::app::bash::run_bash(&ctx, &command).await;
                if let Some(session) = core.lane_mut().session.as_mut() {
                    crate::app::bash::record_bash(session, &command, out.text.clone());
                }
                if let Err(e) = core.save() {
                    eprintln!("warning: the transcript was not saved: {e}");
                }
                for line in out.screen() {
                    println!("{line}");
                }
            }
            // There is no bar row to hold it here, and nothing repaints: a
            // flash is simply printed, like every other answer on this surface.
            Step::Flash(line) => println!("{line}"),
            Step::Swap(lines) | Step::Handled(lines) | Step::Worktrees(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Step::Compact(focus) => match core.compact_now(focus.as_deref()).await {
                Some((report, spent)) => {
                    core.lane_mut().charge(&spent);

                    println!("compacted {} → {} tokens", report.before, report.after);
                    if let Err(e) = core.save() {
                        eprintln!("warning: the transcript was not saved: {e}");
                    }
                }
                None => {
                    let held = core.lane_mut().agent.kept_tokens();
                    let now = core.tokens_now();
                    println!(
                        "nothing to compact — {now} tokens, all inside the {held} kept as working context"
                    );
                }
            },
            Step::Prompt { send, typed } => {
                let spent = turn(&mut core, send, typed, &tx, &rates).await;
                core.lane_mut().charge(&spent);
            }
            Step::Wechat(_) => {
                println!("wechat needs a terminal to show the login QR — run pi in a terminal")
            }
        }
    }
    Ok(())
}

async fn turn(
    core: &mut App,
    prompt: String,
    typed: Option<String>,
    tx: &UnboundedSender<Event>,
    rates: &Rates,
) -> Usage {
    // The renderer owns the receiver, so it prices on a task of its own and
    // cannot ask the lane. Between runs is the only place this surface can
    // switch models — a line comes off stdin one at a time — so handing it the
    // rate here is the whole of keeping the two in step.
    rates.set(core.lane().agent.spec.pricing);
    // Lent for the length of the turn and put back after, the same shape the
    // terminal uses — here there is no loop to free, only one owner throughout.
    let Some(mut session) = core.lane_mut().session.take() else {
        return Usage::default();
    };
    session.send_prompt(prompt, typed, None);
    let ctx = core
        .lane_mut()
        .ctx
        .clone()
        .with_cancel(agent::cancel_on_interrupt());
    let out = core.lane_mut().agent.run(&mut session, &ctx, tx).await;

    session.note_outcome(&out);
    core.lane_mut().session = Some(session);

    // Saved either way: an interrupted turn is exactly the one worth keeping.
    if let Err(e) = core.save() {
        eprintln!("warning: the transcript was not saved: {e}");
    }

    match out {
        Ok(usage) => usage,
        Err(AgentError::Cancelled) => {
            eprintln!("stopped");
            Usage::default()
        }
        Err(e) => {
            eprintln!("error {e}");
            Usage::default()
        }
    }
}
