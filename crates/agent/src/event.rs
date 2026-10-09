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
        preview: String,
    },
    ToolDenied {
        id: String,
        name: String,
        reason: String,
    },
    // The transcript was shrunk to fit before this turn was sent.
    Compacted(crate::compaction::ladder::Report),
    // What the transcript occupies against what it may, for the request just
    // sent. Ours rather than the provider's: this is what compaction acts on.
    Context {
        used: usize,
        budget: usize,
    },
    // The request failed in a way worth another attempt.
    Retrying {
        attempt: usize,
        delay_ms: u64,
        reason: String,
    },
    // Something the run recovered from but the user should know about.
    Warning(String),
    // Cumulative for the turn, not a delta. Not every wire sends one (Anthropic
    // reports early, OpenAI only at stream end); absence means unknown, not zero.
    Usage(Usage),

    TurnEnd {
        usage: Usage,
    },
    Done {
        turns: usize,
        usage: Usage,
        // What the transcript occupied against what it was allowed to, in
        // tokens, for the request that ended the run.
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

/// Says to the user and journals it, in that order, in one call — call this
/// where the fact occurs, not where the event is consumed, or order drifts.
pub(crate) fn say(tx: &tokio::sync::mpsc::UnboundedSender<Event>, event: Event) {
    note(&event);
    let _ = tx.send(event);
}

// Deltas are the exception: they are the transcript, they arrive thousands at
// a time, and the transcript is saved beside the journal already.
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
            tracing::info!(target: "pi::tool", call = %id, tool = %name, "call");
            // Logged separately at debug: args can be a whole file's worth, and
            // the info-level record above would serialize it all just to truncate it.
            tracing::debug!(target: "pi::tool", call = %id, args = %args, "arguments");
        }
        Event::ToolEnd {
            id,
            name,
            is_error,
            preview,
        } => {
            if *is_error {
                tracing::warn!(target: "pi::tool", call = %id, tool = %name, detail = %preview, "failed")
            } else {
                tracing::info!(target: "pi::tool", call = %id, tool = %name, preview = %preview, "ok")
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
        Event::Warning(w) => tracing::warn!(target: "pi::loop", "{w}"),
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
