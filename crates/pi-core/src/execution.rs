//! A submitted operation, its execution plan, and its outputs. Surfaces render
//! these values; neither the sender nor the renderer defines command behavior.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::core::Core;
use crate::driver::{Ended, Origin};
use crate::input::{Intent, Rewound, Step};
use pi_store::listing::Listing;
use toolbox::channel::Outcome;

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
    pub outcome: Outcome,
}

pub enum Kind {
    Turn,
    Bash { lines: Vec<String> },
    Compact(Option<agent::Report>),
}

#[derive(Debug)]
pub enum RunError {
    Agent(agent::AgentError),
    Failed(String),
    Exit(i32),
    Crashed,
}

impl From<agent::AgentError> for RunError {
    fn from(error: agent::AgentError) -> Self {
        Self::Agent(error)
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Agent(error) => std::fmt::Display::fmt(error, f),
            Self::Failed(message) => f.write_str(message),
            Self::Exit(code) => write!(f, "exit {code}"),
            Self::Crashed => f.write_str("the run crashed"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::Failed(_) | Self::Exit(_) | Self::Crashed => None,
        }
    }
}

impl RunError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Agent(agent::AgentError::Cancelled))
    }

    pub fn ended(&self) -> Ended {
        if self.is_cancelled() {
            Ended::Stopped
        } else {
            Ended::Failed(self.to_string())
        }
    }

    pub fn outcome(&self) -> Outcome {
        match self {
            Self::Agent(agent::AgentError::Cancelled) => Outcome::Stopped(String::new()),
            Self::Exit(code) => Outcome::Exited(*code),
            Self::Agent(_) | Self::Failed(_) | Self::Crashed => Outcome::Failed(self.to_string()),
        }
    }
}

pub type RunResult = Result<llm::stream::Usage, RunError>;

pub struct Done {
    pub request: Request,
    pub kind: Kind,
    pub ran: Option<(agent::session::Session, RunResult)>,
}

impl Done {
    pub fn ended(&self) -> Ended {
        match &self.ran {
            Some((_, Ok(_))) => Ended::Done,
            Some((_, Err(error))) => error.ended(),
            None => RunError::Crashed.ended(),
        }
    }

    pub fn outcome(&self) -> Outcome {
        match &self.ran {
            Some((_, Ok(_))) if matches!(self.kind, Kind::Bash { .. }) => Outcome::Exited(0),
            Some((_, Ok(_))) => Outcome::Done,
            Some((_, Err(error))) => error.outcome(),
            None => RunError::Crashed.outcome(),
        }
    }

    /// Model text already streams as events. Other tasks return their complete
    /// output here, including failures and operations which changed nothing.
    pub fn completion(&self) -> Option<Completion> {
        let outcome = self.outcome();
        let output = match &self.kind {
            Kind::Turn => return None,
            Kind::Compact(report)
                if self.ran.as_ref().is_some_and(|(_, result)| result.is_ok()) =>
            {
                compaction_output(*report)
            }
            Kind::Compact(_) => Output::Notice(String::new()),
            Kind::Bash { lines, .. } => Output::Text(lines.join("\n")),
        };
        Some(Completion {
            request: self.request,
            output,
            outcome,
        })
    }
}

thread_local! {
    // How many `guard`s are polling on this thread right now.
    static GUARDING: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether a panic raised on this thread now is one `guard` will catch: the
/// process goes on, so whatever a panic hook would tear down is still in use.
pub fn panic_is_caught() -> bool {
    GUARDING.with(std::cell::Cell::get) > 0
}

/// A panic cannot leave a request waiting forever or hide a lost session.
pub async fn guard<F, T>(run: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    use futures::FutureExt;
    struct Polling;
    impl Drop for Polling {
        fn drop(&mut self) {
            GUARDING.with(|n| n.set(n.get() - 1));
        }
    }
    let mut run = std::pin::pin!(run);
    // Counted only while it polls: between polls the thread runs other work,
    // whose panics nothing here catches.
    let polled = std::future::poll_fn(move |cx| {
        GUARDING.with(|n| n.set(n.get() + 1));
        let _polling = Polling;
        run.as_mut().poll(cx)
    });
    std::panic::AssertUnwindSafe(polled)
        .catch_unwind()
        .await
        .ok()
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
        Some((session, Err(e))) => (None, Some((session, Err(e.into())))),
        None => (None, None),
    };
    Done {
        request,
        kind: Kind::Compact(report),
        ran,
    }
}

/// Content left by a successful compaction.
pub fn compaction_output(report: Option<agent::Report>) -> Output {
    match report {
        Some(report) => Output::Compacted(report),
        None => Output::Notice("nothing to compact — all of it is the working context kept".into()),
    }
}

pub async fn bash(
    request: Request,
    mut session: agent::session::Session,
    ctx: tool::Ctx,
    command: String,
) -> Done {
    let out = guard(async move {
        let (lines, result) = match crate::core::bash::run_bash(&ctx, &command).await {
            Ok(out) => {
                crate::core::bash::record_bash(&mut session, &command, out.text);
                let result = if ctx.cancel.is_cancelled() {
                    Err(agent::AgentError::Cancelled.into())
                } else if out.code != 0 {
                    Err(RunError::Exit(out.code))
                } else {
                    Ok(llm::stream::Usage::default())
                };
                (out.output, result)
            }
            Err(tool::ToolError::Cancelled) => {
                (Vec::new(), Err(agent::AgentError::Cancelled.into()))
            }
            Err(error) => (
                Vec::new(),
                Err(RunError::Failed(format!(
                    "failed to run `{command}`: {error}"
                ))),
            ),
        };
        (session, result, lines)
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
            ran: out.map(|out| (session, out.map_err(Into::into))),
        }
    }
}

pub type Started = Completion;

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
    let (output, outcome) = match said {
        Some(Ok(text)) => (
            Output::Note(format!(
                "The user ran `{name}` directly, as `{call}`, and it answered:\n\n{text}"
            )),
            Outcome::Done,
        ),
        Some(Err(why)) => (
            Output::Notice(String::new()),
            Outcome::Failed(format!("{name}: {why}")),
        ),
        None => (
            Output::Notice(String::new()),
            Outcome::Stopped(String::new()),
        ),
    };
    Started {
        request,
        output,
        outcome,
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

#[cfg(test)]
mod tests {
    #[test]
    fn a_panic_under_guard_is_known_to_be_caught() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let prior = std::panic::take_hook();
        let (saw, here) = (seen.clone(), std::thread::current().id());
        // The hook is the process's: other tests' panics are not this one's.
        std::panic::set_hook(Box::new(move |_| {
            if std::thread::current().id() == here {
                saw.lock().unwrap().push(super::panic_is_caught());
            }
        }));
        let out = futures::executor::block_on(super::guard(async {
            if super::panic_is_caught() {
                panic!("caught");
            }
        }));
        let outside = std::panic::catch_unwind(|| panic!("not guarded"));
        std::panic::set_hook(prior);
        assert!(out.is_none() && outside.is_err());
        assert_eq!(*seen.lock().unwrap(), [true, false]);
        assert!(
            !super::panic_is_caught(),
            "the count unwound with the panic"
        );
    }
}
