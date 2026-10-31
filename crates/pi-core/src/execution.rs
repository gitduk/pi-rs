//! A submitted operation, its execution plan, and its outputs. Surfaces render
//! these values; neither the sender nor the renderer defines command behavior.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::Core;
use crate::driver::{Ended, Origin};
use crate::input::{Intent, Rewound, Step};
use pi_store::listing::Listing;

/// Identity remains attached to an operation while other operations run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub origin: Origin,
    pub lane: u64,
}

impl Request {
    pub fn new(origin: Origin, lane: u64) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            origin,
            lane,
        }
    }
}

/// Content before a surface lays it out. Empty notices still complete the
/// request, even when there is nothing to print.
#[derive(Debug)]
pub enum Output {
    Notice(String),
    Listing(Listing),
    Text(String),
    /// A result which also becomes context for the next model request.
    Note(String),
    Compacted(agent::Report),
}

/// State changes a surface must adopt after execution. The output is separate
/// so adopting a changed session cannot swallow the operation's answer.
pub enum Change {
    None,
    Session,
    Directory,
    Worktrees,
    Rewound(Rewound),
}

/// Immediate results and operations needing an asynchronous or host capability.
pub enum Action {
    Complete {
        change: Change,
        output: Output,
    },
    Prompt {
        send: String,
        typed: Option<String>,
    },
    Bash(String),
    Compact(Option<String>),
    Tool {
        name: String,
        args: serde_json::Value,
    },
    McpPrompt {
        prompt: mcp::Prompt,
        args: String,
        typed: String,
    },
    Edit(std::path::PathBuf),
    Surface {
        word: String,
        args: String,
    },
    Drive(crate::input::Drive),
    Quit,
}

/// Run the synchronous part once. All consumers receive the same output,
/// including commands which also replace or move the current session.
pub fn dispatch(core: &mut Core, intent: Intent) -> Action {
    let (change, output) = match core.dispatch(intent) {
        Step::Notice(text) => (Change::None, Output::Notice(text)),
        Step::Handled(rows) => (Change::None, Output::Listing(rows)),
        Step::Swap(text) => (Change::Session, Output::Notice(text)),
        Step::Moved(text) => (Change::Directory, Output::Notice(text)),
        Step::Worktrees(text) => (Change::Worktrees, Output::Notice(text)),
        Step::Rewound(outcome, unsaved) => {
            let text = rewind_output(core, &outcome, unsaved);
            (Change::Rewound(outcome), Output::Notice(text))
        }
        Step::Prompt { send, typed } => return Action::Prompt { send, typed },
        Step::Bash(command) => return Action::Bash(command),
        Step::Compact(focus) => return Action::Compact(focus),
        Step::Tool { name, args } => return Action::Tool { name, args },
        Step::McpPrompt {
            prompt,
            args,
            typed,
        } => {
            return Action::McpPrompt {
                prompt,
                args,
                typed,
            };
        }
        Step::Edit(path) => return Action::Edit(path),
        Step::Surface { word, args } => return Action::Surface { word, args },
        Step::Drive(drive) => return Action::Drive(drive),
        Step::Quit => return Action::Quit,
    };
    Action::Complete { change, output }
}

fn rewind_output(core: &Core, outcome: &Rewound, unsaved: Option<String>) -> String {
    let text = match outcome {
        Rewound::Nothing => "nothing to rewind to".into(),
        Rewound::Unsent(_) => "unsent".into(),
        Rewound::Kept => {
            let at = core
                .lane()
                .session()
                .and_then(|s| s.last_node())
                .map(|n| pi_store::text::clip(n.show(), 60))
                .filter(|t| !t.is_empty())
                .map(|t| format!(" — the transcript now ends at {t}"))
                .unwrap_or_default();
            format!("rewound{at}")
        }
    };
    crate::input::notice(std::iter::once(text).chain(unsaved))
}

/// A completed task retains its request, including after a session switch.
pub struct Completion {
    pub request: Request,
    pub output: Output,
    pub ended: Ended,
}

pub enum Kind {
    Turn,
    Bash { lines: Vec<String> },
    Compact(Option<agent::Report>),
}

pub struct Done {
    pub request: Request,
    pub kind: Kind,
    pub ran: Option<(
        agent::session::Session,
        Result<llm::stream::Usage, agent::AgentError>,
    )>,
}

impl Done {
    pub fn ended(&self) -> Ended {
        match &self.ran {
            Some((_, Ok(_))) => Ended::Done,
            Some((_, Err(agent::AgentError::Cancelled))) => Ended::Stopped,
            Some((_, Err(e))) => Ended::Failed(e.to_string()),
            None => Ended::Failed("the run crashed".into()),
        }
    }

    /// Model text already streams as events. Other tasks return their complete
    /// output here, including failures and operations which changed nothing.
    pub fn completion(&self) -> Option<Completion> {
        let ended = self.ended();
        let output = match &self.kind {
            Kind::Turn => return None,
            Kind::Compact(report) => compaction_output(*report, &ended),
            Kind::Bash { lines } => {
                let mut text = lines.join("\n");
                let suffix = match &ended {
                    Ended::Done => None,
                    Ended::Stopped | Ended::Unsent => Some("stopped".into()),
                    Ended::Failed(why) => Some(format!("failed: {why}")),
                };
                if let Some(suffix) = suffix {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&suffix);
                }
                Output::Text(text)
            }
        };
        Some(Completion {
            request: self.request,
            output,
            ended,
        })
    }
}

/// A panic cannot leave a request waiting forever or hide a lost session.
pub async fn guard<F, T>(run: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    use futures::FutureExt;
    std::panic::AssertUnwindSafe(run).catch_unwind().await.ok()
}

pub async fn compact(
    request: Request,
    mut session: agent::session::Session,
    agent: agent::Agent,
    focus: Option<String>,
    cancel: tokio_util::sync::CancellationToken,
) -> Done {
    let out = guard(async move {
        let ran = tokio::select! {
            got = agent.compact_now(&mut session, focus.as_deref()) => Ok(got),
            _ = cancel.cancelled() => Err(agent::AgentError::Cancelled),
        };
        (session, ran)
    })
    .await;
    let (report, ran) = match out {
        Some((session, Ok(report))) => (report, Some((session, Ok(llm::stream::Usage::default())))),
        Some((session, Err(e))) => (None, Some((session, Err(e)))),
        None => (None, None),
    };
    Done {
        request,
        kind: Kind::Compact(report),
        ran,
    }
}

/// Every terminal outcome is derived here, before any surface renders it.
pub fn compaction_output(report: Option<agent::Report>, ended: &Ended) -> Output {
    match ended {
        Ended::Done => match report {
            Some(report) => Output::Compacted(report),
            None => {
                Output::Notice("nothing to compact — all of it is the working context kept".into())
            }
        },
        Ended::Stopped | Ended::Unsent => Output::Notice("stopped".into()),
        Ended::Failed(why) => Output::Notice(format!("compaction failed: {why}")),
    }
}

pub async fn bash(
    request: Request,
    mut session: agent::session::Session,
    ctx: tool::Ctx,
    command: String,
) -> Done {
    let out = guard(async move {
        let out = crate::core::bash::run_bash(&ctx, &command).await;
        let ran = if ctx.cancel.is_cancelled() {
            Err(agent::AgentError::Cancelled)
        } else {
            Ok(llm::stream::Usage::default())
        };
        crate::core::bash::record_bash(&mut session, &command, out.text.clone());
        (session, ran, out.screen())
    })
    .await;
    let (lines, ran) = match out {
        Some((session, ran, lines)) => (lines, Some((session, ran))),
        None => (Vec::new(), None),
    };
    Done {
        request,
        kind: Kind::Bash { lines },
        ran,
    }
}

/// Resources lent to one model run. A host provides queued input through the
/// existing seam; it does not define the execution or its failure handling.
pub struct Turn {
    pub request: Request,
    pub session: agent::session::Session,
    pub agent: agent::Agent,
    pub ctx: tool::Ctx,
    pub events: tokio::sync::mpsc::UnboundedSender<agent::Event>,
    pub retry: agent::Retry,
    pub input: crate::driver::input::Input,
}

impl Turn {
    pub async fn run(self) -> Done {
        let Self {
            request,
            mut session,
            agent,
            ctx,
            events,
            retry,
            input,
        } = self;
        let out = guard(agent.run_with_input(&mut session, &ctx, &events, &retry, &input)).await;
        Done {
            request,
            kind: Kind::Turn,
            ran: out.map(|out| (session, out)),
        }
    }
}

pub struct Started {
    pub request: Request,
    pub output: Output,
    pub ended: Ended,
}

impl Started {
    pub fn completion(self) -> Completion {
        Completion {
            request: self.request,
            output: self.output,
            ended: self.ended,
        }
    }
}

pub async fn tool(
    request: Request,
    tool: std::sync::Arc<dyn tool::Tool>,
    args: serde_json::Value,
    ctx: tool::Ctx,
) -> Started {
    let name = tool.name().to_string();
    let call = tool::summary(&args).into_owned();
    let said = guard(crate::core::tools::run_named(tool, args, &ctx))
        .await
        .unwrap_or_else(|| Some(Err("it panicked".into())));
    let (output, ended) = match said {
        Some(Ok(text)) => (
            Output::Note(format!(
                "The user ran `{name}` directly, as `{call}`, and it answered:\n\n{text}"
            )),
            Ended::Done,
        ),
        Some(Err(why)) => (
            Output::Notice(format!("{name} failed: {why}")),
            Ended::Failed(why),
        ),
        None => (Output::Notice("stopped".into()), Ended::Stopped),
    };
    Started {
        request,
        output,
        ended,
    }
}

pub struct Fetched {
    pub request: Request,
    pub typed: String,
    pub text: Result<String, String>,
}

pub async fn fetch(request: Request, prompt: mcp::Prompt, args: String, typed: String) -> Fetched {
    let text = guard(async {
        const WAIT: std::time::Duration = std::time::Duration::from_secs(30);
        tokio::time::timeout(WAIT, prompt.text(&args))
            .await
            .unwrap_or_else(|_| Err(format!("{} did not answer", prompt.word())))
    })
    .await
    .unwrap_or_else(|| Err(format!("{} panicked", prompt.word())));
    Fetched {
        request,
        typed,
        text,
    }
}
