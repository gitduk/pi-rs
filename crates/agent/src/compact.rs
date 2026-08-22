use std::collections::HashMap;

use brain::estimate;
use brain::message::{Message, Text, ToolCallId, ToolResult, ToolResultContent, UserContent};
use serde_json::Value;

use crate::log::{Compaction, Elision, EntryId, Log};

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

/// Decide how to shrink the log's context to fit `budget`, cheapest measure
/// first.
///
/// Nothing is mutated: the result is a record the caller appends to the log,
/// and the view derives from it. That is what keeps a long session readable
/// afterwards — a transcript compacted in place is a transcript destroyed.
///
/// Every tool result keeps its message. A `tool_use` with no answering
/// `tool_result` makes the next request invalid on both wires, so content is
/// replaced, never removed.
///
/// omp guards pruning behind a prompt-cache check, because rewriting a message
/// inside the warm prefix costs a full cache rewrite. That guard is for
/// opportunistic pruning; this runs only when the context is already over
/// budget, where paying for a rewrite beats being refused outright.
pub fn plan(log: &Log, budget: usize, policy: &Policy) -> (Compaction, Report) {
    let mut ids: Vec<EntryId> = log.live().iter().map(|(id, _)| *id).collect();
    let mut work = log.context();

    let mut record = Compaction {
        tokens_before: estimate::tokens(&work),
        ..Default::default()
    };
    let mut report = Report {
        before: record.tokens_before,
        ..Default::default()
    };

    if record.tokens_before <= budget {
        record.tokens_after = record.tokens_before;
        report.after = report.before;
        return (record, report);
    }

    let calls = calls(&work);
    let positions = results_with_suffix(&work);

    let note = |record: &mut Compaction, work: &mut [Message], i, j, notice: String| -> bool {
        let Some(r) = result_at(work, i, j) else {
            return false;
        };
        if !elide(r, &notice) {
            return false;
        }
        record.elisions.push(Elision {
            call: r.call.clone(),
            notice,
        });
        true
    };

    // A result whose key reappears later is dead weight wherever it sits, so
    // this ignores the protected tail.
    let mut newest: HashMap<String, usize> = HashMap::new();
    for (n, (i, j, _)) in positions.iter().enumerate() {
        if let Some(r) = result_at(&mut work, *i, *j)
            && let Some((name, args)) = calls.get(&r.call)
            && let Some(key) = supersede_key(name, args)
        {
            newest.insert(key, n);
        }
    }
    for (n, (i, j, _)) in positions.iter().enumerate() {
        let Some(r) = result_at(&mut work, *i, *j) else {
            continue;
        };
        let Some((name, args)) = calls.get(&r.call) else {
            continue;
        };
        let Some(key) = supersede_key(name, args) else {
            continue;
        };
        if newest.get(&key) == Some(&n) {
            continue;
        }
        let notice = format!("[elided: superseded by a later {name}]");
        if note(&mut record, &mut work, *i, *j, notice) {
            report.superseded += 1;
        }
    }

    // Results their own tool marked as carrying nothing.
    for (i, j, _) in &positions {
        let uneventful = result_at(&mut work, *i, *j).is_some_and(|r| r.useless);
        if uneventful
            && note(
                &mut record,
                &mut work,
                *i,
                *j,
                "[elided: this result reported nothing]".into(),
            )
        {
            report.uneventful += 1;
        }
    }

    if estimate::tokens(&work) > budget {
        // Oldest first, and never inside the tail the agent is working from.
        for (i, j, suffix) in &positions {
            if estimate::tokens(&work) <= budget || *suffix < policy.protect_tail {
                break;
            }
            let notice = "[elided to fit the context window — read it again if you need it]";
            if note(&mut record, &mut work, *i, *j, notice.into()) {
                report.aged_out += 1;
            }
        }
    }

    // Last resort: whole exchanges leave the view, oldest first. The opening
    // message is the task itself and never goes.
    while estimate::tokens(&work) > budget && work.len() >= 4 {
        if std::mem::discriminant(&work[1]) == std::mem::discriminant(&work[2]) {
            break;
        }
        work.drain(1..=2);
        record.dropped.extend(ids.drain(1..=2));
        report.dropped += 2;
    }

    record.tokens_after = estimate::tokens(&work);
    report.after = record.tokens_after;
    report.still_over = report.after > budget;
    (record, report)
}
