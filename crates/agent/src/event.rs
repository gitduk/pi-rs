use llm::stream::Usage;
pub use llm::totals::Totals;

pub use transcript::Outcome;

mod channel;
pub use channel::{Receiver, Sender, channel};

/// What the loop reports as it runs. A renderer consumes these; the loop never
/// writes to a terminal itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TurnStart {
        turn: usize,
    },
    TextDelta(String),
    ReasoningDelta(String),
    // A call taken into its batch; it may wait there for an exclusive one.
    ToolQueued {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    // The call begins running at `at`: its time counts from there, by this
    // one clock for whoever shows it.
    ToolStart {
        id: String,
        at: std::time::Instant,
    },
    // A running call's latest word on how far it has got.
    ToolProgress {
        id: String,
        text: String,
    },
    // What a running call has printed so far, replacing the last view.
    ToolPrinted {
        id: String,
        text: String,
    },
    ToolEnd {
        id: String,
        name: String,
        outcome: Outcome,
        screen: String,
        // From its `ToolStart` to when it landed.
        took: std::time::Duration,
    },
    ToolDenied {
        id: String,
        name: String,
        reason: String,
    },
    // The transcript was shrunk to fit before this turn was sent.
    Compacted(crate::compaction::ladder::Report),
    // What the request just answered occupied — system prompt, tools and
    // transcript — as the provider counted it, against the model's window.
    Context {
        used: usize,
        window: usize,
    },
    // The request failed in a way worth another attempt.
    Retrying {
        attempt: usize,
        delay_ms: u64,
        reason: String,
    },
    // Something the run recovered from but the user should know about.
    Warning(String),
    // The request did not fit and is being resent smaller: a warning, but one
    // about the context from here on, like `Compacted`, not about the reply.
    Refit(String),
    // Cumulative for the turn, not a delta. Sent whenever the provider reports
    // counts; before its first report the usage is unknown, not zero.
    Usage(Usage),

    TurnEnd {
        usage: Usage,
        // The reply's text, whole: what streamed as `TextDelta`s, for whoever
        // wants the turn's answer without collecting them.
        text: String,
        // How long the model streamed its reply, first piece to last; `None`
        // when nothing streamed. What `usage.output` took to write.
        generating: Option<std::time::Duration>,
    },
    Done {
        turns: usize,
        usage: Usage,
        // The run's last `Context`; `None` when the provider counted nothing.
        ctx: Option<(usize, usize)>,
        // How many times the transcript was shrunk to fit during this run.
        compactions: usize,
    },
    // The run ended on an error; a stop the user asked for is not one.
    Failed {
        error: String,
    },
    // The transcript gained entries — a state update, not a drawing
    // instruction: whoever draws derives its rows from them itself.
    Committed {
        entries: Vec<crate::session::Entry>,
    },
}

impl Event {
    fn is_progress(&self) -> bool {
        matches!(
            self,
            Self::Usage(_) | Self::ToolProgress { .. } | Self::ToolPrinted { .. }
        )
    }

    fn retained_bytes(&self) -> usize {
        let payload = match self {
            Self::TextDelta(text)
            | Self::ReasoningDelta(text)
            | Self::Warning(text)
            | Self::Refit(text)
            | Self::Failed { error: text }
            | Self::TurnEnd { text, .. } => text.len(),
            Self::ToolQueued { id, name, args } => id.len() + name.len() + json_bytes(args),
            Self::ToolStart { id, .. } => id.len(),
            Self::ToolProgress { id, text } | Self::ToolPrinted { id, text } => {
                id.len() + text.len()
            }
            Self::ToolEnd {
                id,
                name,
                screen: text,
                ..
            }
            | Self::ToolDenied {
                id,
                name,
                reason: text,
            } => id.len() + name.len() + text.len(),
            Self::Committed { entries } => json_bytes(entries),
            Self::Compacted(_)
            | Self::Context { .. }
            | Self::Usage(_)
            | Self::TurnStart { .. }
            | Self::Done { .. } => 0,
            Self::Retrying { reason, .. } => reason.len(),
        };
        std::mem::size_of::<Self>() + payload
    }

    fn replaces(&self, previous: &Self) -> bool {
        match (self, previous) {
            (Self::Usage(_), Self::Usage(_)) => true,
            (Self::ToolProgress { id, .. }, Self::ToolProgress { id: old, .. })
            | (Self::ToolPrinted { id, .. }, Self::ToolPrinted { id: old, .. }) => id == old,
            _ => false,
        }
    }
}

fn json_bytes(value: &impl serde::Serialize) -> usize {
    struct Count(usize);

    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("serializable event payload");
    count.0
}

/// Where a run reports what it is doing: the channel the surface drains, and
/// the hooks that watch it.
#[derive(Clone)]
pub struct Reporter {
    tx: Sender,
    hooks: std::sync::Arc<dyn crate::Hooks>,
    cancel: tokio_util::sync::CancellationToken,
}

impl Reporter {
    pub fn new(tx: Sender, hooks: std::sync::Arc<dyn crate::Hooks>) -> Self {
        Self {
            tx,
            hooks,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub(crate) fn with_cancel(mut self, cancel: tokio_util::sync::CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    /// Report a warning through the same hooks and outlet as loop events.
    pub async fn warn(&self, warning: String) {
        self.report(Event::Warning(warning)).await;
    }

    /// Notify hooks and forward an event from the place where it occurs.
    pub(crate) fn report(&self, event: Event) -> impl std::future::Future<Output = ()> + Send + '_ {
        self.hooks.saw(&event);
        async move {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {},
                _ = self.tx.send(event) => {},
            }
        }
    }

    pub(crate) fn latest(&self, event: Event) {
        self.hooks.saw(&event);
        self.tx.latest(event);
    }
}
