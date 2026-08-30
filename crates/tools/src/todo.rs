use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// Beyond this many items the closed ones collapse; a finished list should not
/// cost as much context as an active one.
const SHOW_ALL_UNDER: usize = 20;
const KEEP_CLOSED: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    /// Waiting on something outside the agent's reach.
    Blocked,
    /// Deliberately not doing it. Distinct from done, and the reason matters.
    Abandoned,
}

impl TodoStatus {
    pub fn mark(self) -> &'static str {
        match self {
            TodoStatus::Pending => "[ ]",
            TodoStatus::InProgress => "[~]",
            TodoStatus::Done => "[x]",
            TodoStatus::Blocked => "[!]",
            TodoStatus::Abandoned => "[-]",
        }
    }

    pub fn closed(self) -> bool {
        matches!(self, TodoStatus::Done | TodoStatus::Abandoned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub task: String,
    pub status: TodoStatus,
    /// Why it is blocked or abandoned. Ignored for the other statuses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// At most one item may be in progress: two are a plan the agent is not
/// actually following. Later ones fall back to pending.
pub fn normalize(items: &mut [Todo]) {
    let mut seen = false;
    for item in items.iter_mut() {
        if item.status == TodoStatus::InProgress {
            if seen {
                item.status = TodoStatus::Pending;
            }
            seen = true;
        }
    }
}

/// Render for the model. Open items always show; closed ones collapse once the
/// list is long, because a finished task carries less than a pending one.
pub fn render(items: &[Todo]) -> String {
    if items.is_empty() {
        return "the list is empty".into();
    }

    let closed = items.iter().filter(|t| t.status.closed()).count();
    let mut skip = if items.len() > SHOW_ALL_UNDER {
        closed.saturating_sub(KEEP_CLOSED)
    } else {
        0
    };

    let mut out = String::new();
    let mut collapsed = 0usize;
    for t in items {
        if t.status.closed() && skip > 0 {
            skip -= 1;
            collapsed += 1;
            continue;
        }
        if collapsed > 0 {
            out.push_str(&format!("… {collapsed} finished\n"));
            collapsed = 0;
        }
        out.push_str(t.status.mark());
        out.push(' ');
        out.push_str(&t.task);
        if let Some(note) = t
            .note
            .as_deref()
            .filter(|_| t.status.closed() || t.status == TodoStatus::Blocked)
        {
            out.push_str(&format!(" — {note}"));
        }
        out.push('\n');
    }
    if collapsed > 0 {
        out.push_str(&format!("… {collapsed} finished\n"));
    }

    let open = items.iter().filter(|t| !t.status.closed()).count();
    out.push_str(&format!("{open} open, {closed} closed\n"));
    out
}

#[derive(Debug, Deserialize)]
struct Args {
    items: Vec<Todo>,
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Record the plan for a task with more than a couple of steps, and keep it \
         current as you go. Each call replaces the whole list, so send every item \
         every time. Exactly one item may be in_progress. Mark work done as it \
         lands rather than in a batch at the end — an abandoned step with a note \
         says more than a silently dropped one."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "description": "The complete list, in the order it should be done.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done", "blocked", "abandoned"],
                            },
                            "note": {
                                "type": "string",
                                "description": "Why, for blocked and abandoned items.",
                            },
                        },
                        "required": ["task", "status"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["items"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        // It touches nothing but the agent's own plan.
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args(args)?;
        let mut items = args.items;
        normalize(&mut items);

        // An acknowledgement, not the list. The list reaches the model as a
        // note on the next request, recomputed from the stored plan — echoing
        // it here too would put one copy per call in the transcript, every one
        // of them stale the moment the next call lands.
        let open = items.iter().filter(|t| !t.status.closed()).count();
        let total = items.len();
        *ctx.todos.lock().expect("todo list poisoned") = items;

        Ok(ToolOutput::text(format!("Recorded: {open} of {total} open."))
            .with_preview(format!("{open} open")))
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use serde_json::json;

    #[test]
    fn a_missing_status_names_its_item() {
        let e = crate::parse_args::<Args>(json!({ "items": [{ "task": "x" }] }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("items[0]"), "{e}");
        assert!(e.contains("status"), "{e}");
    }
}
