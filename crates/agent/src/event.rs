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
