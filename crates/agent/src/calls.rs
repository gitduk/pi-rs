//! One batch of tool calls: gated, hooked, run at the concurrency each
//! tool allows, and landed as results in call order.

use std::collections::HashMap;
use std::sync::Arc;

use llm::message::{ToolCall, ToolResult};
use llm::stream::{InvalidToolArgs, Usage};
use tokio::sync::mpsc::UnboundedSender;
use tool::{Concurrency, Ctx, ToolError, ToolOutput};
use tracing::Instrument as _;

use crate::event::say;
use crate::session::Drawn;
use crate::{Agent, Decision, Event};

// How much of a failed argument blob rides back to the model and log; past
// it, show a window around serde's column rather than the head.
const MAX_INVALID_ARGS_SHOWN: usize = 400;

// What a streamed call resolves to before anything runs. Deciding first keeps
// the result list aligned with the call list even when nothing executes.
enum Action {
    Reject(String),
    Run(Arc<dyn tool::Tool>),
}

impl Agent {
    // One result per call, in call order, except a cancelled one, left
    // unanswered for the run's end to close; `spent` carries nested costs up.
    pub(crate) async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<String, InvalidToolArgs>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
        spent: &mut Usage,
    ) -> (Vec<(ToolResult, Drawn)>, bool) {
        let mut actions: Vec<Action> = calls
            .iter()
            .map(|c| {
                if let Some(invalid) = bad.get(&c.id) {
                    let snippet = invalid_args_snippet(&invalid.raw, &invalid.error);
                    tracing::warn!(
                        target: "pi::wire",
                        call = %c.id,
                        name = %c.name,
                        error = %invalid.error,
                        raw = %snippet,
                        "tool arguments were not valid JSON"
                    );
                    return Action::Reject(format!(
                        "arguments were not valid JSON ({}); you sent: {snippet}; \
                         send the whole object again",
                        invalid.error,
                    ));
                }
                let Some(tool) = self.brief.registry.get(&c.name) else {
                    return Action::Reject(format!(
                        "no tool named `{}`; available: {}",
                        c.name,
                        self.brief.registry.names().join(", ")
                    ));
                };
                match self.brief.approver.approve(&c.name, tool.tier(), &c.args) {
                    Decision::Allow => Action::Run(tool),
                    Decision::Deny(why) => Action::Reject(why),
                }
            })
            .collect();
        // Hooks decide beside the approver, so a refusal is a denial like its.
        let refusals = futures::future::join_all(calls.iter().zip(&actions).map(
            |(call, action)| async move {
                match action {
                    Action::Run(_) => self
                        .brief
                        .hooks
                        .before(&call.name, &call.args, ctx)
                        .await
                        .err(),
                    Action::Reject(_) => None,
                }
            },
        ))
        .await;
        for (action, refused) in actions.iter_mut().zip(refusals) {
            if let Some(why) = refused {
                *action = Action::Reject(why);
            }
        }

        for (call, action) in calls.iter().zip(&actions) {
            match action {
                Action::Reject(why) => {
                    say(
                        tx,
                        Event::ToolDenied {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            reason: why.clone(),
                        },
                    );
                }
                Action::Run(_) => {
                    say(
                        tx,
                        Event::ToolStart {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: call.args.clone(),
                        },
                    );
                }
            }
        }

        // Awaited positionally, so results stay aligned with the calls.
        let futures: Vec<_> = calls
            .iter()
            .zip(&actions)
            .map(|(call, action)| {
                async move {
                    match action {
                        Action::Reject(why) => Landed::Answered(
                            ToolResult::error(call.id.clone(), &call.name, why.clone()),
                            Drawn::default(),
                        ),
                        Action::Run(t) => {
                            let ctx = &with_progress(ctx, call, tx);
                            let mut output =
                                tool::output::gated(t.as_ref(), call.args.clone(), ctx).await;
                            if let Ok(out) = &mut output
                                && let Some(note) = self
                                    .brief
                                    .hooks
                                    .after(&call.name, &call.args, out, ctx)
                                    .await
                            {
                                // Through the same gate as the tool's own output.
                                let note = tool::output::bound(ToolOutput::text(note), ctx);
                                out.content.extend(note.content);
                            }
                            landed(call, output, tx)
                        }
                    }
                }
                .instrument(ran(call))
            })
            .collect();
        // Neighbouring Shared calls join; an Exclusive one waits for those
        // ahead of it and holds back those behind.
        let mut outcomes: Vec<Landed> = Vec::with_capacity(calls.len());
        let mut shared = Vec::new();
        for (f, action) in futures.into_iter().zip(&actions) {
            if matches!(action, Action::Run(t) if t.concurrency() == Concurrency::Exclusive) {
                outcomes.extend(futures::future::join_all(std::mem::take(&mut shared)).await);
                outcomes.push(f.await);
            } else {
                shared.push(f);
            }
        }
        outcomes.extend(futures::future::join_all(shared).await);

        let mut results = Vec::with_capacity(calls.len());
        let mut stopped = false;
        for outcome in outcomes {
            match outcome {
                // Left unanswered: its siblings may have acted, so theirs are
                // kept, and the run's end closes this one as stopped.
                Landed::Stopped => stopped = true,
                Landed::Answered(result, drawn) => results.push((result, drawn)),
                Landed::Ran(result, drawn, used) => {
                    // A nested run's spend belongs to the run that called it:
                    // folded in here, it reaches `Event::Done` and the return.
                    spent.add(&used);
                    results.push((result, drawn));
                }
            }
        }

        (results, stopped)
    }
}

// What one call came to, with what the screen drew for it: sent along so a
// rebuild redraws those bytes instead of re-reading the content.
enum Landed {
    Stopped,
    Answered(ToolResult, Drawn),
    Ran(ToolResult, Drawn, Usage),
}

// The call's own context, its progress said as events under its id.
fn with_progress(ctx: &Ctx, call: &ToolCall, tx: &UnboundedSender<Event>) -> Ctx {
    let (id, tx) = (call.id.clone(), tx.clone());
    ctx.clone().with_progress(Arc::new(move |text| {
        say(
            &tx,
            Event::ToolProgress {
                id: id.clone(),
                text,
            },
        )
    }))
}

// A call's output as its result, its end said as it lands.
fn landed(
    call: &ToolCall,
    output: Result<ToolOutput, ToolError>,
    tx: &UnboundedSender<Event>,
) -> Landed {
    match output {
        Err(ToolError::Cancelled) => Landed::Stopped,
        Err(e) => {
            let mut body = e.to_string();
            if let Some(code) = e.code() {
                // A stable code lets the model branch on what happened
                // instead of parsing the prose; the prose still leads.
                body = format!("Error: {body} [code: {code}]");
            }
            // The pending live line draws from this same text, so a rebuild's
            // equality check sees one source, not a live/rebuilt mismatch.
            say(
                tx,
                Event::ToolEnd {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: true,
                    warned: false,
                    preview: body.clone(),
                },
            );
            Landed::Answered(
                ToolResult::error(call.id.clone(), &call.name, body.clone()),
                Drawn {
                    preview: Some(body),
                    warned: false,
                },
            )
        }
        Ok(out) => {
            say(
                tx,
                Event::ToolEnd {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    is_error: false,
                    warned: out.warned,
                    preview: out.preview(),
                },
            );
            let result = ToolResult {
                call: call.id.clone(),
                name: call.name.clone(),
                content: out.content,
                is_error: false,
            };
            let drawn = Drawn {
                preview: out.preview,
                warned: out.warned,
            };
            Landed::Ran(result, drawn, out.spent)
        }
    }
}

fn ran(call: &ToolCall) -> tracing::Span {
    tracing::info_span!(target: "pi::tool", "tool", name = %call.name, call = %call.id)
}

// Shown whole when it fits, else windowed around serde's 1-indexed column
// (chars, not bytes — close enough); no column falls back to the tail.
fn invalid_args_snippet(raw: &str, err: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() <= MAX_INVALID_ARGS_SHOWN {
        return raw.to_string();
    }
    let column = err
        .rsplit_once("column ")
        .and_then(|(_, n)| n.trim().parse::<usize>().ok())
        .unwrap_or(chars.len())
        .saturating_sub(1)
        .min(chars.len());
    let half = MAX_INVALID_ARGS_SHOWN / 2;
    let to = (column.saturating_sub(half) + MAX_INVALID_ARGS_SHOWN).min(chars.len());
    let from = to - MAX_INVALID_ARGS_SHOWN;

    let mut out = String::new();
    if from > 0 {
        out.push('…');
    }
    out.extend(chars[from..to].iter());
    if to < chars.len() {
        out.push('…');
    }
    out
}
