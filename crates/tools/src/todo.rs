use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

// Beyond this many items the closed ones collapse; a finished list should not
// cost as much context as an active one.
const SHOW_ALL_UNDER: usize = 20;
const KEEP_CLOSED: usize = 3;
// What a call that omits or misspells `op` is told: the ops and the fields
// each takes, so the resend is shaped right the first time.
const ARGS_HINT: &str = "the arguments are flat: `op` is required and one of \
    `set`, `mark`, `clear`, `show`, plus the fields that op needs (`items` for \
    set; `at` and `status` for mark; none for clear or show)";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
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
    /// Absent means pending — a freshly written plan is all pending, and
    /// saying so item by item is a cost paid on every plan for nothing.
    #[serde(default)]
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

/// Open and closed. Counted in one place because the tally the model reads, the
/// collapse threshold and the row the UI shows all depend on the same split —
/// three copies of `closed()` are three chances for it to drift.
fn counts(items: &[Todo]) -> (usize, usize) {
    let closed = items.iter().filter(|t| t.status.closed()).count();
    (items.len() - closed, closed)
}

/// Render for the model. Open items always show; closed ones collapse once the
/// list is long, because a finished task carries less than a pending one.
///
/// Numbered by position in the whole list, not by what survived the collapse:
/// the numbers are how `mark` addresses an item, so one that shifts when a
/// finished task folds away would retarget the call that follows.
pub fn render(items: &[Todo]) -> String {
    if items.is_empty() {
        return "the list is empty".into();
    }

    let (open, closed) = counts(items);
    let mut skip = if items.len() > SHOW_ALL_UNDER {
        closed.saturating_sub(KEEP_CLOSED)
    } else {
        0
    };

    let mut out = String::new();
    let mut collapsed = 0usize;
    for (i, t) in items.iter().enumerate() {
        if t.status.closed() && skip > 0 {
            skip -= 1;
            collapsed += 1;
            continue;
        }
        if collapsed > 0 {
            out.push_str(&format!("… {collapsed} finished\n"));
            collapsed = 0;
        }
        out.push_str(&format!("{}. {} {}", i + 1, t.status.mark(), t.task));
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

    out.push_str(&format!("{open} open, {closed} closed\n"));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Op {
    /// Replace the whole list.
    Set,
    /// Move named items to a status, leaving the rest of the list alone.
    Mark,
    /// Drop the list.
    Clear,
    /// Answer with the list and change nothing.
    ///
    /// The plan is a tool result now, so a long enough run can age it out of
    /// context — and `mark` addresses items by numbers the model can then no
    /// longer see. Without this its only way back is `set`, rewriting the whole
    /// plan from memory, which is the cost `mark` exists to avoid.
    Show,
}

// Flat rather than a tagged union, and its fields optional rather than required
// per variant: an internally tagged enum makes serde buffer the whole call
// before it picks a variant, and the path to the field that failed is gone by
// then — `unknown variant` without the `items[7].status` that names which item.
// The op's own requirements are checked below, where the message can say what
// the op needed instead of what the type did.
#[derive(Debug, Deserialize)]
struct Args {
    op: Op,
    #[serde(default)]
    items: Option<Vec<Todo>>,
    #[serde(default)]
    at: Option<Vec<usize>>,
    #[serde(default)]
    status: Option<TodoStatus>,
    #[serde(default)]
    note: Option<String>,
}

fn needed<T>(field: &str, op: &str, value: Option<T>) -> Result<T, ToolError> {
    value.ok_or_else(|| ToolError::Invalid(format!("op `{op}` needs `{field}`")))
}

/// Apply one op to the plan the context holds.
fn apply(held: &mut Vec<Todo>, args: Args) -> Result<(), ToolError> {
    match args.op {
        Op::Set => {
            let mut items = needed("items", "set", args.items)?;
            normalize(&mut items);
            *held = items;
        }
        Op::Clear => held.clear(),
        Op::Show => {}
        Op::Mark => {
            let at = needed("at", "mark", args.at)?;
            let status = needed("status", "mark", args.status)?;
            let note = args.note;
            if at.is_empty() {
                return Err(ToolError::Invalid(
                    "op `mark` needs at least one item number in `at`".into(),
                ));
            }
            if held.is_empty() {
                return Err(ToolError::Invalid(
                    "the list is empty; write one with op `set` before marking".into(),
                ));
            }
            // Every index checked before any of them moves: a mark that fails
            // halfway leaves a plan neither the model nor the user wrote.
            if let Some(bad) = at.iter().find(|n| **n == 0 || **n > held.len()) {
                return Err(ToolError::Invalid(format!(
                    "no item {bad}; the list is numbered 1 to {}",
                    held.len()
                )));
            }
            // The item just named is the one in progress. An earlier one still
            // marked so describes work that has already been left.
            if status == TodoStatus::InProgress {
                for t in held.iter_mut() {
                    if t.status == TodoStatus::InProgress {
                        t.status = TodoStatus::Pending;
                    }
                }
            }
            for n in at {
                let item = &mut held[n - 1];
                item.status = status;
                if note.is_some() {
                    item.note = note.clone();
                }
            }
            normalize(held.as_mut_slice());
        }
    }
    Ok(())
}

pub struct TodoTool;

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Keep the plan for a task with more than a couple of steps. `set` writes \
         the whole list, `mark` moves items to a status by the numbers the list \
         shows, `clear` drops it, `show` re-reads it if it has scrolled out of \
         reach. Every call answers with the plan as it now \
         stands, and that answer is the only place you see it — so mark work as \
         it lands rather than in a batch at the end, and re-read the newest \
         answer rather than an older one. Exactly one item may be in_progress: \
         marking another one there returns the previous to pending. An \
         abandoned step with a note says more than a silently dropped one."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["set", "mark", "clear", "show"],
                    "description": "set: replace the list. mark: change the status of items already on it. clear: drop it. show: read it back unchanged.",
                },
                "items": {
                    "type": "array",
                    "description": "For `set`: the complete list, in the order it should be done.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done", "blocked", "abandoned"],
                                "description": "Defaults to pending.",
                            },
                            "note": {
                                "type": "string",
                                "description": "Why, for blocked and abandoned items.",
                            },
                        },
                        "required": ["task"],
                        "additionalProperties": false,
                    },
                },
                "at": {
                    "type": "array",
                    "description": "For `mark`: the item numbers to move, as shown in the list.",
                    "items": { "type": "integer", "minimum": 1 },
                },
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "done", "blocked", "abandoned"],
                    "description": "For `mark`: what to move them to.",
                },
                "note": {
                    "type": "string",
                    "description": "For `mark`: why, for blocked and abandoned items.",
                },
            },
            "required": ["op"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        // It touches nothing but the agent's own plan.
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args_hinted(args, ARGS_HINT)?;
        let (shown, open) = {
            let mut held = ctx.todos.lock().expect("todo list poisoned");
            apply(&mut held, args)?;
            (render(&held), counts(&held).0)
        };

        Ok(ToolOutput::text(shown).with_preview(format!("{open} open")))
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use serde_json::json;

    #[test]
    fn an_unknown_op_names_the_field_it_failed_on() {
        let e = crate::parse_args::<Args>(json!({ "op": "finish" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("op"), "{e}");
    }

    // The path is why the args stay flat: a plan is a list, and "one of them is
    // wrong" is not an answer the model can act on.
    #[test]
    fn a_bad_status_names_its_item() {
        let e = crate::parse_args::<Args>(json!({
            "op": "set",
            "items": [{ "task": "x" }, { "task": "y", "status": "later" }],
        }))
        .unwrap_err()
        .to_string();
        assert!(e.contains("items[1]"), "{e}");
        assert!(e.contains("status"), "{e}");
    }

    #[test]
    fn a_missing_task_names_its_item() {
        let e = crate::parse_args::<Args>(json!({ "op": "set", "items": [{}] }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("items[0]"), "{e}");
        assert!(e.contains("task"), "{e}");
    }
}
