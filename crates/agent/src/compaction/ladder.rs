use std::collections::HashMap;

use llm::estimate;
use llm::model::ModelSpec;

use crate::session::{
    Compaction, Entry, EntryId, Omission, Seen, Session, injected_summary, oversized_args,
    user_block,
};

// What stands in for a result the same call answered again later.
const REPEATED: &str = "[omitted: the same call ran again later]";

// What leads an aged-out result, ahead of the ends it keeps.
const AGED_OUT: &str = "[omitted to fit the context window]";

// What stands in for an argument the model no longer sees. Also written
// into the record, so an archive says what went without knowing the rule.
const ARGS_TAKEN: &str = "[omitted: the call has already run]";

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    /// Entries within this many estimated tokens of the end are left alone —
    /// they are what the agent is working from right now.
    pub protect_tail: usize,
    /// Text over this many chars is pruned to a bounded head and tail instead
    /// of a one-line notice, keeping both ends of a long output. Must exceed
    /// `head_chars` + marker + `tail_chars` for one pass to land under budget;
    /// even if not, it still converges — an omitted entry is never omitted twice.
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
    pub aged_out: usize,
    /// Tool calls whose oversized arguments went.
    pub args_taken: usize,
    /// Notices cut back to their first line by the last rung.
    pub notices_pruned: usize,
    pub dropped: usize,
    /// The dropped span left a summary behind.
    pub summarized: bool,
    /// Even after dropping history the transcript is still over budget.
    pub still_over: bool,
}

impl Report {
    pub fn touched(&self) -> bool {
        self.superseded + self.aged_out + self.args_taken + self.notices_pruned + self.dropped > 0
    }
}

// One entry as the plan currently intends to leave it. `tokens` tracks the
// running estimate, so the budget check is a sum, not a re-walk each time.
struct Item<'a> {
    id: EntryId,
    entry: &'a Entry,
    tokens: usize,
    // What the view already shows in its place, from this pass or an earlier
    // one; `fresh` marks only this pass's decisions, so the record doesn't restate.
    notice: Option<String>,
    fresh: bool,
    gone: bool,
    // Blocks of this turn whose arguments this pass is taking. Separate from
    // `notice`: the entry is still shown, only its bulk is not.
    args_gone: Vec<usize>,
}

impl<'a> Item<'a> {
    fn result(&self) -> Option<&'a llm::message::ToolResult> {
        match self.entry {
            Entry::Tool { result: r, .. } if self.notice.is_none() => Some(r),
            _ => None,
        }
    }

    // The text an omission would stand in for: a tool's result or a `!`
    // command's output — the two things on the user's side nobody is waiting on.
    fn prunable(&self) -> Option<String> {
        if self.notice.is_some() {
            return None;
        }
        match self.entry {
            Entry::Tool { result: r, .. } => Some(r.flatten_text()),
            Entry::Bash { run, .. } => Some(run.text.clone()),
            _ => None,
        }
    }

    // Assistant turns are never taken: a `tool_use` with no answering
    // `tool_result` makes the next request invalid on both formats.
    fn omittable(&self) -> bool {
        match self.entry {
            Entry::Tool { .. } => true,
            // A `!` command's output is the other half of a question — bulk
            // nothing downstream waits on.
            Entry::Bash { .. } => true,
            // A note is the same half: machine prose nothing downstream waits
            // on once the run it explains is past.
            Entry::Note { .. } => true,
            // A question stays whatever the budget says: what someone asked is
            // not the answer's spare context.
            Entry::Ask { .. } => false,
            _ => false,
        }
    }

    fn omit(&mut self, notice: String) {
        self.tokens = omitted_tokens(self.entry, &notice);
        self.notice = Some(notice);
        self.fresh = true;
    }
}

/// What an entry costs once replaced by a notice. A result keeps its block —
/// the notice must still fill the `tool_result` its `tool_use` requires.
fn omitted_tokens(entry: &Entry, notice: &str) -> usize {
    estimate::MESSAGE_OVERHEAD
        + match entry {
            Entry::Tool { result, .. } => estimate::omitted_result(result, notice),
            _ => estimate::text(notice),
        }
}

// Per entry, not per wire message, so framing may be counted more than
// once — the safe direction, since compacting early costs less than late.
fn tokens_of(seen: &Seen<'_>, spec: &ModelSpec, gone: &HashMap<(EntryId, usize), &str>) -> usize {
    let body: usize = match seen.entry() {
        Entry::Answer { id, blocks, .. } => Session::shown_blocks(blocks, *id, gone)
            .iter()
            .map(|b| estimate::assistant_block(b, spec))
            .sum(),
        entry => user_block(entry).iter().map(estimate::user_block).sum(),
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

// One text block standing in for a pruned entry: notice, bounded head,
// a marker, bounded tail. Chars are code points, so slicing keeps pairs whole.
fn pruned(notice: &str, text: &str, policy: &Policy) -> String {
    let c = text.chars().count();
    if c <= policy.prune_chars {
        return notice.to_string();
    }
    let head = llm::slice::head_chars(text, policy.head_chars);
    let tail = llm::slice::tail_chars(text, policy.tail_chars);
    let dropped = c.saturating_sub(policy.head_chars + policy.tail_chars);
    format!("{notice}\n\n{head}\n\n[… {dropped} chars omitted …]\n\n{tail}")
}

// What every rung reads and none of them changes.
struct Frame<'a> {
    spec: &'a ModelSpec,
    budget: usize,
    policy: &'a Policy,
    already_gone: &'a HashMap<(EntryId, usize), &'a str>,
}

// One measure, applied until the transcript fits or the measure runs out.
type Rung = fn(&mut [Item<'_>], &Frame<'_>, &mut Report);

// Cheapest and least lossy first. Compaction knows the transcript's shape —
// calls, results, sizes, rounds — and never what any one tool means.
const RUNGS: [Rung; 5] = [repeated, age_out, take_args, trim_notices, drop_history];

fn total(items: &[Item<'_>]) -> usize {
    items.iter().map(|i| i.tokens).sum()
}

/// Decide how to shrink the session's context to fit `budget`, cheapest
/// measure first. Returns a record for the caller to append rather than
/// mutating the session. Content is replaced with a notice, not removed,
/// except when a whole exchange goes at once — leaving no orphaned `tool_use`.
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
                tokens: omitted_tokens(entry, notice),
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

    // Summaries `context` puts in the first user message aren't in the view,
    // so a planner that stops at items would undercount the real request.
    let summaries: usize = session
        .summaries()
        .iter()
        .map(|s| estimate::text(&injected_summary(s)))
        .sum();
    let before = total(&items) + summaries;
    let mut report = Report {
        before,
        ..Default::default()
    };

    if before > budget {
        let frame = Frame {
            spec,
            budget,
            policy,
            already_gone: &already_gone,
        };
        for rung in RUNGS {
            if total(&items) <= budget {
                break;
            }
            rung(&mut items, &frame, &mut report);
        }
    }

    let mut record = Compaction {
        tokens_before: before,
        ..Default::default()
    };
    for it in &items {
        if it.gone {
            record.dropped.push(it.id);
            continue;
        }
        for k in &it.args_gone {
            record.omissions.push(Omission {
                entry: it.id,
                block: Some(*k),
                notice: ARGS_TAKEN.to_string(),
            });
        }
        if let Some(notice) = &it.notice
            && it.fresh
        {
            record.omissions.push(Omission {
                entry: it.id,
                block: None,
                notice: notice.clone(),
            });
        }
    }

    record.tokens_after = if before > budget {
        total(&items)
    } else {
        before
    };
    report.after = record.tokens_after;
    report.still_over = report.after > budget;
    (record, report)
}

// A call made again with the same name and arguments: the later answer is the
// one that stands, so every earlier one is dead weight wherever it sits.
fn repeated<'a>(items: &mut [Item<'a>], _: &Frame<'_>, report: &mut Report) {
    let calls: HashMap<&'a str, String> = items
        .iter()
        .flat_map(|it| {
            let entry: &'a Entry = it.entry;
            entry.tool_calls()
        })
        .map(|c| (c.id.as_str(), format!("{}\0{}", c.name, c.args)))
        .collect();
    let key = |it: &Item<'_>| it.result().and_then(|r| calls.get(r.call.as_str()));
    let mut newest: HashMap<&String, usize> = HashMap::new();
    for (n, it) in items.iter().enumerate() {
        if let Some(k) = key(it) {
            newest.insert(k, n);
        }
    }
    for (n, it) in items.iter_mut().enumerate() {
        if key(it).is_some_and(|k| newest[k] != n) {
            it.omit(REPEATED.to_string());
            report.superseded += 1;
        }
    }
}

// Results and `!` command output, oldest first, never inside the tail the
// agent is working from. Both carry bulk nothing downstream waits on.
fn age_out(items: &mut [Item<'_>], f: &Frame<'_>, report: &mut Report) {
    let suffix = suffixes(items);
    for n in 0..items.len() {
        if total(items) <= f.budget || suffix[n] < f.policy.protect_tail {
            break;
        }
        if !items[n].omittable() {
            continue;
        }
        let Some(body) = items[n].prunable() else {
            continue;
        };
        items[n].omit(pruned(AGED_OUT, &body, f.policy));
        report.aged_out += 1;
    }
}

// Args duplicate work the result already recorded, so they're safe to drop.
// Reasoning blocks are not: the API filters and bills prior ones itself.
fn take_args(items: &mut [Item<'_>], f: &Frame<'_>, report: &mut Report) {
    let suffix = suffixes(items);
    let mut gone = f.already_gone.clone();
    for n in 0..items.len() {
        if total(items) <= f.budget || suffix[n] < f.policy.protect_tail {
            break;
        }
        let Entry::Answer { id, blocks, .. } = items[n].entry else {
            continue;
        };
        let fat: Vec<usize> = blocks
            .iter()
            .enumerate()
            .filter(|(k, b)| {
                !f.already_gone.contains_key(&(*id, *k))
                    && matches!(b, llm::message::AssistantContent::ToolCall(c)
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
                .map(|b| estimate::assistant_block(b, f.spec))
                .sum::<usize>();
        report.args_taken += items[n].args_gone.len();
    }
}

// The kept ends are a floor a window the provider just named can refuse:
// a pruned entry keeps only the notice that leads it.
fn trim_notices(items: &mut [Item<'_>], f: &Frame<'_>, report: &mut Report) {
    for n in 0..items.len() {
        if total(items) <= f.budget {
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
        report.notices_pruned += 1;
    }
}

// Last resort: history leaves the view, oldest first.
fn drop_history(items: &mut [Item<'_>], f: &Frame<'_>, report: &mut Report) {
    while total(items) > f.budget {
        let suffix = suffixes(items);
        let Some(doomed) = droppable(items, f.policy, &suffix) else {
            break;
        };
        for n in &doomed {
            items[*n].gone = true;
            items[*n].tokens = 0;
        }
        report.dropped += doomed.len();
    }
}

// A round is a prompt plus everything that answered it. A `!` command
// right before a prompt attaches to it, so both can be dropped together.
fn round_starts(items: &[Item<'_>]) -> Vec<usize> {
    let is_prompt = |it: &Item<'_>| matches!(it.entry, Entry::Ask { .. });
    let leads_in = |it: &Item<'_>| matches!(it.entry, Entry::Bash { .. });
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

// The entries of `span` that are still in the view, or `None` when the span
// holds nothing to take.
fn takeable(items: &[Item<'_>], span: std::ops::Range<usize>) -> Option<Vec<usize>> {
    let out: Vec<usize> = span.filter(|n| !items[*n].gone).collect();
    (!out.is_empty()).then_some(out)
}

// The first entry of a round's body — everything the prompt and its
// attachments are not.
fn after_prompt(items: &[Item<'_>], start: usize, end: usize) -> usize {
    items[start..end]
        .iter()
        .position(|it| matches!(it.entry, Entry::Ask { .. }))
        .map_or(start, |p| start + p + 1)
}

// A round — prompt plus its answers — is the drop unit, so a question is
// never orphaned; the current round falls back to exchange-by-exchange.
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
        if items[n].gone || !matches!(items[n].entry, Entry::Answer { .. }) {
            continue;
        }
        return Some(exchange(items, n));
    }
    None
}

// One exchange: an assistant turn plus its answers — droppable's fallback
// unit. Joined by call id, not adjacency, to keep every `tool_use` paired.
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
        let answers = matches!(it.entry, Entry::Tool { result: r, .. }
            if calls.contains(&r.call.as_str()));
        if answers && !it.gone {
            out.push(n);
        }
    }
    out
}
