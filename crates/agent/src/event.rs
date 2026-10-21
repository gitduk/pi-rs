use llm::stream::Usage;
pub use llm::totals::Totals;

/// What the loop reports as it runs. A renderer consumes these; the loop never
/// writes to a terminal itself.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TurnStart {
        turn: usize,
    },
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    // A running call's latest word on how far it has got.
    ToolProgress {
        id: String,
        text: String,
    },
    ToolEnd {
        id: String,
        name: String,
        is_error: bool,
        // Ran to the end, but asks for a look: see `ToolOutput::warned`.
        warned: bool,
        preview: String,
    },
    ToolDenied {
        id: String,
        name: String,
        reason: String,
    },
    // The transcript was shrunk to fit before this turn was sent.
    Compacted(crate::compaction::ladder::Report),
    // What the request just sent occupies — system prompt, tools and
    // transcript — against the model's window. Ours rather than the
    // provider's; compaction works to a budget inside this window.
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
    // Cumulative for the turn, not a delta. Not every wire sends one (Anthropic
    // reports early, OpenAI only at stream end); absence means unknown, not zero.
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
        // What a request occupied against the model's window, in tokens,
        // as of the run's end; measured as `Context` is.
        ctx: (usize, usize),
        // How many times the transcript was shrunk to fit during this run.
        compactions: usize,
    },
    // The transcript gained entries — a state update, not a drawing
    // instruction: the renderer derives their rows through `f_entry` itself.
    Committed {
        entries: Vec<crate::session::Entry>,
    },
}

/// Where a run says what it is doing: the channel the surface drains, and
/// the hooks that watch it.
#[derive(Clone)]
pub struct Voice {
    tx: tokio::sync::mpsc::UnboundedSender<Event>,
    hooks: std::sync::Arc<dyn crate::Hooks>,
}

impl Voice {
    pub fn new(
        tx: tokio::sync::mpsc::UnboundedSender<Event>,
        hooks: std::sync::Arc<dyn crate::Hooks>,
    ) -> Self {
        Self { tx, hooks }
    }

    /// Journals it, hands it to the hooks, says it to the user, in that order,
    /// in one call — call this where the fact occurs, not where the event is
    /// consumed, or order drifts.
    pub(crate) fn say(&self, event: Event) {
        note(&event);
        self.hooks.saw(&event);
        let _ = self.tx.send(event);
    }
}

// Deltas are the exception: they are the transcript, they arrive thousands at
// a time, and the transcript is saved beside the log already.
fn note(event: &Event) {
    match event {
        Event::TextDelta(_)
        | Event::ReasoningDelta(_)
        | Event::Usage(_)
        | Event::Committed { .. } => {}
        // The loop's own "sending" record carries these two already.
        Event::Context { .. } => {}
        // What it sums up is journaled where it happened.
        Event::ToolProgress { .. } => {}
        Event::TurnStart { turn } => tracing::info!(target: "pi::loop", turn, "turn start"),
        Event::ToolStart { id, name, args } => {
            tracing::info!(target: "pi::tool", call = %id, tool = %name, args = %clipped(args), "call");
            // Whole at debug: args can be a file's worth, which `call` clips.
            tracing::debug!(target: "pi::tool", call = %id, args = %args, "arguments");
        }
        Event::ToolEnd {
            id,
            name,
            is_error,
            warned,
            preview,
        } => {
            if *is_error {
                tracing::warn!(target: "pi::tool", call = %id, tool = %name, detail = %preview, "failed")
            } else {
                tracing::info!(target: "pi::tool", call = %id, tool = %name, preview = %preview, warned, "ok")
            }
        }
        Event::ToolDenied { id, name, reason } => {
            tracing::warn!(target: "pi::tool", call = %id, tool = %name, reason = %reason, "denied")
        }
        Event::Compacted(r) => tracing::info!(
            target: "pi::compact",
            before = r.before,
            after = r.after,
            summarized = r.summarized,
            "compacted"
        ),
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => tracing::warn!(target: "pi::wire", attempt, delay_ms, reason = %reason, "retrying"),
        Event::Warning(w) | Event::Refit(w) => tracing::warn!(target: "pi::loop", "{w}"),
        // Named for what it carries: the loop sends this before the turn's
        // tool calls run, so "end" would put the record in the wrong place.
        Event::TurnEnd { usage, .. } => tracing::info!(
            target: "pi::loop",
            input = usage.input,
            output = usage.output,
            cache_read = usage.cache_read,
            cache_write = usage.cache_write,
            "counted"
        ),
        Event::Done { turns, .. } => tracing::info!(target: "pi::loop", turns, "done"),
    }
}

// Every key a call passed, each long string cut to its head: which file and
// which command survive, a written file's body does not.
fn clipped(args: &serde_json::Value) -> serde_json::Value {
    const HEAD: usize = 160;
    use serde_json::Value;
    match args {
        Value::String(s) if s.chars().count() > HEAD => {
            let head: String = s.chars().take(HEAD).collect();
            let more = s.chars().count() - HEAD;
            Value::String(format!("{head}… {more} more chars"))
        }
        Value::Array(items) => Value::Array(items.iter().map(clipped).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), clipped(v))).collect())
        }
        other => other.clone(),
    }
}
