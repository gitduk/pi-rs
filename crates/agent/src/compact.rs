use std::collections::HashMap;

use brain::estimate;
use brain::message::{Message, Text, ToolCallId, ToolResult, ToolResultContent, UserContent};
use serde_json::Value;

use crate::session::{Compaction, Elision, EntryId, Session};

/// Tools whose results describe current state rather than an action taken. Only
/// these supersede: an `edit` result records something that happened, and the
/// record stays true however many later edits land — but only the newest task
/// list or file read is worth carrying.
const SUPERSEDABLE: &[&str] = &["read", "grep", "glob", "todo"];

/// Results that must survive compaction whatever the budget says.
///
/// A skill body is instructions the agent is in the middle of following.
/// Eliding it saves tokens and breaks the task — omp protects these for the
/// same reason.
const PROTECTED: &[&str] = &["skill"];

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Results within this many estimated tokens of the end are left alone —
    /// they are what the agent is working from right now.
    pub protect_tail: usize,
    /// Text over this many chars is pruned to a bounded head and tail instead
    /// of a one-line notice. The defaults keep both ends of a long output —
    /// the part of a test run that says what broke — without the middle that
    /// made it over budget. `prune_chars` is large enough for `head_chars`
    /// plus the marker plus `tail_chars`, so one pass lands under the
    /// threshold; a policy that breaks that still converges, because an
    /// elided result is never elided twice.
    pub prune_chars: usize,
    pub head_chars: usize,
    pub tail_chars: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            protect_tail: 16_000,
            prune_chars: 8_192,
            head_chars: 4_096,
            tail_chars: 1_024,
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
    /// The dropped span left a summary behind.
    pub summarized: bool,
    /// Even after dropping history the transcript is still over budget.
    pub still_over: bool,
}

impl Report {
    pub fn touched(&self) -> bool {
        self.superseded + self.uneventful + self.aged_out + self.dropped > 0
    }
}

/// Replace a result's content with a bounded stand-in. `ends` Some keeps a
/// head and tail over the prune threshold — the aged-out tier, where the
/// result was useful but too big; None replaces with the notice alone, which
/// is all a superseded or empty result is worth. Returns the replacement
/// text, or nothing when the result must be left alone; the caller records
/// it, and the view derives from the record, so plan's scratch copy never
/// reaches the session.
fn elide(result: &mut ToolResult, notice: &str, ends: Option<&Policy>) -> Option<String> {
    if PROTECTED.contains(&result.name.as_str()) {
        return None;
    }
    let already = matches!(
        result.content.as_slice(),
        [ToolResultContent::Text(t)] if t.text.starts_with("[elided")
    );
    if already {
        return None;
    }
    let replacement = match ends {
        Some(policy) => pruned(notice, &result.flatten_text(), policy),
        None => notice.to_string(),
    };
    result.content = vec![ToolResultContent::Text(Text {
        text: replacement.clone(),
    })];
    result.useless = false;
    Some(replacement)
}

/// One text block standing in for a pruned result: the notice, a bounded
/// head, a marker naming how much went, and a bounded tail. Chars are code
/// points, so slicing never splits a surrogate pair.
fn pruned(notice: &str, text: &str, policy: &Policy) -> String {
    let c = text.chars().count();
    if c <= policy.prune_chars {
        return notice.to_string();
    }
    let head = brain::slice::head_chars(text, policy.head_chars);
    let tail = brain::slice::tail_chars(text, policy.tail_chars);
    let dropped = c.saturating_sub(policy.head_chars + policy.tail_chars);
    format!("{notice}\n\n{head}\n\n[… {dropped} chars elided …]\n\n{tail}")
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

/// Whether the exchange that `k` (a tool call) opens may be dropped whole —
/// every result in its answering message must be one elision would take. The
/// drop tier owes the skill body the same protection the elision tier does.
fn exchange_droppable(work: &[Message], k: usize) -> bool {
    match &work[k + 1] {
        Message::User { content } => content.iter().all(|c| match c {
            UserContent::ToolResult(r) => !PROTECTED.contains(&r.name.as_str()),
            _ => true,
        }),
        _ => true,
    }
}

/// Decide how to shrink the session's context to fit `budget`, cheapest measure
/// first.
///
/// Nothing is mutated: the result is a record the caller appends to the session,
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
pub fn plan(session: &Session, budget: usize, policy: &Policy) -> (Compaction, Report) {
    let mut ids: Vec<EntryId> = session.live().iter().map(|(id, _)| *id).collect();
    let mut work = session.context();

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

    let note = |record: &mut Compaction,
                work: &mut [Message],
                i,
                j,
                notice: String,
                ends: Option<&Policy>| -> bool {
        let Some(r) = result_at(work, i, j) else {
            return false;
        };
        let Some(replacement) = elide(r, &notice, ends) else {
            return false;
        };
        record.elisions.push(Elision {
            call: r.call.clone(),
            notice: replacement,
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
        if note(&mut record, &mut work, *i, *j, notice, None) {
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
                None,
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
            let notice = "[elided to fit the context window]";
            if note(&mut record, &mut work, *i, *j, notice.into(), Some(policy)) {
                report.aged_out += 1;
            }
        }
    }

    // The kept ends are a floor a window the provider just named can refuse.
    // Last rung: a pruned result keeps only the notice that leads it. The
    // `already` guard would refuse to touch an elided result, so this
    // replaces outright; a one-line notice has no newline and stays as it is.
    if estimate::tokens(&work) > budget {
        for (i, j, _) in &positions {
            if estimate::tokens(&work) <= budget {
                break;
            }
            let Some(r) = result_at(&mut work, *i, *j) else {
                continue;
            };
            if PROTECTED.contains(&r.name.as_str()) {
                continue;
            }
            let [ToolResultContent::Text(t)] = r.content.as_slice() else {
                continue;
            };
            let Some(notice) = t.text.lines().next() else {
                continue;
            };
            if !t.text.starts_with("[elided") || t.text.len() == notice.len() {
                continue;
            }
            let notice = notice.to_string();
            r.content = vec![ToolResultContent::Text(Text {
                text: notice.clone(),
            })];
            record.elisions.push(Elision {
                call: r.call.clone(),
                notice,
            });
        }
    }

    // Last resort: whole exchanges leave the view, oldest first. The opening
    // message is the task itself and never goes, and neither does an exchange
    // whose result elision would refuse — the skill body is instructions the
    // agent is in the middle of following, not spare context.
    while estimate::tokens(&work) > budget && work.len() >= 4 {
        let Some(start) = (1..=work.len() - 2)
            .step_by(2)
            .find(|&k| exchange_droppable(&work, k))
        else {
            break;
        };
        if std::mem::discriminant(&work[start]) == std::mem::discriminant(&work[start + 1]) {
            break;
        }
        work.drain(start..=start + 1);
        record.dropped.extend(ids.drain(start..=start + 1));
        report.dropped += 2;
    }

    record.tokens_after = estimate::tokens(&work);
    report.after = record.tokens_after;
    report.still_over = report.after > budget;
    (record, report)
}
