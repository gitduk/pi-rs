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
    },
    Done {
        turns: usize,
        usage: Usage,
        cost: f64,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub usage: Usage,
    pub cost: f64,
}

impl Totals {
    pub fn add(&mut self, usage: &Usage, cost: f64) {
        self.usage.input += usage.input;
        self.usage.output += usage.output;
        self.usage.cache_read += usage.cache_read;
        self.usage.cache_write += usage.cache_write;
        self.cost += cost;
    }
}
