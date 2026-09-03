use std::collections::HashMap;

use brain::estimate;
use brain::model::ModelSpec;
use serde_json::Value;

use crate::session::{
    Compaction, Entry, EntryId, Omission, Seen, Session, UserBody, oversized_args, user_block,
};

// Tools whose results describe current state rather than an action taken. Only
// these supersede: an `edit` result records something that happened, and the
// record stays true however many later edits land — but only the newest file
// read is worth carrying.
const SUPERSEDABLE: &[&str] = &["read", "grep", "glob"];

// Results that must survive compaction whatever the budget says.
//
// A skill body is instructions the agent is in the middle of following.
// Omitting it saves tokens and breaks the task — omp protects these for the
// same reason.
const PROTECTED: &[&str] = &["skill"];

// What stands in for an argument the model no longer sees. Written into the
// record as well as the view, so an archive says what went without the reader
// having to know the rule.
const ARGS_TAKEN: &str = "[omitted: the call has already run]";

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Entries within this many estimated tokens of the end are left alone —
    /// they are what the agent is working from right now.
    pub protect_tail: usize,
    /// Text over this many chars is pruned to a bounded head and tail instead
    /// of a one-line notice. The defaults keep both ends of a long output —
    /// the part of a test run that says what broke — without the middle that
    /// made it over budget. `prune_chars` is large enough for `head_chars`
    /// plus the marker plus `tail_chars`, so one pass lands under the
    /// threshold; a policy that breaks that still converges, because an
    /// omitted entry is never omitted twice.
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
    /// Tool calls whose oversized arguments went.
    pub args_taken: usize,
    pub dropped: usize,
    /// The dropped span left a summary behind.
    pub summarized: bool,
    /// Even after dropping history the transcript is still over budget.
    pub still_over: bool,
}

impl Report {
    pub fn touched(&self) -> bool {
        self.superseded + self.uneventful + self.aged_out + self.args_taken + self.dropped > 0
    }
}

// One entry as the plan currently intends to leave it. `tokens` tracks the
// running estimate so the budget check is a sum, not a re-walk of the whole
// transcript after every decision.
struct Item<'a> {
    id: EntryId,
    entry: &'a Entry,
    tokens: usize,
    /// What the view already shows in its place, from this pass or an earlier
    /// one. `fresh` is what separates the two: only this pass's decisions go
    /// into the record, or every pass would restate the ones before it.
    notice: Option<String>,
    fresh: bool,
    gone: bool,
    /// Blocks of this turn whose arguments this pass is taking. Separate from
    /// `notice`: the entry is still shown, only its bulk is not.
    args_gone: Vec<usize>,
}

impl<'a> Item<'a> {
    fn result(&self) -> Option<&'a brain::message::ToolResult> {
        match self.entry {
            Entry::User {
                body: UserBody::Result { result: r, .. },
                ..
            } if self.notice.is_none() => Some(r),
            _ => None,
        }
    }

    /// The text an omission would stand in for. A tool's result, or a `!`
    /// command's output — the two things on the user's side that carry bulk
    /// nobody is waiting on.
    fn prunable(&self) -> Option<String> {
        if self.notice.is_some() {
            return None;
        }
        match self.entry {
            Entry::User {
                body: UserBody::Result { result: r, .. },
                ..
            } => Some(r.flatten_text()),
            Entry::User {
                body: UserBody::Aside(t),
                ..
            } => Some(t.text.clone()),
            _ => None,
        }
    }

    /// Assistant turns are never taken: a `tool_use` with no answering
    /// `tool_result` makes the next request invalid on both formats.
    fn omittable(&self) -> bool {
        match self.entry {
            Entry::User { body, .. } => match body {
                UserBody::Result { result: r, .. } => !PROTECTED.contains(&r.name.as_str()),
                // A question stays whatever the budget says: what someone
                // asked is not the answer's spare context. An aside is the
                // other half of that — a `!` command's output, which nothing
                // downstream waits on — and the variant is what makes the two
                // answerable apart at all.
                UserBody::Aside(_) => true,
                UserBody::Prompt(_) | UserBody::Image(_) => false,
            },
            _ => false,
        }
    }

    fn omit(&mut self, notice: String) {
        self.tokens = estimate::MESSAGE_OVERHEAD + estimate::text(&notice);
        self.notice = Some(notice);
        self.fresh = true;
    }
}

// Per entry, not per wire message: several user entries merge into one
// message, so this counts the framing more than once. That is the safe
// direction — the estimate decides *when* to compact, and compacting a little
// early costs tokens where compacting a little late costs the request.
fn tokens_of(
    seen: &Seen<'_>,
    spec: &ModelSpec,
    gone: &HashMap<(EntryId, usize), &str>,
) -> usize {
    let body = match seen.entry() {
        Entry::User { body, .. } => estimate::user_block(&user_block(body)),
        Entry::Assistant { id, blocks, .. } => Session::shown_blocks(blocks, *id, gone)
            .iter()
            .map(|b| estimate::assistant_block(b, spec))
            .sum(),
        _ => return 0,
    };
    estimate::MESSAGE_OVERHEAD + body
}

// Tokens sitting after each index, so "is this inside the working tail" is a
// lookup rather than a re-walk.
fn suffixes(items: &[Item<'_>]) -> Vec<usize> {
    let mut out = vec![0; items.len()];
    let mut running = 0;
    for n in (0..items.len()).rev() {
        out[n] = running;
        running += items[n].tokens;
    }
    out
}

// One text block standing in for a pruned entry: the notice, a bounded head,
// a marker naming how much went, and a bounded tail. Chars are code points, so
// slicing never splits a surrogate pair.
fn pruned(notice: &str, text: &str, policy: &Policy) -> String {
    let c = text.chars().count();
    if c <= policy.prune_chars {
        return notice.to_string();
    }
    let head = brain::slice::head_chars(text, policy.head_chars);
    let tail = brain::slice::tail_chars(text, policy.tail_chars);
    let dropped = c.saturating_sub(policy.head_chars + policy.tail_chars);
    format!("{notice}\n\n{head}\n\n[… {dropped} chars omitted …]\n\n{tail}")
}

// What makes two results interchangeable. A later result under the same key
// makes every earlier one dead weight.
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

/// Decide how to shrink the session's context to fit `budget`, cheapest measure
/// first.
///
/// Nothing is mutated: the result is a record the caller appends to the session,
/// and the view derives from it. That is what keeps a long session readable
/// afterwards — a transcript compacted in place is a transcript destroyed.
///
/// Every entry keeps its place in the exchange. A `tool_use` with no answering
/// `tool_result` makes the next request invalid on both formats, so content is
/// replaced, never removed, except when a whole exchange goes at once.
pub fn plan(
    session: &Session,
    spec: &ModelSpec,
    budget: usize,
    policy: &Policy,
) -> (Compaction, Report) {
    let view = session.view();
    let already_gone = session.block_omissions();
    let mut items: Vec<Item> = view
        .iter()
        .map(|s| match s {
            Seen::Omitted { entry, notice } => Item {
                id: entry.id(),
                entry,
                tokens: estimate::MESSAGE_OVERHEAD + estimate::text(notice),
                notice: Some((*notice).to_string()),
                fresh: false,
                gone: false,
                args_gone: Vec::new(),
            },
            Seen::As(entry) => Item {
                id: entry.id(),
                entry,
                tokens: tokens_of(s, spec, &already_gone),
                notice: None,
                fresh: false,
                gone: false,
                args_gone: Vec::new(),
            },
        })
        .collect();

    let before: usize = items.iter().map(|i| i.tokens).sum();
    let mut record = Compaction {
        tokens_before: before,
        ..Default::default()
    };
    let mut report = Report {
        before,
        ..Default::default()
    };

    if before <= budget {
        record.tokens_after = before;
        report.after = before;
        return (record, report);
    }

    // Every call in the transcript, so a result can name the work it answers.
    let calls: HashMap<&str, (&str, &Value)> = view
        .iter()
        .flat_map(|s| s.entry().tool_calls())
        .map(|c| (c.id.as_str(), (c.name.as_str(), &c.args)))
        .collect();

    let total = |items: &[Item]| -> usize { items.iter().map(|i| i.tokens).sum() };

    // A result whose key reappears later is dead weight wherever it sits, so
    // this ignores the protected tail.
    let mut newest: HashMap<String, usize> = HashMap::new();
    for (n, it) in items.iter().enumerate() {
        if let Some(r) = it.result()
            && let Some((name, args)) = calls.get(r.call.as_str())
            && let Some(key) = supersede_key(name, args)
        {
            newest.insert(key, n);
        }
    }
    // Decided in one pass and applied in another: `omit` needs the item
    // mutably, and reading it to decide already holds it.
    let stale: Vec<(usize, String)> = items
        .iter()
        .enumerate()
        .filter_map(|(n, it)| {
            let r = it.result()?;
            let (name, args) = calls.get(r.call.as_str())?;
            let key = supersede_key(name, args)?;
            (newest.get(&key) != Some(&n) && it.omittable())
                .then(|| (n, format!("[omitted: superseded by a later {name}]")))
        })
        .collect();
    for (n, notice) in stale {
        items[n].omit(notice);
        report.superseded += 1;
    }

    // Results their own tool marked as carrying nothing.
    let empty: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.result().is_some_and(|r| r.useless) && it.omittable())
        .map(|(n, _)| n)
        .collect();
    for n in empty {
        items[n].omit("[omitted: this result reported nothing]".into());
        report.uneventful += 1;
    }

    // Results and `!` command output, oldest first, never inside the tail the
    // agent is working from. Both carry bulk nothing downstream waits on.
    if total(&items) > budget {
        let suffix = suffixes(&items);
        for n in 0..items.len() {
            if total(&items) <= budget || suffix[n] < policy.protect_tail {
                break;
            }
            if !items[n].omittable() || items[n].notice.is_some() {
                continue;
            }
            let notice = "[omitted to fit the context window]";
            let Some(body) = items[n].prunable() else {
                continue;
            };
            items[n].omit(pruned(notice, &body, policy));
            report.aged_out += 1;
        }
    }

    // Oversized tool arguments: the file a `write` wrote, the patch an `edit`
    // applied. The call has run and its result records what happened, so what
    // is left is the model's own carbon copy of the work, not context it still
    // needs — and it is the one weight on the assistant side worth taking.
    // Thinking blocks are deliberately not touched: the API filters prior ones
    // itself without billing them, and the last turn's may not be edited at all.
    if total(&items) > budget {
        let suffix = suffixes(&items);
        let mut gone = already_gone.clone();
        for n in 0..items.len() {
            if total(&items) <= budget || suffix[n] < policy.protect_tail {
                break;
            }
            let Entry::Assistant { id, blocks, .. } = items[n].entry else {
                continue;
            };
            let fat: Vec<usize> = blocks
                .iter()
                .enumerate()
                .filter(|(k, b)| {
                    !already_gone.contains_key(&(*id, *k))
                        && matches!(b, brain::message::AssistantContent::ToolCall(c)
                            if oversized_args(c) > 0)
                })
                .map(|(k, _)| k)
                .collect();
            if fat.is_empty() {
                continue;
            }
            items[n].args_gone.extend(fat);
            for k in &items[n].args_gone {
                gone.insert((*id, *k), ARGS_TAKEN);
            }
            items[n].tokens = estimate::MESSAGE_OVERHEAD
                + Session::shown_blocks(blocks, *id, &gone)
                    .iter()
                    .map(|b| estimate::assistant_block(b, spec))
                    .sum::<usize>();
            report.args_taken += items[n].args_gone.len();
        }
    }

    // The kept ends are a floor a window the provider just named can refuse.
    // Last rung: a pruned entry keeps only the notice that leads it.
    if total(&items) > budget {
        for n in 0..items.len() {
            if total(&items) <= budget {
                break;
            }
            let Some(full) = items[n].notice.clone() else {
                continue;
            };
            let Some(head) = full.lines().next() else {
                continue;
            };
            if head.len() == full.len() {
                continue;
            }
            items[n].omit(head.to_string());
        }
    }

    // Last resort: history leaves the view, oldest first.
    while total(&items) > budget {
        let suffix = suffixes(&items);
        let Some(doomed) = droppable(&items, policy, &suffix) else {
            break;
        };
        for n in &doomed {
            items[*n].gone = true;
            items[*n].tokens = 0;
            record.dropped.push(items[*n].id);
        }
        report.dropped += doomed.len();
    }

    for it in &items {
        if !it.gone {
            for k in &it.args_gone {
                record.omissions.push(Omission {
                    entry: it.id,
                    block: Some(*k),
                    notice: ARGS_TAKEN.to_string(),
                });
            }
        }
        if let Some(notice) = &it.notice
            && it.fresh
            && !it.gone
        {
            record.omissions.push(Omission {
                entry: it.id,
                block: None,
                notice: notice.clone(),
            });
        }
    }

    record.tokens_after = total(&items);
    report.after = record.tokens_after;
    report.still_over = report.after > budget;
    (record, report)
}

// Where each round of the conversation begins.
//
// A round is a prompt and everything that answered it. What sits *ahead* of a
// prompt with nothing between belongs to it, not to the round that ended
// before: an image is the attachment the question is about, and a `!` command
// is what the user ran in order to ask. Attaching either backwards lets the
// drop tier take it out from under the question that refers to it.
fn round_starts(items: &[Item<'_>]) -> Vec<usize> {
    let is_prompt = |it: &Item<'_>| {
        matches!(
            it.entry,
            Entry::User {
                body: UserBody::Prompt(_),
                ..
            }
        )
    };
    let leads_in = |it: &Item<'_>| {
        matches!(
            it.entry,
            Entry::User {
                body: UserBody::Image(_) | UserBody::Aside(_),
                ..
            }
        )
    };
    let mut out = Vec::new();
    for n in 0..items.len() {
        if !is_prompt(&items[n]) {
            continue;
        }
        let mut start = n;
        while start > 0 && leads_in(&items[start - 1]) {
            start -= 1;
        }
        out.push(start);
    }
    out
}

// A skill body is instructions the agent is in the middle of following.
// Eliding it saves tokens and breaks the task, and so does dropping it.
fn protected(it: &Item<'_>) -> bool {
    matches!(
        it.entry,
        Entry::User {
            body: UserBody::Result { result: r, .. },
            ..
        } if PROTECTED.contains(&r.name.as_str())
    )
}

// The entries of `span` that are still in the view, or `None` when the span
// holds nothing to take or something that must not go.
fn takeable(items: &[Item<'_>], span: std::ops::Range<usize>) -> Option<Vec<usize>> {
    if items[span.clone()].iter().any(protected) {
        return None;
    }
    let out: Vec<usize> = span.filter(|n| !items[*n].gone).collect();
    (!out.is_empty()).then_some(out)
}

// The first entry of a round's body — everything the prompt and its
// attachments are not.
fn after_prompt(items: &[Item<'_>], start: usize, end: usize) -> usize {
    items[start..end]
        .iter()
        .position(|it| {
            matches!(
                it.entry,
                Entry::User {
                    body: UserBody::Prompt(_),
                    ..
                }
            )
        })
        .map_or(start, |p| start + p + 1)
}

// What leaves the view next, oldest first.
//
// The unit is a round — a prompt and everything that answered it — because
// the smaller one was an assistant turn and its results, which left the
// question standing with its answer gone. A question nobody will answer is
// not the answer's spare context; it is what someone asked.
//
// The opening prompt is the task itself and stays whatever happens to its
// work, so round zero gives up its body and keeps its head.
//
// A round the working tail reaches is taken exchange by exchange instead.
// That is not a weaker rule but the same one: inside a single round there is
// only one question, it is the task, and it is already being kept — so there
// is nothing left to orphan. It is also the only thing that works on the
// shape most sessions actually have, one prompt and eighty tool calls, where
// a round-sized unit can never fire at all.
fn droppable(items: &[Item<'_>], policy: &Policy, suffix: &[usize]) -> Option<Vec<usize>> {
    let starts = round_starts(items);
    let tail = |end: usize| end < items.len() && suffix[end] >= policy.protect_tail;

    for (k, &start) in starts.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(items.len());
        if !tail(end) {
            break;
        }
        let body = if k == 0 {
            after_prompt(items, start, end)
        } else {
            start
        };
        if let Some(doomed) = takeable(items, body..end) {
            return Some(doomed);
        }
    }

    // Nothing whole qualified: take one exchange out of the newest round,
    // never reaching past its prompt.
    let &last = starts.last()?;
    let floor = after_prompt(items, last, items.len());
    for n in floor..items.len() {
        if suffix[n] < policy.protect_tail {
            break;
        }
        if items[n].gone || !matches!(items[n].entry, Entry::Assistant { .. }) {
            continue;
        }
        let span = exchange(items, n);
        if span.iter().any(|k| protected(&items[*k])) {
            continue;
        }
        return Some(span);
    }
    None
}

// One exchange: an assistant turn and the results answering it. The unit
// `droppable` falls back to, inside the newest round.
//
// Joined by call id rather than by adjacency, because the invariant is the
// pairing — a `tool_result` whose `tool_use` is gone makes the next request
// invalid — and adjacency does not express it: a turn that called no tool has
// no answers, so the entry after it belongs to whatever came next, not here.
fn exchange(items: &[Item<'_>], start: usize) -> Vec<usize> {
    let calls: Vec<&str> = items[start]
        .entry
        .tool_calls()
        .map(|c| c.id.as_str())
        .collect();
    let mut out = vec![start];
    if calls.is_empty() {
        return out;
    }
    for (n, it) in items.iter().enumerate().skip(start + 1) {
        let answers = matches!(it.entry, Entry::User { body: UserBody::Result { result: r, .. }, .. }
            if calls.contains(&r.call.as_str()));
        if answers && !it.gone {
            out.push(n);
        }
    }
    out
}
