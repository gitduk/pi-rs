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
            Step::Bash(command) => {
                for line in core.bash(&command, agent::cancel_on_interrupt()).await {
                    println!("{line}");
                }
            }
            Step::Swap(lines) | Step::Handled(lines) => {
                for line in lines {
                    println!("{line}");
                }
            }
            Step::Compact(focus) => match core
                .agent
                .compact_now(&mut core.session, focus.as_deref())
                .await
            {
                Some((report, spent)) => {
                    totals.merge(&spent);
                    println!("compacted {} → {} tokens", report.before, report.after);
                    if let Err(e) = core.save() {
                        eprintln!("warning: the transcript was not saved: {e}");
                    }
                }
                None => {
                    let held = core.agent.kept_tokens().unwrap_or(0);
                    let now = brain::estimate::tokens(&core.session.context(), &core.agent.spec);
                    println!(
                        "nothing to compact — {now} tokens, all inside the {held} kept as working context"
                    );
                }
            },
            Step::Prompt { send, typed } => {
                let spent = turn(&mut core, send, typed, &tx).await;
                totals.merge(&spent);
            }
            Step::Wechat(_) => println!(
                "wechat needs a terminal to show the login QR — run pi in a terminal"
            ),
        }
    }
    Ok(())
}

async fn turn(
    core: &mut Repl,
    prompt: String,
    typed: Option<String>,
    tx: &UnboundedSender<Event>,
) -> Totals {
    core.session.send_prompt(prompt, typed);
    let ctx = core.ctx.clone().with_cancel(agent::cancel_on_interrupt());
    let out = core.agent.run(&mut core.session, &ctx, tx).await;

    // Saved either way: an interrupted turn is exactly the one worth keeping.
    if let Err(e) = core.save() {
        eprintln!("warning: the transcript was not saved: {e}");
    }

    match out {
        Ok(totals) => totals,
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
