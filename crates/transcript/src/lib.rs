use std::collections::{HashMap, HashSet};

use llm::message::{
    AssistantContent, Image, Message, Text, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId(pub u64);

/// Wall-clock seconds, the stamp every session artefact is dated by. Public
/// because the transcript store dates its files by the same clock, and two
/// clocks would date one session twice.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One entry the model no longer sees in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omission {
    pub entry: EntryId,
    /// Which block of an assistant turn, when what went is one tool call's
    /// arguments rather than the entry. `None` addresses the entry itself.
    ///
    /// Kept as narrow as the reason for it: only oversized tool-call
    /// arguments. Reasoning blocks are never touched — the API filters them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<usize>,
    pub notice: String,
}

/// An argument string longer than this is worth replacing with a notice. Read
/// by the planner deciding and by the view rebuilding, so the two agree
/// without the record having to spell out which strings went.
pub const ARG_CHARS: usize = 1_024;

/// A tool call as the model sees it once its bulk is gone: same id, same name,
/// every argument short enough to be worth keeping. A `write`'s path survives
/// and its file content does not.
pub fn omitted_args(call: &ToolCall, notice: &str) -> ToolCall {
    let mut out = call.clone();
    if let Some(map) = out.args.as_object_mut() {
        for v in map.values_mut() {
            if v.as_str().is_some_and(|t| t.chars().count() > ARG_CHARS) {
                *v = serde_json::Value::String(notice.to_string());
            }
        }
    }
    out
}

/// What the oversized arguments of `call` weigh in characters — nothing when
/// none of them is worth omitting.
pub fn oversized_args(call: &ToolCall) -> usize {
    call.args
        .as_object()
        .map(|m| {
            m.values()
                .filter_map(|v| v.as_str())
                .map(|t| t.chars().count())
                .filter(|n| *n > ARG_CHARS)
                .sum()
        })
        .unwrap_or(0)
}

/// A record of one compaction pass. It says what the model stopped seeing; it
/// does not remove anything, so the session keeps everything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Compaction {
    /// Entries the view skips entirely.
    pub dropped: Vec<EntryId>,
    /// Entries the view shows as a notice instead of their content.
    pub omissions: Vec<Omission>,
    /// Stands in for the dropped entries. Absent when nothing was dropped, or
    /// when summarizing failed — the entries still go, unsummarized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

/// One thing the user side said, kept in both voices: what the model reads,
/// and — when the two differ — what the person saw.
///
/// `text` reaches the wire; `shown` is the screen echo and rewind label;
/// `images` ride only with an ask. A note carries neither, hence a bare `String`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    /// What the model reads.
    pub text: String,
    /// Pictures pasted with the ask. An ask is the only carrier. Read from
    /// the single `image` older transcripts wrote, too.
    #[serde(
        default,
        alias = "image",
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub images: Vec<Image>,
    /// What a person reads — the rollback menu, `/resume` naming, the screen.
    /// `None` when it is the same as `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<String>,
    /// Set when pi sent this ask on the model's own behalf — a `later` come
    /// due, a background subagent's answer: what the model is told of where
    /// it came from. Nobody typed it, so `shown` names it rather than echoes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relayed: Option<String>,
}

fn one_or_many<'de, D>(de: D) -> Result<Vec<Image>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Many(Vec<Image>),
        One(Image),
    }
    Ok(match Either::deserialize(de)? {
        Either::Many(images) => images,
        Either::One(image) => vec![image],
    })
}

impl Prompt {
    /// What to put in front of a person. One question, answered here rather
    /// than at each call site, where the answers drift.
    pub fn shown_text(&self) -> &str {
        self.shown.as_deref().unwrap_or(&self.text)
    }
}

/// Who said or did a thing. Derived from the entry's variant, never stored:
/// the variant is the author, and a stored field could disagree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    User,
    Assistant,
    Agent,
}

/// The session's atom. Append only, or truncated by a rollback; the content of
/// an entry is never rewritten.
///
/// Flat on purpose: the author is the variant (see [`Entry::author`]), not a
/// field, so no illegal combination can be built.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    // Opens a round; compaction's drop unit is a round, so a question and
    // its answers go together or not at all. Never omitted on its own.
    Ask {
        id: EntryId,
        at: u64,
        ask: Prompt,
    },
    // A `!` command with its output. `run.text` is what the model reads;
    // `run.shown` is the `!cmd` line the screen echoes, both from one source.
    Bash {
        id: EntryId,
        at: u64,
        run: Prompt,
    },
    // One response, whole — never addressed block by block, which keeps a
    // `tool_use` beside the reasoning that produced it.
    Answer {
        id: EntryId,
        at: u64,
        blocks: Vec<AssistantContent>,
    },
    // A tool's result, plus the copy the screen drew for it. `preview` sits
    // beside `ToolResult` rather than inside it, so no wire encoder sends it.
    Tool {
        id: EntryId,
        at: u64,
        result: ToolResult,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    // Machine prose in the user's voice — a stopped run's cause, a round
    // note. The model reads it; the screen shows it muted. Omittable.
    Note {
        id: EntryId,
        at: u64,
        note: String,
    },
    // The session's own mechanism: one compaction pass's record. Input to the
    // view, never content on the wire.
    Compaction {
        id: EntryId,
        at: u64,
        record: Compaction,
    },
    // A row the screen shows and the model never reads: a tally line, a
    // turn warning. Kept so a rebuild draws the same screen the live path did.
    Screen {
        id: EntryId,
        at: u64,
        text: String,
    },
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Ask { id, .. }
            | Entry::Bash { id, .. }
            | Entry::Answer { id, .. }
            | Entry::Tool { id, .. }
            | Entry::Note { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::Screen { id, .. } => *id,
        }
    }

    pub fn at(&self) -> u64 {
        match self {
            Entry::Ask { at, .. }
            | Entry::Bash { at, .. }
            | Entry::Answer { at, .. }
            | Entry::Tool { at, .. }
            | Entry::Note { at, .. }
            | Entry::Compaction { at, .. }
            | Entry::Screen { at, .. } => *at,
        }
    }

    /// Who said or did this. The variant is the author; there is nothing to
    /// keep in step.
    pub fn author(&self) -> Author {
        match self {
            Entry::Ask { .. } | Entry::Bash { .. } => Author::User,
            Entry::Answer { .. } => Author::Assistant,
            Entry::Tool { .. }
            | Entry::Note { .. }
            | Entry::Compaction { .. }
            | Entry::Screen { .. } => Author::Agent,
        }
    }

    /// An assistant turn's blocks; `None` for every other kind.
    pub fn blocks(&self) -> Option<&[AssistantContent]> {
        match self {
            Entry::Answer { blocks, .. } => Some(blocks),
            _ => None,
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        let blocks = match self {
            Entry::Answer { blocks, .. } => blocks.as_slice(),
            _ => &[],
        };
        blocks.iter().filter_map(|b| match b {
            AssistantContent::ToolCall(c) => Some(c),
            _ => None,
        })
    }

    /// The prompt side of a user entry — an ask or a `!` command. `None` for
    /// every other kind.
    pub fn prompt(&self) -> Option<&Prompt> {
        match self {
            Entry::Ask { ask, .. } => Some(ask),
            Entry::Bash { run, .. } => Some(run),
            _ => None,
        }
    }
}

/// One place the conversation can be rewound to.
///
/// Not one operation with a flag: going back to something the user said
/// unsends it — text returns to the editor — while going back to an answer
/// keeps it, since that is what the conversation carries on from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Ask { id: EntryId, show: String },
    Reply { id: EntryId, show: String },
}

impl Node {
    pub fn id(&self) -> EntryId {
        match self {
            Node::Ask { id, .. } | Node::Reply { id, .. } => *id,
        }
    }

    pub fn show(&self) -> &str {
        match self {
            Node::Ask { show, .. } | Node::Reply { show, .. } => show,
        }
    }
}

// What an assistant turn said, when it said anything at all.
fn said(blocks: &[AssistantContent]) -> Option<String> {
    blocks.iter().find_map(|b| match b {
        AssistantContent::Text(t) if !t.text.trim().is_empty() => Some(t.text.clone()),
        _ => None,
    })
}

// The place this entry offers to go back to, or `None` when it is not one.
fn node_of(entry: &Entry) -> Option<Node> {
    match entry {
        Entry::Ask { id, ask, .. } => Some(Node::Ask {
            id: *id,
            show: ask.shown_text().to_string(),
        }),
        Entry::Bash { id, run, .. } => Some(Node::Ask {
            id: *id,
            show: run.shown_text().to_string(),
        }),
        Entry::Answer { id, blocks, .. } => said(blocks).map(|show| Node::Reply { id: *id, show }),
        _ => None,
    }
}

/// One entry as the model currently sees it. Both shapes answer `id`, which is
/// the whole point: compaction needs the content to measure and the id to
/// record, and reading them from two lists is what let them drift apart.
#[derive(Debug, Clone, Copy)]
pub enum Seen<'a> {
    As(&'a Entry),
    // Content replaced, shell kept — a `tool_use` must keep its `tool_result`.
    Omitted { entry: &'a Entry, notice: &'a str },
}

impl<'a> Seen<'a> {
    pub fn id(&self) -> EntryId {
        match self {
            Seen::As(e) | Seen::Omitted { entry: e, .. } => e.id(),
        }
    }

    pub fn entry(&self) -> &'a Entry {
        match self {
            Seen::As(e) | Seen::Omitted { entry: e, .. } => e,
        }
    }
}

/// The whole conversation: every prompt, tool result, response, and
/// compaction record, in order.
///
/// Held by the caller so a run that errors still leaves everything it
/// produced. What the model sees is *derived* from this — never stored in
/// place of it — since compaction writes a record and `view` applies it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    entries: Vec<Entry>,
    #[serde(default)]
    next: u64,
    // Why the last run ended unanswered. Cleared by the next prompt, or by
    // a rewind that cuts the round it describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interrupted: Option<StopCause>,
}

// Why the most recent run ended before its prompt was answered, if it did:
// the transcript alone can't say whose stop it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum StopCause {
    // The user asked the run to stop: Esc, `/stop`, an interrupt.
    User,
    // It died on its own — an error or a crash — and no more is known.
    Other,
    // It died with a specific error.
    Error(String),
}

impl StopCause {
    fn note(&self) -> Option<String> {
        match self {
            Self::User => None,
            Self::Other => Some(stopped_note(None)),
            Self::Error(e) => Some(stopped_note(Some(e))),
        }
    }
}

/// What an unanswered call is closed with when stopped by the user.
pub const STOPPED_CALL: &str = "The user stopped this call before it returned.";

/// What an unanswered call is closed with when the run died under it.
pub const DIED_CALL: &str =
    "The run died before this call returned; whether it took effect is unknown.";

/// Whether a tool result was synthesized to close an interrupted call.
pub fn is_stopped_call(r: &ToolResult) -> bool {
    r.content.iter().any(|c| match c {
        // The bare stem also matches older wording, still recorded in
        // transcripts written before it changed.
        ToolResultContent::Text(t) => {
            t.text.starts_with("The user stopped this call") || t.text == DIED_CALL
        }
        _ => false,
    })
}

// What the model is told after a run that died for an unknown reason.
const STOPPED_UNKNOWN: &str = "The previous run ended before it finished, for an unknown \
     reason. Treat the request it was working on as unresolved; the message below is what to act on.";

// What the model reads. Everything else in the list is there for a view: the
// record of a compaction pass, and a row only the screen ever knew.
fn is_content(entry: &Entry) -> bool {
    !matches!(entry, Entry::Compaction { .. } | Entry::Screen { .. })
}

fn stopped_note(err: Option<&str>) -> String {
    let Some(e) = err.map(str::trim).filter(|s| !s.is_empty()) else {
        return STOPPED_UNKNOWN.to_string();
    };
    format!(
        "The previous run ended before it finished, with an error: {e}. \
         Treat the request it was working on as unresolved; the message below is what to act on."
    )
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// A session that starts from a single user prompt.
    pub fn with_prompt(prompt: impl Into<String>) -> Self {
        let mut session = Self::new();
        session.prompt(prompt);
        session
    }

    /// Build a session from a transcript's worth of messages. A user
    /// message's text and image merge into one ask; each tool result lands
    /// as its own entry. Nothing in the run needs this — a message is the
    /// wire shape, an entry the session's — but a test wants it.
    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        let mut log = Self::new();
        for m in messages {
            match m {
                Message::User { content } => {
                    // The ask a message's text and image gather into; a tool
                    // result in the middle flushes it, so block order holds.
                    let mut pending: Option<Prompt> = None;
                    let flush = |log: &mut Self, pending: &mut Option<Prompt>| {
                        if let Some(ask) = pending.take() {
                            log.push_ask(ask);
                        }
                    };
                    for b in content {
                        match b {
                            UserContent::Text(t) => match &mut pending {
                                Some(ask) => {
                                    ask.text.push('\n');
                                    ask.text.push_str(&t.text);
                                }
                                None => {
                                    pending = Some(Prompt {
                                        text: t.text,
                                        images: Vec::new(),
                                        shown: None,
                                        relayed: None,
                                    });
                                }
                            },
                            UserContent::Image(i) => match &mut pending {
                                Some(ask) => ask.images.push(i),
                                None => {
                                    pending = Some(Prompt {
                                        text: String::new(),
                                        images: vec![i],
                                        shown: None,
                                        relayed: None,
                                    });
                                }
                            },
                            UserContent::ToolResult(r) => {
                                flush(&mut log, &mut pending);
                                log.push_previewed(vec![(r, None)]);
                            }
                        }
                    }
                    flush(&mut log, &mut pending);
                }
                Message::Assistant { content, .. } => {
                    log.push_assistant(content);
                }
                Message::System { .. } => {}
            }
        }
        log
    }

    fn mint(&mut self) -> (EntryId, u64) {
        let id = EntryId(self.next);
        self.next += 1;
        (id, now())
    }

    /// Text from the person at the keyboard.
    pub fn prompt(&mut self, text: impl Into<String>) -> EntryId {
        self.push_ask(Prompt {
            text: text.into(),
            images: Vec::new(),
            shown: None,
            relayed: None,
        })
    }

    fn push_ask(&mut self, ask: Prompt) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Ask { id, at, ask });
        id
    }

    /// A `!` command and its output, filed by the door that ran it.
    pub fn push_bash(&mut self, run: Prompt) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Bash { id, at, run });
        id
    }

    /// Machine prose in the user's voice — the loop's round number, a stopped
    /// run's cause. Read by the model as if the user said it, which is why
    /// anything pushed here must survive being read that way. Shown on screen
    /// as a notice.
    pub fn push_note(&mut self, text: impl Into<String>) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Note {
            id,
            at,
            note: text.into(),
        });
        id
    }

    /// A row for the screen alone: a run's tally line, a warning about the
    /// turn. Never content on the wire — the model is told what it needs in
    /// prose it can act on, and a status line is not that. Kept so the rebuild
    /// draws the same screen the live path drew.
    pub fn push_screen(&mut self, text: impl Into<String>) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Screen {
            id,
            at,
            text: text.into(),
        });
        id
    }

    /// One entry per result: compaction decides about them one at a time.
    /// Each result becomes its own entry; joining them into one wire message is
    /// the encoder's business.
    pub fn push_results(&mut self, results: Vec<ToolResult>) -> Vec<EntryId> {
        self.push_previewed(results.into_iter().map(|r| (r, None)).collect())
    }

    /// The same, carrying the copy the screen drew for each — an edit's diff
    /// rows, which the rebuild must draw rather than read back from the result.
    pub fn push_previewed(&mut self, results: Vec<(ToolResult, Option<String>)>) -> Vec<EntryId> {
        results
            .into_iter()
            .map(|(result, preview)| {
                let (id, at) = self.mint();
                self.entries.push(Entry::Tool {
                    id,
                    at,
                    result,
                    preview,
                });
                id
            })
            .collect()
    }

    pub fn push_assistant(&mut self, blocks: Vec<AssistantContent>) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Answer { id, at, blocks });
        id
    }

    pub fn record(&mut self, record: Compaction) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Compaction { id, at, record });
        id
    }

    /// The run that just ended died of `why` before answering its prompt,
    /// recorded for [`Session::send_prompt`] to name.
    pub fn note_failure(&mut self, why: String) {
        self.interrupted = Some(StopCause::Error(why));
    }

    /// The run that just ended was stopped by the user before it answered.
    pub fn note_user_stop(&mut self) {
        self.interrupted = Some(StopCause::User);
    }

    // Feed a cause straight in, for tests shaping a session by hand.
    #[cfg(test)]
    fn mark_stopped(&mut self, cause: StopCause) {
        self.interrupted = Some(cause);
    }

    /// Continue with a new prompt, repairing a turn that died mid-call:
    /// unanswered tool calls are closed with a result naming the
    /// interruption, and an unknown failure adds a note first. `shown` is
    /// what the user typed, when it differs from what the model reads.
    pub fn send_prompt(&mut self, prompt: impl Into<String>, shown: Option<String>) {
        self.send_prompt_with(prompt, shown, Vec::new());
    }

    /// [`Session::send_prompt`], with pictures riding the ask.
    pub fn send_prompt_with(
        &mut self,
        prompt: impl Into<String>,
        shown: Option<String>,
        images: Vec<Image>,
    ) {
        self.send(Prompt {
            text: prompt.into(),
            images,
            shown,
            relayed: None,
        });
    }

    /// Continue with an ask pi sends on the model's behalf: `label` is what
    /// the screen names it, `note` what the model is told of where it came from.
    pub fn send_relayed(&mut self, prompt: impl Into<String>, label: String, note: String) {
        self.send(Prompt {
            text: prompt.into(),
            images: Vec::new(),
            shown: Some(label),
            relayed: Some(note),
        });
    }

    fn send(&mut self, ask: Prompt) {
        let answered: HashSet<&str> = self
            .entries
            .iter()
            .rev()
            .take_while(|e| !matches!(e, Entry::Answer { .. }))
            .filter_map(|e| match e {
                Entry::Tool { result: r, .. } => Some(r.call.as_str()),
                _ => None,
            })
            .collect();
        let unanswered: Vec<ToolCall> = self
            .entries
            .iter()
            .rev()
            .find(|e| matches!(e, Entry::Answer { .. }))
            .into_iter()
            .flat_map(Entry::tool_calls)
            .filter(|c| !answered.contains(c.id.as_str()))
            .cloned()
            .collect();
        // Calls left open with no cause on record: the process died mid-run.
        let cause = self
            .interrupted
            .take()
            .or_else(|| (!unanswered.is_empty()).then_some(StopCause::Other));
        let closing = match cause {
            Some(StopCause::User) => STOPPED_CALL,
            _ => DIED_CALL,
        };
        for c in unanswered {
            self.push_previewed(vec![(ToolResult::text(c.id, c.name, closing), None)]);
        }
        // A run that died tells the model so; a user stop leaves direction to
        // the next prompt.
        if let Some(note) = cause.and_then(|c| c.note()) {
            self.push_note(note);
        }
        self.push_ask(ask);
    }

    /// Everywhere the conversation can be rewound to, in session order:
    /// what the user said (asks and `!` asides), never an answer.
    ///
    /// Reads `history`, not `view`: a compacted-away prompt stays reachable,
    /// and rewinding past its compaction entry undoes the compaction too.
    pub fn rewind_nodes(&self) -> Vec<Node> {
        self.entries
            .iter()
            .filter_map(node_of)
            .filter(|node| matches!(node, Node::Ask { .. }))
            .collect()
    }

    /// Where the transcript ends now, which is what a rewind's notice names.
    /// Walks back to the first one instead of building the whole list.
    pub fn last_node(&self) -> Option<Node> {
        self.entries.iter().rev().find_map(node_of)
    }

    /// The text to hand back to the editor when this entry is unsent; `None`
    /// for anything the user did not say, which is what tells the two rewind
    /// semantics apart.
    pub fn unsent_text(&self, entry: EntryId) -> Option<String> {
        self.entries
            .iter()
            .find(|e| e.id() == entry)
            .and_then(Entry::prompt)
            .map(|p| p.shown_text().to_string())
    }

    /// The last question the user asked, when there is one to take back.
    ///
    /// Prompts only: a `!` command's output is not something anyone sent, and
    /// unsending it would put a line back in the editor that was never typed
    /// as a question.
    pub fn last_ask(&self) -> Option<EntryId> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::Ask { id, .. } => Some(*id),
            _ => None,
        })
    }

    /// Rewind to an entry, keeping it: everything after is removed and the
    /// count returned.
    ///
    /// Removed, not compacted: a `Compaction` entry caught in the cut takes
    /// its record with it, so the content that pass dropped comes back.
    pub fn rollback_to(&mut self, entry: EntryId) -> usize {
        self.truncate(entry, true)
    }

    /// Rewind to just before an entry: it goes too, along with everything
    /// after it. What unsending a message does — the message has to leave the
    /// transcript, or the editor and the model both hold it.
    pub fn rollback_before(&mut self, entry: EntryId) -> usize {
        self.truncate(entry, false)
    }

    fn truncate(&mut self, entry: EntryId, keep: bool) -> usize {
        let Some(at) = self.entries.iter().position(|e| e.id() == entry) else {
            return 0;
        };
        let keep = at + usize::from(keep);
        let removed = self.entries.len() - keep;
        self.entries.truncate(keep);
        // The marker described the round the cut just removed; keeping it
        // would name a death the transcript no longer shows.
        self.interrupted = None;
        removed
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Whether this session has anything the model would be told. The record of
    /// a compaction and a row only the screen reads are both input to a view
    /// rather than content, so a session holding one of them and nothing else
    /// is empty — the same rule `view` reads them by.
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|e| !is_content(e))
    }

    fn dropped(&self) -> HashSet<EntryId> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Compaction { record, .. } => Some(record.dropped.iter().copied()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    // Later passes win: an entry omitted then re-omitted shows the newer
    // notice. Whole-entry only — block-level lives in `block_omissions`.
    fn omissions(&self) -> HashMap<EntryId, &str> {
        // An assistant turn is never whole-entry omitted — its `tool_use`
        // blocks must stay legal, so only block-level omissions apply to it.
        let answers: HashSet<EntryId> = self
            .entries
            .iter()
            .filter(|e| matches!(e, Entry::Answer { .. }))
            .map(Entry::id)
            .collect();
        let mut out = HashMap::new();
        for e in &self.entries {
            if let Entry::Compaction { record, .. } = e {
                for el in record.omissions.iter().filter(|o| o.block.is_none()) {
                    if !answers.contains(&el.entry) {
                        out.insert(el.entry, el.notice.as_str());
                    }
                }
            }
        }
        out
    }

    /// Arguments the model no longer sees, by the block that held them.
    pub fn block_omissions(&self) -> HashMap<(EntryId, usize), &str> {
        let mut out = HashMap::new();
        for e in &self.entries {
            if let Entry::Compaction { record, .. } = e {
                for el in &record.omissions {
                    if let Some(n) = el.block {
                        out.insert((el.entry, n), el.notice.as_str());
                    }
                }
            }
        }
        out
    }

    /// An assistant turn's blocks as the model sees them.
    pub fn shown_blocks(
        blocks: &[AssistantContent],
        id: EntryId,
        gone: &HashMap<(EntryId, usize), &str>,
    ) -> Vec<AssistantContent> {
        if gone.is_empty() {
            return blocks.to_vec();
        }
        blocks
            .iter()
            .enumerate()
            .map(|(n, b)| match (b, gone.get(&(id, n))) {
                (AssistantContent::ToolCall(c), Some(notice)) => {
                    AssistantContent::ToolCall(omitted_args(c, notice))
                }
                _ => b.clone(),
            })
            .collect()
    }

    /// Everything, in order, compaction or no — what a person can see, as
    /// against `view` (what the model can). Used by the screen, the rewind
    /// menu, and the session's own name.
    pub fn history(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Every entry the model has stopped seeing — dropped, or shown as a
    /// notice. What the screen marks rather than hides.
    ///
    /// The whole set, not one lookup: per-entry answers would rebuild both
    /// maps every time — quadratic, right when the transcript is longest.
    pub fn out_of_view(&self) -> HashSet<EntryId> {
        let mut out = self.dropped();
        out.extend(self.omissions().keys().copied());
        out
    }

    pub fn view(&self) -> Vec<Seen<'_>> {
        let dropped = self.dropped();
        let omissions = self.omissions();
        self.entries
            .iter()
            .filter(|e| is_content(e))
            .filter(|e| !dropped.contains(&e.id()))
            .map(|entry| match omissions.get(&entry.id()) {
                Some(notice) => Seen::Omitted { entry, notice },
                None => Seen::As(entry),
            })
            .collect()
    }

    /// Summaries still in force, oldest first. A compaction entry can itself be
    /// dropped — that is how a fresh summary replaces the one it folded in,
    /// instead of the view accumulating one section per pass.
    pub fn summaries(&self) -> Vec<&str> {
        let dropped = self.dropped();
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Compaction { id, record, .. } if !dropped.contains(id) => {
                    record.summary.as_deref()
                }
                _ => None,
            })
            .collect()
    }

    /// Ids of compaction entries carrying a summary, for a later pass to retire.
    pub fn summary_entries(&self) -> Vec<EntryId> {
        let dropped = self.dropped();
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Compaction { id, record, .. }
                    if record.summary.is_some() && !dropped.contains(id) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect()
    }

    /// The entries behind a set of ids, in session order.
    pub fn entries_for(&self, ids: &[EntryId]) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| ids.contains(&e.id()))
            .collect()
    }

    pub fn context(&self) -> Vec<Message> {
        let summaries = self.summaries();
        let gone = self.block_omissions();
        let mut out: Vec<Message> = Vec::new();
        let mut first_user = true;

        for seen in self.view() {
            let blocks = match seen {
                Seen::As(Entry::Answer { id, blocks, .. }) => {
                    out.push(Message::Assistant {
                        content: Self::shown_blocks(blocks, *id, &gone),
                    });
                    continue;
                }
                // An assistant turn is never omitted: its `tool_use` blocks
                // must stay to keep the answering results legal.
                Seen::Omitted {
                    entry: Entry::Answer { id, blocks, .. },
                    ..
                } => {
                    out.push(Message::Assistant {
                        content: Self::shown_blocks(blocks, *id, &gone),
                    });
                    continue;
                }
                Seen::As(entry) => user_block(entry),
                Seen::Omitted { entry, notice } => omitted_block(entry, notice),
            };

            let mut content = blocks;
            if first_user {
                first_user = false;
                for s in &summaries {
                    content.push(UserContent::Text(Text {
                        text: injected_summary(s),
                    }));
                }
            }
            out.push(Message::User { content });
        }
        out
    }
}

/// One summary as `context` sends it. Named here because the estimate of a
/// transcript has to count the same bytes, wrapper included.
pub fn injected_summary(s: &str) -> String {
    format!("<earlier-work>\n{s}\n</earlier-work>")
}

/// The wire blocks one entry projects to. Empty for what never reaches the
/// wire; callers skip it. `shown` and `preview` stay behind — the screen's
/// fields are not the model's.
pub fn user_block(entry: &Entry) -> Vec<UserContent> {
    match entry {
        Entry::Ask { ask, .. } => {
            let mut out: Vec<UserContent> = ask
                .relayed
                .iter()
                .map(|note| UserContent::Text(Text { text: note.clone() }))
                .collect();
            // An image-only ask sends no text block: providers reject the
            // empty one.
            if !ask.text.is_empty() || ask.images.is_empty() {
                out.push(UserContent::Text(Text {
                    text: ask.text.clone(),
                }));
            }
            out.extend(ask.images.iter().cloned().map(UserContent::Image));
            out
        }
        Entry::Bash { run, .. } => vec![UserContent::Text(Text {
            text: run.text.clone(),
        })],
        Entry::Note { note, .. } => vec![UserContent::Text(Text { text: note.clone() })],
        Entry::Tool { result, .. } => vec![UserContent::ToolResult(result.clone())],
        _ => Vec::new(),
    }
}

/// The blocks an omitted entry keeps. A result must stay a result, or the
/// `tool_use` it answers is left dangling.
pub fn omitted_block(entry: &Entry, notice: &str) -> Vec<UserContent> {
    match entry {
        Entry::Tool { result: r, .. } => {
            let mut out = r.clone();
            out.content = vec![ToolResultContent::Text(Text {
                text: notice.to_string(),
            })];
            vec![UserContent::ToolResult(out)]
        }
        _ => vec![UserContent::Text(Text {
            text: notice.to_string(),
        })],
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use llm::message::{Text as MsgText, ToolCall, ToolResult};

    // The contract the Anthropic encoder's join is written against: joining
    // per-turn messages into one is the wire's business, not this projection's.
    #[test]
    fn an_ask_written_with_one_image_reads_back_as_a_list_of_one() {
        let old = r#"{"text":"look","image":{"source":"url","url":"http://x/i.png"}}"#;
        let ask: Prompt = serde_json::from_str(old).unwrap();
        assert_eq!(ask.images.len(), 1);
        let many =
            r#"{"text":"look","images":[{"source":"url","url":"a"},{"source":"url","url":"b"}]}"#;
        assert_eq!(
            serde_json::from_str::<Prompt>(many).unwrap().images.len(),
            2
        );
        let none: Prompt = serde_json::from_str(r#"{"text":"look"}"#).unwrap();
        assert!(none.images.is_empty());
        assert!(!serde_json::to_string(&none).unwrap().contains("image"));
    }

    #[test]
    fn the_view_hands_over_one_message_per_entry() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "on it".into(),
        })]);
        s.push_results(vec![
            ToolResult::text("c1", "read", "a"),
            ToolResult::text("c2", "grep", "b"),
        ]);
        s.push_ask(Prompt {
            text: "and now this".into(),
            images: vec![Image::Url {
                url: "http://x/i.png".into(),
            }],
            shown: None,
            relayed: None,
        });

        let msgs = s.context();
        assert_eq!(msgs.len(), s.view().len());
        for m in &msgs {
            if let Message::User { content } = m {
                assert!(
                    (1..=2).contains(&content.len()),
                    "a user message carried more than its entry"
                );
            }
        }
    }

    // What names a session is read out of the archive, so a compaction that
    // removed it would rename the session the first time the window filled.
    #[test]
    fn compacting_the_opening_turn_leaves_it_in_the_transcript() {
        let mut s = Session::new();
        let first = s.prompt("why is the flaky test flaky?");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "looking".into(),
        })]);
        s.prompt("and the other one?");

        s.record(Compaction {
            dropped: vec![first],
            ..Default::default()
        });

        // Gone from what the model reads, still on disk and still first.
        assert!(!s.view().iter().any(|seen| seen.id() == first));
        assert!(s.out_of_view().contains(&first));
        assert_eq!(s.history().count(), 4, "nothing left the transcript");
        assert_eq!(s.rewind_nodes().first().map(Node::id), Some(first));
    }

    // And because the menu can still name it, rewinding to it truncates the
    // compaction entry too — which puts the dropped turns back.
    #[test]
    fn rewinding_past_a_compaction_undoes_it() {
        let mut s = Session::new();
        let first = s.prompt("the task");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "on it".into(),
        })]);
        let second = s.prompt("more");
        s.record(Compaction {
            dropped: vec![first],
            ..Default::default()
        });

        assert!(
            s.rewind_nodes().iter().any(|n| n.id() == first),
            "the menu must reach it"
        );
        s.rollback_to(second);

        assert!(
            !s.out_of_view().contains(&first),
            "the compaction went with the rewind"
        );
        assert!(s.view().iter().any(|seen| seen.id() == first));
    }

    // Unsending is the other half of the rewind: the message itself has to
    // leave, or the editor holds a line the model is still being sent.
    #[test]
    fn unsending_a_message_takes_it_out_of_the_transcript() {
        let mut s = Session::new();
        s.prompt("the first thing");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "done".into(),
        })]);
        let second = s.prompt("teh typo one");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "answering".into(),
        })]);

        assert_eq!(s.unsent_text(second).as_deref(), Some("teh typo one"));
        assert_eq!(s.last_ask(), Some(second));
        assert_eq!(
            s.rollback_before(second),
            2,
            "the message and the answer to it"
        );
        assert!(!s.history().any(|e| e.id() == second));
        assert_eq!(s.rewind_nodes().len(), 1, "the first turn, the ask alone");
    }

    // The menu lists what was said, never what came back: an answer is a
    // place to continue from, a tool-only turn just a step of the work.
    #[test]
    fn only_asks_reach_the_rewind_menu() {
        let mut s = Session::new();
        let ask = s.prompt("read it");
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: serde_json::json!({ "path": "f.rs" }),
        })]);
        s.push_results(vec![ToolResult::text("c1", "read", "a")]);
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "it says a".into(),
        })]);

        let nodes = s.rewind_nodes();
        assert_eq!(nodes.len(), 1, "the question, and no answer beside it");
        assert_eq!(nodes[0].id(), ask);
        assert!(matches!(nodes[0], Node::Ask { .. }));
    }

    // An answer and a user stop add no note; a death is named, with its
    // cause where known. One note per dead run; sends after stay clean.
    #[test]
    fn the_outcome_of_a_run_decides_the_note_before_the_next_prompt() {
        // An answer that did full tool work asks for no note.
        let mut answered = Session::new();
        answered.prompt("go");
        answered.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: serde_json::json!({}),
        })]);
        answered.push_results(vec![ToolResult::text("c1", "read", "a")]);
        answered.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "it says a".into(),
        })]);

        let stopped = |cause: StopCause| {
            let mut s = Session::new();
            s.prompt("go");
            s.mark_stopped(cause);
            s
        };
        let failed = |why: &str| {
            let mut s = Session::new();
            s.prompt("go");
            s.note_failure(why.into());
            s
        };

        for (why, mut s, note) in [
            ("an answer", answered, None),
            ("a user stop", stopped(StopCause::User), None),
            (
                "a death of no known cause",
                stopped(StopCause::Other),
                Some(STOPPED_UNKNOWN.to_string()),
            ),
            (
                "a death with a cause",
                failed("stream: died"),
                Some(stopped_note(Some("stream: died"))),
            ),
        ] {
            s.send_prompt("and now this", None);
            s.send_prompt("and still this", None);

            let entries = s.entries();
            match &note {
                Some(text) => match &entries[entries.len() - 3] {
                    Entry::Note { note: got, .. } => assert_eq!(got, text, "{why}"),
                    other => panic!("{why}: expected the note, got {other:?}"),
                },
                None => assert!(
                    !entries.iter().any(|e| matches!(e, Entry::Note { .. })),
                    "{why}: no note"
                ),
            }
            assert!(
                matches!(&entries[entries.len() - 1], Entry::Ask { .. }),
                "{why}: the newest prompt is last"
            );
        }
    }

    // Kept so a rebuild draws what the live path drew — but a status line in
    // the prompt would be context spent on numbers nothing can act on.
    #[test]
    fn a_screen_row_is_kept_and_never_read() {
        let mut s = Session::new();
        s.prompt("go");
        let id = s.push_screen("12s · 1.2k/340 · $0.0123");

        assert!(
            s.entries().iter().any(|e| e.id() == id),
            "the row is part of what happened"
        );
        assert!(
            !s.view().iter().any(|seen| seen.id() == id),
            "and not part of what the model is told"
        );
        // And it travels with the archive, so a rebuild after a resume has it.
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    // The note is the session's words, not the user's: it stays out of the
    // rewind menu, and nothing an unsend hands back to the editor.
    #[test]
    fn a_stop_note_is_model_only_and_not_rewindable() {
        let mut s = Session::new();
        s.prompt("version up");
        s.mark_stopped(StopCause::Other);
        s.send_prompt("delete the branch", None);

        let entries = s.entries();
        let note = entries[1].id();
        assert_eq!(
            s.unsent_text(note),
            None,
            "not the user's words to take back"
        );
        assert_eq!(
            s.rewind_nodes().len(),
            2,
            "the two asks only — the note is not a place to rewind to"
        );

        // The note travels with the archive, and comes back whole.
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn rewinding_drops_the_stop_marker() {
        let mut s = Session::new();
        let ask = s.prompt("the task");
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({}),
        })]);
        s.mark_stopped(StopCause::User);
        s.rollback_before(ask);

        s.send_prompt("a fresh start", None);

        let entries = s.entries();
        assert_eq!(entries.len(), 1, "only the new prompt follows the rewind");
        assert!(matches!(&entries[0], Entry::Ask { .. }));
    }

    // An open call is closed with what is known of why: the user stopped it,
    // or the run died under it — an error, or a crash that left no cause.
    #[test]
    fn an_unanswered_tool_call_is_closed_with_why_it_never_returned() {
        let open = |cause: Option<StopCause>| {
            let mut s = Session::new();
            s.prompt("run something");
            s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "cargo check" }),
            })]);
            if let Some(cause) = cause {
                s.mark_stopped(cause);
            }
            s.send_prompt("actually do this", None);
            s
        };
        for (why, cause, closing, noted) in [
            ("a user stop", Some(StopCause::User), STOPPED_CALL, false),
            (
                "an error",
                Some(StopCause::Error("died".into())),
                DIED_CALL,
                true,
            ),
            ("a crash", None, DIED_CALL, true),
        ] {
            let s = open(cause);
            let entries = s.entries();
            let result = match &entries[2] {
                Entry::Tool { result, .. } => result,
                other => panic!("{why}: expected the closing result, got {other:?}"),
            };
            assert!(!result.is_error, "{why}: a closed call is not an error");
            assert!(is_stopped_call(result), "{why}");
            assert_eq!(result.flatten_text(), closing, "{why}");
            let note = entries.iter().any(|e| matches!(e, Entry::Note { .. }));
            assert_eq!(note, noted, "{why}");
        }
    }

    #[test]
    fn a_stopped_call_in_the_pre_05e26ec_wording_is_still_recognised() {
        let result = ToolResult::text(
            "c1",
            "bash",
            "The user stopped this call before it returned; nothing about the call itself failed.",
        );
        assert!(is_stopped_call(&result));
    }

    // Rewinding to an answer is the opposite call: the answer stays, and the
    // conversation continues from it.
    #[test]
    fn rewinding_to_an_answer_keeps_it() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "here".into(),
        })]);
        let reply = s.last_node().map(|n| n.id()).expect("an answer");
        s.prompt("and then");

        assert_eq!(s.rollback_to(reply), 1);
        assert!(s.history().any(|e| e.id() == reply), "the answer stays");
        assert_eq!(
            s.unsent_text(reply),
            None,
            "nothing goes back to the editor"
        );
    }

    // The one user message that legitimately carries more than one block.
    #[test]
    fn summaries_ride_the_first_user_message_rather_than_one_of_their_own() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Text(MsgText {
            text: "done".into(),
        })]);
        s.record(Compaction {
            summary: Some("earlier: read two files".into()),
            ..Default::default()
        });

        let msgs = s.context();
        let Message::User { content } = &msgs[0] else {
            panic!("the first message is the opening prompt")
        };
        assert_eq!(content.len(), 2);
        assert!(matches!(&content[1], UserContent::Text(t) if t.text.contains("<earlier-work>")));
    }

    #[test]
    fn a_relayed_ask_tells_the_model_where_it_came_from_and_survives_a_save() {
        let mut s = Session::new();
        s.send_relayed("the answer", "subagent #1 find".into(), "Not typed.".into());
        let texts = |s: &Session| -> Vec<String> {
            s.context()
                .into_iter()
                .flat_map(|m| match m {
                    Message::User { content } => content,
                    _ => Vec::new(),
                })
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(texts(&s), ["Not typed.", "the answer"]);

        let back: Session = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(texts(&back), ["Not typed.", "the answer"]);
        let Some(Entry::Ask { ask, .. }) = back.entries().last() else {
            panic!("no ask");
        };
        assert_eq!(ask.shown_text(), "subagent #1 find");
        let typed: Prompt = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert!(typed.relayed.is_none());
    }
}
