use std::collections::HashMap;
use std::sync::Arc;

use brain::catalog::ModelSpec;
use brain::message::{Message, ToolCall, ToolResult};
use brain::request::{Effort, Request};
use brain::stream::{Accumulator, StreamEvent};
use brain::transport::Transport;
use futures::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tools::{Concurrency, Ctx, Registry, ToolError, ToolOutput};

pub mod approval;
pub mod event;

pub use approval::{Approver, Ceiling, Decision};
pub use event::{Event, Totals};

pub const DEFAULT_SYSTEM: &str = include_str!("../prompts/system.md");

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Brain(#[from] brain::BrainError),

    #[error("cancelled")]
    Cancelled,

    #[error("stopped at the {0}-turn limit")]
    TurnLimit(usize),
}

/// The transcript. Held by the caller so a run that ends in an error still
/// leaves behind everything it produced.
#[derive(Debug, Default, Clone)]
pub struct Session {
    pub messages: Vec<Message>,
}

impl Session {
    pub fn with_prompt(prompt: impl Into<String>) -> Self {
        Self {
            messages: vec![Message::user(prompt)],
        }
    }
}

pub struct Agent {
    pub transport: Arc<dyn Transport>,
    pub spec: ModelSpec,
    pub registry: Registry,
    pub approver: Arc<dyn Approver>,
    pub system: String,
    pub effort: Effort,
    pub max_turns: usize,
}

/// What a streamed call resolves to before anything runs. Deciding first keeps
/// the result list aligned with the call list even when nothing executes.
enum Action {
    Reject(String),
    Run(Arc<dyn tools::Tool>),
}

impl Agent {
    pub fn new(transport: Arc<dyn Transport>, spec: ModelSpec) -> Self {
        Self {
            transport,
            spec,
            registry: Registry::builtin(),
            approver: Arc::new(Ceiling(tools::Tier::Exec)),
            system: DEFAULT_SYSTEM.to_string(),
            effort: Effort::Off,
            max_turns: 50,
        }
    }

    pub async fn run(
        &self,
        session: &mut Session,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<Totals, AgentError> {
        let mut totals = Totals::default();

        for turn in 1..=self.max_turns {
            let _ = tx.send(Event::TurnStart { turn });

            let req = Request {
                system: Some(self.system.clone()),
                messages: session.messages.clone(),
                tools: self.registry.defs(),
                max_output_tokens: None,
                temperature: None,
                effort: self.effort,
                tool_choice: Default::default(),
            };

            let done = self.stream_turn(&req, ctx, tx).await?;
            let cost = self.spec.cost(&done.usage);
            totals.add(&done.usage, cost);
            let _ = tx.send(Event::TurnEnd {
                usage: done.usage,
                cost,
            });

            let calls: Vec<ToolCall> = done.message.tool_calls().cloned().collect();
            session.messages.push(done.message);

            if calls.is_empty() {
                let _ = tx.send(Event::Done {
                    turns: turn,
                    usage: totals.usage,
                    cost: totals.cost,
                });
                return Ok(totals);
            }

            let bad: HashMap<_, _> = done
                .invalid
                .iter()
                .map(|i| (i.call.clone(), i.error.clone()))
                .collect();
            let results = self.run_calls(&calls, &bad, ctx, tx).await?;
            session.messages.push(Message::tool_results(results));
        }

        Err(AgentError::TurnLimit(self.max_turns))
    }

    async fn stream_turn(
        &self,
        req: &Request,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<brain::stream::Completion, AgentError> {
        let mut acc = Accumulator::new(self.transport.name(), &self.spec.wire_id);
        let mut stream = tokio::select! {
            r = self.transport.stream(&self.spec, req) => r?,
            _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
        };

        loop {
            let next = tokio::select! {
                n = stream.next() => n,
                _ = ctx.cancel.cancelled() => return Err(AgentError::Cancelled),
            };
            let Some(ev) = next else { break };
            let ev = ev?;
            match &ev {
                StreamEvent::TextDelta { delta, .. } => {
                    let _ = tx.send(Event::TextDelta(delta.clone()));
                }
                StreamEvent::ReasoningDelta { delta, .. } => {
                    let _ = tx.send(Event::ReasoningDelta(delta.clone()));
                }
                _ => {}
            }
            acc.push(ev);
        }

        Ok(acc.finish())
    }

    /// Every call gets exactly one result, in call order: an unanswered
    /// `tool_use` makes the next request invalid on both wires.
    async fn run_calls(
        &self,
        calls: &[ToolCall],
        bad: &HashMap<brain::message::ToolCallId, String>,
        ctx: &Ctx,
        tx: &UnboundedSender<Event>,
    ) -> Result<Vec<ToolResult>, AgentError> {
        let actions: Vec<Action> = calls
            .iter()
            .map(|c| {
                if let Some(err) = bad.get(&c.id) {
                    return Action::Reject(format!(
                        "arguments were not valid JSON ({err}); send the whole object again"
                    ));
                }
                let Some(tool) = self.registry.get(&c.name) else {
                    return Action::Reject(format!(
                        "no tool named `{}`; available: {}",
                        c.name,
                        self.registry.names().join(", ")
                    ));
                };
                match self.approver.approve(&c.name, tool.tier(), &c.args) {
                    Decision::Allow => Action::Run(tool),
                    Decision::Deny(why) => Action::Reject(why),
                }
            })
            .collect();

        for (call, action) in calls.iter().zip(&actions) {
            match action {
                Action::Reject(why) => {
                    let _ = tx.send(Event::ToolDenied {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        reason: why.clone(),
                    });
                }
                Action::Run(_) => {
                    let _ = tx.send(Event::ToolStart {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        args: call.args.clone(),
                    });
                }
            }
        }

        // Building one future per call and awaiting them positionally is what
        // keeps results aligned; completion order never reaches the transcript.
        let exclusive = actions
            .iter()
            .any(|a| matches!(a, Action::Run(t) if t.concurrency() == Concurrency::Exclusive));

        let mut outputs: Vec<Option<Result<ToolOutput, ToolError>>> =
            Vec::with_capacity(calls.len());
        if exclusive {
            for (call, action) in calls.iter().zip(&actions) {
                outputs.push(match action {
                    Action::Reject(_) => None,
                    Action::Run(t) => Some(t.execute(call.args.clone(), ctx).await),
                });
            }
        } else {
            let futures: Vec<_> = calls
                .iter()
                .zip(&actions)
                .map(|(call, action)| async move {
                    match action {
                        Action::Reject(_) => None,
                        Action::Run(t) => Some(t.execute(call.args.clone(), ctx).await),
                    }
                })
                .collect();
            outputs = futures::future::join_all(futures).await;
        }

        let mut results = Vec::with_capacity(calls.len());
        for ((call, action), output) in calls.iter().zip(&actions).zip(outputs) {
            let result = match (action, output) {
                (Action::Reject(why), _) => ToolResult::error(call.id.clone(), &call.name, why),
                (_, Some(Err(ToolError::Cancelled))) => return Err(AgentError::Cancelled),
                (_, Some(Err(e))) => {
                    let body = e.to_string();
                    let _ = tx.send(Event::ToolEnd {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        is_error: true,
                        preview: body.clone(),
                    });
                    ToolResult::error(call.id.clone(), &call.name, body)
                }
                (_, Some(Ok(out))) => {
                    let _ = tx.send(Event::ToolEnd {
                        id: call.id.0.clone(),
                        name: call.name.clone(),
                        is_error: false,
                        preview: out.preview(),
                    });
                    ToolResult {
                        call: call.id.clone(),
                        provider: call.provider.clone(),
                        name: call.name.clone(),
                        content: out.content,
                        is_error: false,
                        useless: out.useless,
                    }
                }
                (_, None) => unreachable!("only rejected calls produce no output"),
            };
            results.push(result);
        }

        Ok(results)
    }
}

/// Cancels on the first Ctrl-C so a runaway turn stops without killing the
/// process mid-write.
pub fn cancel_on_interrupt() -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            child.cancel();
        }
    });
    token
}
