//! The surface for when there is no terminal to own.
//!
//! `pi -i < script` and `pi | tee log` both reach interactive mode without a
//! terminal worth repainting. There are no keys to read and no region to hold
//! still, so a line comes off stdin at a time and the renderer prints as it
//! does for a one-shot run.

use std::io::{BufRead, IsTerminal, Write};

use agent::{AgentError, Event, Totals};
use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::repl::{Repl, Step};

pub async fn run(mut core: Repl, tx: UnboundedSender<Event>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut totals = Totals::default();
    let mut buffer = String::new();

    // Worth writing when a person is watching stderr — `pi | tee` reaches here
    // with a terminal still on the other stream — and pure noise in a log.
    let prompt = std::io::stderr().is_terminal();

    loop {
        if prompt {
            eprint!("› ");
            let _ = std::io::stderr().flush();
        }
        buffer.clear();
        if stdin.lock().read_line(&mut buffer)? == 0 {
            break;
        }
        let line = buffer.trim();
        if line.is_empty() {
            continue;
        }

        match core.command(line, &totals) {
            Step::Quit => break,
            Step::Handled(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Step::Prompt(prompt) => {
                let spent = turn(&mut core, prompt, &tx).await;
                totals.add(&spent.usage, spent.cost);
            }
        }
    }
    Ok(())
}

async fn turn(core: &mut Repl, prompt: String, tx: &UnboundedSender<Event>) -> Totals {
    core.session.log.resume(prompt);
    let ctx = core
        .ctx
        .clone()
        .with_cancel(agent::cancel_on_interrupt())
        .with_fresh_result();
    let out = core.agent.run(&mut core.session, &ctx, tx).await;

    // Saved either way: an interrupted turn is exactly the one worth keeping.
    if let Err(e) = core.store.save(
        &core.id,
        core.ctx.workspace.root(),
        &core.model,
        &core.session.log,
    ) {
        eprintln!("warning: the transcript was not saved: {e}");
    }

    match out {
        Ok(o) => o.totals,
        Err(AgentError::Cancelled) => {
            eprintln!("stopped");
            Totals::default()
        }
        Err(e) => {
            eprintln!("error {e}");
            Totals::default()
        }
    }
}
