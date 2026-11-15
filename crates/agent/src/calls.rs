//! One batch of tool calls: gated, hooked, run at the concurrency each
//! tool allows, and retained as each call finishes.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};

use llm::message::{ToolCall, ToolResult};
use llm::stream::{InvalidToolArgs, Usage};
use tool::{Concurrency, Ctx, ToolError, ToolOutput};
use tracing::Instrument as _;

use crate::event::Reporter;
use crate::session::{Drawn, Session};
use crate::{Agent, AgentError, Decision, Event, replies, seams::Input};

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
    // Completed calls are retained independently; cancelled calls are left
    // unanswered for the run's end to close.
    pub(crate) async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<String, InvalidToolArgs>,
        contexts: &[Ctx],
        reporter: &Reporter,
        batch: &mut Batch<'_>,
    ) -> Result<bool, AgentError> {
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
                        "arguments were not valid JSON ({}). You sent: {snippet}. \
                         Send the complete JSON object again.",
                        invalid.error,
                    ));
                }
                let Some(tool) = self.brief.registry.get(&c.name) else {
                    return Action::Reject(format!(
                        "no tool named `{}`. Available tools: {}",
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
        let refusals = futures::future::join_all(calls.iter().zip(&actions).zip(contexts).map(
            |((call, action), ctx)| async move {
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
                    reporter
                        .report(Event::ToolDenied {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            reason: why.clone(),
                        })
                        .await;
                }
                Action::Run(_) => {
                    reporter
                        .report(Event::ToolQueued {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: call.args.clone(),
                        })
                        .await;
                }
            }
        }

        // Each future carries its call identity, independent of completion order.
        let futures: Vec<_> = calls
            .iter()
            .zip(&actions)
            .zip(contexts)
            .map(|((call, action), ctx)| {
                async move {
                    match action {
                        Action::Reject(why) => Landed::Answered(
                            ToolResult::error(call.id.clone(), &call.name, why.clone()),
                            Drawn::default(),
                        ),
                        Action::Run(t) => {
                            let began = std::time::Instant::now();
                            reporter
                                .report(Event::ToolStart {
                                    id: call.id.clone(),
                                    at: began,
                                })
                                .await;
                            let ctx = &with_call_reporter(ctx, call, reporter);
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
                            landed(call, output, began.elapsed())
                        }
                    }
                }
                .instrument(ran(call))
            })
            .collect();
        // Neighbouring Shared calls join; an Exclusive one waits for those
        // ahead of it and holds back those behind.
        let mut shared = FuturesUnordered::new();
        for (f, action) in futures.into_iter().zip(&actions) {
            if matches!(action, Action::Run(t) if t.concurrency() == Concurrency::Exclusive) {
                while let Some(outcome) = shared.next().await {
                    batch.retain(outcome).await?;
                }
                batch.flush().await;
                batch.retain(f.await).await?;
            } else {
                shared.push(f);
            }
        }
        while let Some(outcome) = shared.next().await {
            batch.retain(outcome).await?;
        }
        batch.flush().await;
        Ok(batch.stopped)
    }
}

/// Retain each finished result while its siblings continue to run.
pub(crate) struct Batch<'a> {
    pub session: &'a mut Session,
    pub input: &'a dyn Input,
    pub ctx: &'a Ctx,
    pub reporter: &'a Reporter,
    pub spent: &'a mut Usage,
    pub stopped: bool,
    pub pending: VecDeque<BoxFuture<'a, ()>>,
}

impl<'a> Batch<'a> {
    async fn retain(&mut self, outcome: Landed) -> Result<(), AgentError> {
        let (result, drawn) = match outcome {
            Landed::Stopped => {
                self.stopped = true;
                return Ok(());
            }
            Landed::Answered(result, drawn) => (result, drawn),
            Landed::Ran(result, drawn, used) => {
                self.spent.add(&used);
                (result, drawn)
            }
        };
        let resolved = self
            .input
            .completed_calls()
            .map_err(AgentError::Interaction)?
            .into_iter()
            .filter(|r| r.call == result.call)
            .collect();
        let call = ToolCall {
            id: result.call.clone(),
            name: result.name.clone(),
            args: serde_json::Value::Null,
        };
        let results = replies::reconcile(&[call], vec![(result, drawn)], resolved, self.ctx)?;
        for event in replies::retain(self.session, self.input, results).await? {
            self.pending.push_back(self.reporter.report(event).boxed());
        }
        self.publish_ready();
        Ok(())
    }

    fn publish_ready(&mut self) {
        // Keep draining completed tools even if observation is backpressured.
        // The deferred notifications are bounded by this finite call batch.
        while let Some(front) = self.pending.front_mut() {
            if front.as_mut().now_or_never().is_none() {
                break;
            }
            self.pending.pop_front();
        }
    }

    async fn flush(&mut self) {
        while let Some(reporting) = self.pending.pop_front() {
            reporting.await;
        }
    }
}

// What one call came to, with what the screen drew for it: sent along so a
// rebuild redraws those bytes instead of re-reading the content.
enum Landed {
    Stopped,
    Answered(ToolResult, Drawn),
    Ran(ToolResult, Drawn, Usage),
}

// The call's own context: its progress and what it prints said as events
// under its id.
fn with_call_reporter(ctx: &Ctx, call: &ToolCall, reporter: &Reporter) -> Ctx {
    let (progress_id, output_id) = (call.id.clone(), call.id.clone());
    let (progress_reporter, output_reporter) = (reporter.clone(), reporter.clone());
    ctx.clone()
        .with_progress(Arc::new(move |text| {
            progress_reporter.latest(Event::ToolProgress {
                id: progress_id.clone(),
                text,
            })
        }))
        .with_output(Arc::new(move |text| {
            output_reporter.latest(Event::ToolPrinted {
                id: output_id.clone(),
                text,
            })
        }))
}

// A call's output as its result, its end said as it lands.
fn landed(
    call: &ToolCall,
    output: Result<ToolOutput, ToolError>,
    took: std::time::Duration,
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
            Landed::Answered(
                ToolResult::error(call.id.clone(), &call.name, body.clone()),
                Drawn {
                    preview: Some(body),
                    warned: false,
                    took: Some(took),
                },
            )
        }
        Ok(out) => {
            let result = ToolResult {
                call: call.id.clone(),
                name: call.name.clone(),
                content: out.content,
                is_error: false,
            };
            let drawn = Drawn {
                preview: out.preview,
                warned: out.warned,
                took: Some(took),
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
