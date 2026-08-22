use std::collections::HashMap;

use brain::estimate;
use brain::message::{Message, Text, ToolCallId, ToolResult, ToolResultContent, UserContent};
use serde_json::Value;

/// Tools whose results describe a file or a query rather than an action. Only
/// these supersede: an `edit` result records something that happened, and the
/// record stays true however many later edits land.
const SUPERSEDABLE: &[&str] = &["read", "grep", "glob"];

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Results within this many estimated tokens of the end are left alone —
    /// they are what the agent is working from right now.
    pub protect_tail: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            protect_tail: 16_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    pub before: usize,
    pub after: usize,
    pub superseded: usize,
    pub uneventful: usize,
    pub aged_out: usize,
    pub dropped: usize,
    /// Even after dropping history the transcript is still over budget.
    pub still_over: bool,
}

impl Report {
    pub fn touched(&self) -> bool {
        self.superseded + self.uneventful + self.aged_out + self.dropped > 0
    }
}

fn elide(result: &mut ToolResult, notice: &str) -> bool {
    let already = matches!(
        result.content.as_slice(),
        [ToolResultContent::Text(t)] if t.text.starts_with("[elided")
    );
    if already {
        return false;
    }
    result.content = vec![ToolResultContent::Text(Text {
        text: notice.to_string(),
    })];
    result.useless = false;
    true
}

/// What makes two results interchangeable. A later result under the same key
/// makes every earlier one dead weight.
fn supersede_key(name: &str, args: &Value) -> Option<String> {
    if !SUPERSEDABLE.contains(&name) {
        return None;
    }
    // A whole-file read supersedes an earlier ranged read of the same path, so
    // the key deliberately ignores offset and limit.
    match args.get("path").and_then(Value::as_str) {
        Some(path) if name == "read" => Some(format!("read\0{path}")),
        _ => Some(format!("{name}\0{args}")),
    }
}

fn calls(messages: &[Message]) -> HashMap<ToolCallId, (String, Value)> {
    let mut out = HashMap::new();
    for m in messages {
        for c in m.tool_calls() {
            out.insert(c.id.clone(), (c.name.clone(), c.args.clone()));
        }
    }
    out
}

/// Positions of every tool result, paired with the estimated tokens that follow
/// its message. A small suffix means the result sits near the live end.
fn results_with_suffix(messages: &[Message]) -> Vec<(usize, usize, usize)> {
    let mut suffix = 0usize;
    let mut out = Vec::new();
    for (i, m) in messages.iter().enumerate().rev() {
        if let Message::User { content } = m {
            for (j, block) in content.iter().enumerate() {
                if matches!(block, UserContent::ToolResult(_)) {
                    out.push((i, j, suffix));
                }
            }
        }
        suffix += estimate::message(m);
    }
    out.reverse();
    out
}

fn result_at(messages: &mut [Message], i: usize, j: usize) -> Option<&mut ToolResult> {
    match &mut messages[i] {
        Message::User { content } => match &mut content[j] {
            UserContent::ToolResult(r) => Some(r),
            _ => None,
        },
        _ => None,
    }
}

/// Shrink `messages` to fit `budget`, cheapest measure first.
///
/// Every tool result keeps its message: a `tool_use` with no answering
/// `tool_result` makes the next request invalid on both wires, so content is
/// replaced, never removed.
///
/// omp guards pruning behind a prompt-cache check, because rewriting a message
/// inside the warm prefix costs a full cache rewrite. That guard is for
/// opportunistic pruning; this runs only when the transcript is already over
/// budget, where paying for a rewrite beats being refused outright.
pub fn compact(messages: &mut Vec<Message>, budget: usize, policy: &Policy) -> Report {
    let mut report = Report {
        before: estimate::tokens(messages),
        ..Default::default()
    };
    if report.before <= budget {
        report.after = report.before;
        return report;
    }

    let calls = calls(messages);
    let positions = results_with_suffix(messages);

    // A result whose key reappears later is dead weight wherever it sits, so
    // this ignores the protected tail.
    let mut last_seen: HashMap<String, usize> = HashMap::new();
    for (n, (i, j, _)) in positions.iter().enumerate() {
        if let Some(r) = result_at(messages, *i, *j)
            && let Some((name, args)) = calls.get(&r.call)
            && let Some(key) = supersede_key(name, args)
        {
            last_seen.insert(key, n);
        }
    }
    for (n, (i, j, _)) in positions.iter().enumerate() {
        let Some(r) = result_at(messages, *i, *j) else {
            continue;
        };
        let Some((name, args)) = calls.get(&r.call) else {
            continue;
        };
        let Some(key) = supersede_key(name, args) else {
            continue;
        };
        if last_seen.get(&key) != Some(&n)
            && elide(r, &format!("[elided: superseded by a later {name}]"))
        {
            report.superseded += 1;
        }
    }

    // Results their own tool marked as carrying nothing.
    for (i, j, _) in &positions {
        if let Some(r) = result_at(messages, *i, *j)
            && r.useless
            && elide(r, "[elided: this result reported nothing]")
        {
            report.uneventful += 1;
        }
    }

    if estimate::tokens(messages) > budget {
        // Oldest first, and never inside the tail the agent is working from.
        for (i, j, suffix) in &positions {
            if estimate::tokens(messages) <= budget {
                break;
            }
            if *suffix < policy.protect_tail {
                break;
            }
            if let Some(r) = result_at(messages, *i, *j)
                && elide(
                    r,
                    "[elided to fit the context window — read it again if you need it]",
                )
            {
                report.aged_out += 1;
            }
        }
    }

    // Last resort: whole exchanges leave, oldest first. The opening message is
    // the task itself and never goes.
    while estimate::tokens(messages) > budget && messages.len() >= 4 {
        let pairs_up = std::mem::discriminant(&messages[1]) != std::mem::discriminant(&messages[2]);
        if !pairs_up {
            break;
        }
        messages.drain(1..=2);
        report.dropped += 2;
    }

    report.after = estimate::tokens(messages);
    report.still_over = report.after > budget;
    report
}
