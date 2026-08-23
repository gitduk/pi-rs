use brain::stream::Usage;

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
    /// The transcript was shrunk to fit before this turn was sent.
    Compacted(crate::compact::Report),
    /// The request failed in a way worth another attempt.
    Retrying {
        attempt: usize,
        delay_ms: u64,
        reason: String,
    },
    /// Something the run recovered from but the user should know about.
    Warning(String),
    /// What the turn has cost so far, as the provider has reported it.
    ///
    /// Cumulative for the turn, not a delta, and not every wire sends one: the
    /// Anthropic wire states the input count before the first token, while an
    /// OpenAI host reports nothing until the stream ends. A surface that shows
    /// a running count treats its absence as "not known yet", never as zero.
    Usage(Usage),

    TurnEnd {
        usage: Usage,
        cost: f64,
        estimated: Estimated,
    },
    Done {
        turns: usize,
        usage: Usage,
        cost: f64,
        estimated: Estimated,
    },
}

/// Which halves of a usage figure are our own count rather than the provider's.
///
/// Per half, because a host that reports one and not the other is the ordinary
/// case: marking the pair would put a tilde on a number the provider actually
/// stated, which claims less than is known just as wrongly as claiming more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Estimated {
    pub input: bool,
    pub output: bool,
}

impl Estimated {
    /// Anything derived from both — a cost, a total — is only as measured as
    /// its least measured part.
    pub fn any(self) -> bool {
        self.input || self.output
    }

    pub fn mark(flag: bool) -> &'static str {
        if flag { "~" } else { "" }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub usage: Usage,
    pub cost: f64,
    /// Set once any turn's figures were ours. It never clears: a total mixing
    /// the two is not a measurement.
    pub estimated: Estimated,
}

impl Totals {
    pub fn add(&mut self, usage: &Usage, cost: f64) {
        self.usage.input += usage.input;
        self.usage.output += usage.output;
        self.usage.cache_read += usage.cache_read;
        self.usage.cache_write += usage.cache_write;
        self.cost += cost;
    }

    pub fn add_estimated(&mut self, usage: &Usage, cost: f64, estimated: Estimated) {
        self.add(usage, cost);
        self.estimated.input |= estimated.input;
        self.estimated.output |= estimated.output;
    }
}

/// Say it to the user, and to the journal, in that order and in one call.
///
/// The two answer different questions — a rendered line says what is happening
/// now, a record says what happened — but every fact on this list is already
/// stated here once, and stating them twice is how the two drift apart.
///
/// Called where the fact occurs rather than where the event is consumed: a
/// renderer runs in its own task, so a tap there records a true set of facts in
/// an order that never happened, under whichever span the renderer is in.
pub(crate) fn say(tx: &tokio::sync::mpsc::UnboundedSender<Event>, event: Event) {
    note(&event);
    let _ = tx.send(event);
}

/// Deltas are the exception: they are the transcript, they arrive thousands at
/// a time, and the transcript is saved beside the journal already.
fn note(event: &Event) {
    match event {
        Event::TextDelta(_) | Event::ReasoningDelta(_) | Event::Usage(_) => {}
        Event::TurnStart { turn } => tracing::info!(target: "pi::loop", turn, "turn start"),
        Event::ToolStart { id, name, args } => {
            tracing::info!(target: "pi::tool", call = %id, tool = %name, "call");
            // One level down, and in its own record: the arguments are a whole
            // patch or a whole file, the transcript already holds them, and a
            // field on the record above would serialize all of it on every call
            // only for the length cap to throw it away.
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
        Event::TurnEnd { usage, cost, .. } => tracing::info!(
            target: "pi::loop",
            input = usage.input,
            output = usage.output,
            cache_read = usage.cache_read,
            cache_write = usage.cache_write,
            cost,
            "priced"
        ),
        Event::Done { turns, cost, .. } => {
            tracing::info!(target: "pi::loop", turns, cost, "done")
        }
    }
}
