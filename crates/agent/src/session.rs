use std::collections::{HashMap, HashSet};

use brain::message::{
    AssistantContent, Image, Message, Text, ToolCall, ToolResult, ToolResultContent, UserContent,
};
use serde::{Deserialize, Serialize};
use crate::{AgentError, Totals};

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
    /// The first crack in "an assistant turn is addressed whole", and kept as
    /// narrow as the reason for it: only a `tool_use`'s oversized arguments.
    /// Its thinking blocks are never touched — the API filters prior ones on
    /// its own and does not bill them, and the last turn's may not be edited
    /// at all.
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

/// Text from the user's side of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserText {
    /// What the model reads.
    pub text: String,
    /// What a person reads — the rollback menu, `/resume` naming, the screen.
    /// `None` when it is the same as `text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown: Option<String>,
}

impl UserText {
    /// What to put in front of a person. One question, answered here rather
    /// than at each call site, where the answers drift.
    pub fn shown_text(&self) -> &str {
        self.shown.as_deref().unwrap_or(&self.text)
    }
}

/// The user side, one block per entry. Splitting them is what lets compaction
/// address a single tool result — or a single pasted command's output —
/// without touching what sits beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserBody {
    /// What the user asked. It opens a round, and the drop tier's unit is a
    /// round: a question and everything that answered it go together or not at
    /// all. Never omitted — what someone asked is not the answer's spare
    /// context.
    Prompt(UserText),
    /// User-side text that is not a question: the output of a `!` command.
    ///
    /// A separate variant rather than a flag, because four places ask whether
    /// an entry is a question and three of them need a different answer for
    /// this one — it opens no round, it does not name the session, and it *is*
    /// omittable, since nothing downstream is waiting on an answer to it. A
    /// boolean cannot carry four answers, and inferring them from `shown` is
    /// what let a `!cargo test` be dropped out from under the `fix that` that
    /// referred to it.
    Aside(UserText),

    /// Machine-authored text meant for the model, not the person: what the
    /// session says about a run that ended without answering its prompt. It
    /// opens no round and names no session, and it is omittable, like an
    /// [`Aside`] — but unlike one it is not the user's words, so nothing may
    /// treat it as theirs: no rewind node, nothing an unsend hands back to
    /// the editor. That one difference is why it is a variant of its own.
    Note(UserText),
    /// The result, and what the screen showed for it when that is more than the
    /// result's own first line — the rows an edit sketched, which the stored
    /// content does not contain.
    ///
    /// Beside the result rather than inside it. `ToolResult` is the wire type:
    /// a screen-only field there would be one every encoder has to remember not
    /// to send, and one the token estimate would count for bytes that never
    /// leave. Here neither is possible — `brain` cannot see this type, and
    /// `user_block` carries only the `ToolResult` across.
    ///
    /// The same distinction `UserText` makes with `shown`, one variant along:
    /// what the model reads, beside what a person sees.
    Result {
        result: ToolResult,
        preview: Option<String>,
    },
    Image(Image),
}

/// The session's atom. Append only, or truncated by a rollback; the content of
/// an entry is never rewritten.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    User {
        id: EntryId,
        at: u64,
        body: UserBody,
    },
    /// One response, whole. Its blocks are never addressed separately: nothing
    /// reads them apart, and holding them together is what keeps a `tool_use`
    /// beside the reasoning that produced it.
    Assistant {
        id: EntryId,
        at: u64,
        blocks: Vec<AssistantContent>,
    },
    Compaction {
        id: EntryId,
        at: u64,
        record: Compaction,
    },
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::User { id, .. }
            | Entry::Assistant { id, .. }
            | Entry::Compaction { id, .. } => *id,
        }
    }

    pub fn at(&self) -> u64 {
        match self {
            Entry::User { at, .. }
            | Entry::Assistant { at, .. }
            | Entry::Compaction { at, .. } => *at,
        }
    }

    /// An assistant turn's blocks; `None` for every other kind.
    pub fn blocks(&self) -> Option<&[AssistantContent]> {
        match self {
            Entry::Assistant { blocks, .. } => Some(blocks),
            _ => None,
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        let blocks = match self {
            Entry::Assistant { blocks, .. } => blocks.as_slice(),
            _ => &[],
        };
        blocks.iter().filter_map(|b| match b {
            AssistantContent::ToolCall(c) => Some(c),
            _ => None,
        })
    }
}

/// One place the conversation can be rewound to.
///
/// The two are not one operation with a flag. Going back to something the
/// user said means unsending it: the message leaves the transcript and its
/// text returns to the editor, because half the reason to rewind is to ask
/// it differently. Going back to an answer keeps the answer — that is the
/// state the conversation carries on from.
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

/// What an assistant turn said, when it said anything at all.
fn said(blocks: &[AssistantContent]) -> Option<String> {
    blocks.iter().find_map(|b| match b {
        AssistantContent::Text(t) if !t.text.trim().is_empty() => Some(t.text.clone()),
        _ => None,
    })
}

/// The place this entry offers to go back to, or `None` when it is not one.
fn node_of(entry: &Entry) -> Option<Node> {
    match entry {
        Entry::User {
            id,
            body: UserBody::Prompt(t) | UserBody::Aside(t),
            ..
        } => Some(Node::Ask {
            id: *id,
            show: t.shown_text().to_string(),
        }),
        Entry::Assistant { id, blocks, .. } => {
            said(blocks).map(|show| Node::Reply { id: *id, show })
        }
        _ => None,
    }
}

/// One entry as the model currently sees it. Both shapes answer `id`, which is
/// the whole point: compaction needs the content to measure and the id to
/// record, and reading them from two lists is what let them drift apart.
#[derive(Debug, Clone, Copy)]
pub enum Seen<'a> {
    As(&'a Entry),
    /// Content replaced, shell kept — a `tool_use` must keep its `tool_result`.
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

/// The whole conversation: every prompt, tool result, response, and compaction
/// record, in order.
///
/// Held by the caller so a run that ends in an error still leaves behind
/// everything it produced. What the model sees is *derived* from this, never
/// stored in place of it: compaction writes a record and `view` applies it.
/// Anything else loses the session the moment it grows long enough to need
/// shrinking.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    entries: Vec<Entry>,
    #[serde(default)]
    next: u64,
    /// Why the last run ended unanswered. Saved, so a resumed session still
    /// knows; cleared by the next prompt, and by a rewind that cuts the round
    /// it describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interrupted: Option<StopCause>,
}

/// Why the most recent run ended before its prompt was answered, if it did.
/// The transcript alone cannot say whether the stop was the user's or the
/// run's own, so the caller records it here and the next prompt carries it on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum StopCause {
    /// The user asked the run to stop: Esc, `/stop`, an interrupt.
    User,
    /// It died on its own — an error or a crash — and no more is known.
    Other,
}

/// What the model is told after a run the user stopped.
const STOPPED_BY_USER: &str = "The user stopped the previous run before it finished. Treat the \
     request it was working on as cancelled; the message below is what to act on.";
/// What the model is told after a run that died for an unknown reason.
const STOPPED_UNKNOWN: &str = "The previous run ended before it finished, for an unknown \
     reason. Treat the request it was working on as unresolved; the message below is what to act on.";

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

    /// Build a session from a transcript's worth of messages, one entry per
    /// block. The seam exists because a message is what a request looks like
    /// while an entry is what the session stores; nothing in the run needs it,
    /// but a test that wants to say "given this conversation" does.
    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        let mut log = Self::new();
        for m in messages {
            match m {
                Message::User { content } => {
                    for b in content {
                        log.push_user(match b {
                            UserContent::Text(t) => UserBody::Prompt(UserText {
                                text: t.text,
                                shown: None,
                            }),
                            UserContent::ToolResult(r) => UserBody::Result { result: r, preview: None },
                            UserContent::Image(i) => UserBody::Image(i),
                        });
                    }
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

    pub fn push_user(&mut self, body: UserBody) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::User { id, at, body });
        id
    }

    /// Text from the person at the keyboard. `shown` differs only when what
    /// was typed is not what was sent — a `!` command and its output.
    pub fn prompt(&mut self, text: impl Into<String>) -> EntryId {
        self.push_user(UserBody::Prompt(UserText {
            text: text.into(),
            shown: None,
        }))
    }

    /// One entry per result: compaction decides about them one at a time.
    /// Each result becomes its own entry; joining them into one wire message is
    /// the encoder's business.
    pub fn push_results(&mut self, results: Vec<ToolResult>) -> Vec<EntryId> {
        self.push_previewed(results.into_iter().map(|r| (r, None)).collect())
    }

    /// The same, carrying what the screen showed for each — the rows an edit
    /// sketched, which its stored content does not hold.
    pub fn push_previewed(&mut self, results: Vec<(ToolResult, Option<String>)>) -> Vec<EntryId> {
        results
            .into_iter()
            .map(|(result, preview)| self.push_user(UserBody::Result { result, preview }))
            .collect()
    }

    pub fn push_assistant(&mut self, blocks: Vec<AssistantContent>) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Assistant { id, at, blocks });
        id
    }

    pub fn record(&mut self, record: Compaction) -> EntryId {
        let (id, at) = self.mint();
        self.entries.push(Entry::Compaction { id, at, record });
        id
    }

    /// Fold how the run that just ended went into the next prompt: a run that
    /// did not answer its prompt records why, for [`Session::send_prompt`] to
    /// name; one that answered records nothing.
    pub fn note_outcome(&mut self, outcome: &Result<Totals, AgentError>) {
        match outcome {
            Ok(_) => {}
            Err(AgentError::Cancelled) => self.interrupted = Some(StopCause::User),
            Err(_) => self.interrupted = Some(StopCause::Other),
        }
    }

    /// Feed a cause straight in, for tests shaping a session by hand.
    #[cfg(test)]
    fn mark_stopped(&mut self, cause: StopCause) {
        self.interrupted = Some(cause);
    }

    /// Continue with a new prompt, repairing a turn that may have died
    /// mid-call. An assistant turn whose tool calls were never answered would
    /// make the next request invalid (a `tool_use` with no `tool_result`);
    /// each is closed with a result naming the interruption instead — the model
    /// is told a person stopped it rather than that the call failed, which are
    /// different things to answer. A run that ended unanswered is named too,
    /// but only when the caller says why: the transcript cannot tell a user
    /// stop from a crash, and the note must not guess. The prompt is appended
    /// as its own entry.
    /// `shown` is what the user typed, when that differs from what the model
    /// is sent — a `!cmd` line becomes the command *and its output*, and the
    /// screen has to show the line, not the transcript of running it.
    pub fn send_prompt(&mut self, prompt: impl Into<String>, shown: Option<String>) {
        let answered: HashSet<&str> = self
            .entries
            .iter()
            .rev()
            .take_while(|e| !matches!(e, Entry::Assistant { .. }))
            .filter_map(|e| match e {
                Entry::User {
                    body: UserBody::Result { result: r, .. },
                    ..
                } => Some(r.call.as_str()),
                _ => None,
            })
            .collect();
        let unanswered: Vec<ToolCall> = self
            .entries
            .iter()
            .rev()
            .find(|e| matches!(e, Entry::Assistant { .. }))
            .into_iter()
            .flat_map(Entry::tool_calls)
            .filter(|c| !answered.contains(c.id.as_str()))
            .cloned()
            .collect();
        for c in unanswered {
            self.push_user(UserBody::Result {
                result: ToolResult::error(
                    c.id,
                    c.name,
                    "The user stopped this call before it returned; nothing about the call itself failed.",
                ),
                preview: None,
            });
        }
        // The run that just ended may have died unanswered; the caller said
        // why, and the model is told rather than left to read the shape.
        if let Some(cause) = self.interrupted.take() {
            let text = match cause {
                StopCause::User => STOPPED_BY_USER,
                StopCause::Other => STOPPED_UNKNOWN,
            };
            self.push_user(UserBody::Note(UserText {
                text: text.to_string(),
                shown: None,
            }));
        }
        self.push_user(UserBody::Prompt(UserText {
            text: prompt.into(),
            shown,
        }));
    }

    /// Everywhere the conversation can be rewound to, in session order.
    ///
    /// Asides included, and deliberately: rewinding to before a `!` command is
    /// a thing someone wants. What *names* the session is a different question
    /// with a different answer, and the resume listing asks that one against
    /// the archive without loading it.
    ///
    /// Assistant turns are here too, but only the ones that said something. A
    /// turn holding nothing but tool calls is a step of the work rather than a
    /// place in the conversation, and a menu listing every one of them is a
    /// menu nobody can find anything in.
    ///
    /// Reads `history`, not `view`: a prompt compaction dropped is still one
    /// the user asked, so the rewind menu can reach it — and rewinding past a
    /// compaction entry truncates that too, which undoes the compaction and
    /// brings the content back. It also keeps the session's name from changing
    /// the first time its opening turn is compacted away.
    pub fn rewind_nodes(&self) -> Vec<Node> {
        self.entries.iter().filter_map(node_of).collect()
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
            .and_then(|e| match e {
                Entry::User {
                    body: UserBody::Prompt(t) | UserBody::Aside(t),
                    ..
                } => Some(t.shown_text().to_string()),
                _ => None,
            })
    }

    /// The last question the user asked, when there is one to take back.
    ///
    /// Prompts only: a `!` command's output is not something anyone sent, and
    /// unsending it would put a line back in the editor that was never typed
    /// as a question.
    pub fn last_ask(&self) -> Option<EntryId> {
        self.entries.iter().rev().find_map(|e| match e {
            Entry::User {
                id,
                body: UserBody::Prompt(_),
                ..
            } => Some(*id),
            _ => None,
        })
    }

    /// Rewind to an entry, keeping it: everything after it is removed from the
    /// session, and returns how many entries that was.
    ///
    /// Removed, not compacted: a compaction only records what the model stopped
    /// seeing, while this deletes. A `Compaction` entry caught in the cut takes
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

    pub fn is_empty(&self) -> bool {
        !self
            .entries
            .iter()
            .any(|e| matches!(e, Entry::User { .. } | Entry::Assistant { .. }))
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

    /// Later passes win: an entry omitted as superseded and later aged out
    /// shows the newer notice. Whole-entry omissions only — the ones naming a
    /// block are `block_omissions`, and mixing them would let a block notice
    /// hide an entry that is still shown in full.
    fn omissions(&self) -> HashMap<EntryId, &str> {
        let mut out = HashMap::new();
        for e in &self.entries {
            if let Entry::Compaction { record, .. } = e {
                for el in record.omissions.iter().filter(|o| o.block.is_none()) {
                    out.insert(el.entry, el.notice.as_str());
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

    /// What the model can see right now, in session order.
    ///
    /// Compaction records and task lists are inputs to this, not content, so
    /// they are skipped; dropped entries are gone; omitted ones keep their
    /// shell. Nothing is merged — that is the wire's business, and doing it
    /// here is what once made two lists of different lengths share an index.
    /// What a person can see: everything, in order, compaction or no.
    ///
    /// The other half of `view`, and the split is the whole point —
    /// **compaction is the model losing sight of history, not the user**.
    /// Not destroying the transcript is only worth something if it can still be
    /// read afterwards, and the one surface that reads it to a human was
    /// reading the model's copy: the conversation sat complete on disk and the
    /// screen showed it truncated.
    ///
    /// Readers divide cleanly. `view`: estimate, encode, compact — anything
    /// asking what the model is sent. `history`: the screen, the rewind menu,
    /// the session's own name.
    pub fn history(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Every entry the model has stopped seeing — dropped by a compaction, or
    /// shown to it as a notice. What the screen marks rather than hides.
    ///
    /// The whole set, not one lookup: the only caller walks the transcript, and
    /// answering per entry meant rebuilding both maps from every entry each
    /// time. That is quadratic, and a rebuild is exactly when the transcript is
    /// longest.
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
            .filter(|e| matches!(e, Entry::User { .. } | Entry::Assistant { .. }))
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

    /// The view as messages: one entry, one message, in view order.
    ///
    /// Deliberately unmerged. Anthropic wants a turn's user blocks in one
    /// `role:"user"` message and Responses wants them apart, so joining them is
    /// a format rule and lives in the encoder. Doing it here would write one
    /// wire's requirement into the projection both of them read.
    pub fn context(&self) -> Vec<Message> {
        let summaries = self.summaries();
        let gone = self.block_omissions();
        let mut out: Vec<Message> = Vec::new();
        let mut first_user = true;

        for seen in self.view() {
            let block = match seen {
                Seen::As(Entry::Assistant { id, blocks, .. }) => {
                    out.push(Message::Assistant {
                        content: Self::shown_blocks(blocks, *id, &gone),
                    });
                    continue;
                }
                // An assistant turn is never omitted: its `tool_use` blocks
                // must stay to keep the answering results legal.
                Seen::Omitted {
                    entry: Entry::Assistant { id, blocks, .. },
                    ..
                } => {
                    out.push(Message::Assistant {
                        content: Self::shown_blocks(blocks, *id, &gone),
                    });
                    continue;
                }
                Seen::As(Entry::User { body, .. }) => user_block(body),
                Seen::Omitted {
                    entry: Entry::User { body, .. },
                    notice,
                } => omitted_block(body, notice),
                _ => continue,
            };

            let mut content = vec![block];
            if first_user {
                first_user = false;
                for s in &summaries {
                    content.push(UserContent::Text(Text {
                        text: format!("<earlier-work>\n{s}\n</earlier-work>"),
                    }));
                }
            }
            out.push(Message::User { content });
        }
        out
    }
}

pub fn user_block(body: &UserBody) -> UserContent {
    match body {
        UserBody::Prompt(t) | UserBody::Aside(t) | UserBody::Note(t) => UserContent::Text(Text {
            text: t.text.clone(),
        }),
        UserBody::Result { result: r, .. } => UserContent::ToolResult(r.clone()),
        UserBody::Image(i) => UserContent::Image(i.clone()),
    }
}

/// The shell an omitted entry keeps. A result must stay a result, or the
/// `tool_use` it answers is left dangling.
pub fn omitted_block(body: &UserBody, notice: &str) -> UserContent {
    match body {
        UserBody::Result { result: r, .. } => {
            let mut out = r.clone();
            out.content = vec![ToolResultContent::Text(Text {
                text: notice.to_string(),
            })];
            out.useless = false;
            UserContent::ToolResult(out)
        }
        _ => UserContent::Text(Text {
            text: notice.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use brain::message::{Text as MsgText, ToolCall, ToolResult};

    /// The contract the Anthropic encoder's join is written against: what a
    /// turn holds arrives as separate messages, and joining them is the wire's
    /// business, not this projection's.
    #[test]
    fn the_view_hands_over_one_message_per_entry() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "on it".into() })]);
        s.push_results(vec![
            ToolResult::text("c1", "read", "a"),
            ToolResult::text("c2", "grep", "b"),
        ]);
        s.push_user(UserBody::Image(Image::Url { url: "http://x/i.png".into() }));
        s.prompt("and now this");

        let msgs = s.context();
        assert_eq!(msgs.len(), s.view().len());
        for m in &msgs {
            if let Message::User { content } = m {
                assert_eq!(content.len(), 1, "a user message carried more than its entry");
            }
        }
    }

    /// Compaction is the model losing sight of history, not the user. What
    /// names a session is read out of the archive, so it has to stay in the
    /// archive — a compaction that removed it would rename the session the
    /// first time the window filled.
    #[test]
    fn compacting_the_opening_turn_leaves_it_in_the_transcript() {
        let mut s = Session::new();
        let first = s.prompt("why is the flaky test flaky?");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "looking".into() })]);
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

    /// And because the menu can still name it, rewinding to it truncates the
    /// compaction entry too — which puts the dropped turns back.
    #[test]
    fn rewinding_past_a_compaction_undoes_it() {
        let mut s = Session::new();
        let first = s.prompt("the task");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "on it".into() })]);
        let second = s.prompt("more");
        s.record(Compaction {
            dropped: vec![first],
            ..Default::default()
        });

        assert!(s.rewind_nodes().iter().any(|n| n.id() == first), "the menu must reach it");
        s.rollback_to(second);

        assert!(!s.out_of_view().contains(&first), "the compaction went with the rewind");
        assert!(s.view().iter().any(|seen| seen.id() == first));
    }

    /// Unsending is the other half of the rewind: the message itself has to
    /// leave, or the editor holds a line the model is still being sent.
    #[test]
    fn unsending_a_message_takes_it_out_of_the_transcript() {
        let mut s = Session::new();
        s.prompt("the first thing");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "done".into() })]);
        let second = s.prompt("teh typo one");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "answering".into() })]);

        assert_eq!(s.unsent_text(second).as_deref(), Some("teh typo one"));
        assert_eq!(s.last_ask(), Some(second));
        assert_eq!(s.rollback_before(second), 2, "the message and the answer to it");
        assert!(!s.history().any(|e| e.id() == second));
        assert_eq!(s.rewind_nodes().len(), 2, "the first turn, both halves");
    }

    /// A turn that only called tools is a step of the work; the menu offering
    /// one row per `read` is a menu nothing can be found in.
    #[test]
    fn only_answers_reach_the_rewind_menu() {
        let mut s = Session::new();
        let ask = s.prompt("read it");
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: serde_json::json!({ "path": "f.rs" }),
        })]);
        s.push_results(vec![ToolResult::text("c1", "read", "a")]);
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "it says a".into() })]);

        let nodes = s.rewind_nodes();
        assert_eq!(nodes.len(), 2, "the question and the answer, not the call");
        assert_eq!(nodes[0].id(), ask);
        assert!(matches!(nodes[1], Node::Reply { .. }));
        assert_eq!(nodes[1].show(), "it says a");
    }

    /// The caller records why the last run died; the next prompt carries the
    /// cause on to the model instead of leaving it to read the shape.
    #[test]
    fn a_user_stop_is_named_before_the_next_prompt() {
        let mut s = Session::new();
        s.prompt("version up");
        s.mark_stopped(StopCause::User);

        s.send_prompt("delete the branch", None);

        let entries = s.entries();
        assert_eq!(entries.len(), 3, "prompt, the note, the new prompt");
        assert_eq!(
            match &entries[1] {
                Entry::User { body: UserBody::Note(t), .. } => t.text.as_str(),
                other => panic!("expected the stop note, got {other:?}"),
            },
            STOPPED_BY_USER
        );
        assert!(matches!(&entries[2], Entry::User { body: UserBody::Prompt(_), .. }));

        // One note per dead run: the send that followed consumed the marker.
        s.send_prompt("and now this", None);
        assert_eq!(s.entries().len(), 4, "no second note");
    }

    /// The note is the session's words, not the user's: it stays out of the
    /// rewind menu, nothing an unsend hands to the editor, and the model
    /// still reads it beside the next prompt.
    #[test]
    fn a_stop_note_is_model_only_and_not_rewindable() {
        let mut s = Session::new();
        s.prompt("version up");
        s.mark_stopped(StopCause::User);
        s.send_prompt("delete the branch", None);

        let entries = s.entries();
        let note = entries[1].id();
        assert_eq!(s.unsent_text(note), None, "not the user's words to take back");
        assert_eq!(
            s.rewind_nodes().len(),
            2,
            "the two asks only — the note is not a place to rewind to"
        );
        let reaches_the_model = s.context().iter().any(|m| {
            matches!(
                m,
                Message::User { content } if content.iter().any(|b| {
                    matches!(b, UserContent::Text(t) if t.text.contains(STOPPED_BY_USER))
                })
            )
        });
        assert!(reaches_the_model, "the note is sent with the new prompt");

        // The note travels with the archive, and comes back whole.
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    /// A run that died on its own is named as such, not blamed on the user.
    #[test]
    fn an_unknown_stop_is_not_blamed_on_the_user() {
        let mut s = Session::new();
        s.prompt("version up");
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "grep" }),
        })]);
        s.push_results(vec![ToolResult::text("c1", "bash", "1.1.1")]);
        s.mark_stopped(StopCause::Other);

        s.send_prompt("delete the branch", None);

        let entries = s.entries();
        assert_eq!(entries.len(), 5, "prompt, tool work, the note, the new prompt");
        assert_eq!(
            match &entries[3] {
                Entry::User { body: UserBody::Note(t), .. } => t.text.as_str(),
                other => panic!("expected the stop note, got {other:?}"),
            },
            STOPPED_UNKNOWN
        );
        assert!(matches!(&entries[4], Entry::User { body: UserBody::Prompt(_), .. }));
    }

    /// An answered run leaves no marker, and a clean send stays clean.
    #[test]
    fn a_run_that_finished_adds_no_note() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "read".into(),
            args: serde_json::json!({}),
        })]);
        s.push_results(vec![ToolResult::text("c1", "read", "a")]);
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "it says a".into() })]);

        s.send_prompt("and now this", None);

        let entries = s.entries();
        assert_eq!(entries.len(), 5, "no note: prompt, call, result, reply, prompt");
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e, Entry::User { body: UserBody::Note(_), .. })),
            "an answered round adds no note"
        );
    }

    /// A rewind cuts the round the marker described; the marker goes with it.
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
        assert!(matches!(&entries[0], Entry::User { body: UserBody::Prompt(_), .. }));
    }

    /// The mapping the callers rely on: an answer records nothing, a stop the
    /// user asked for records itself.
    #[test]
    fn note_outcome_tells_an_answer_from_a_user_stop() {
        let mut answered = Session::new();
        answered.prompt("go");
        answered.note_outcome(&Ok(crate::Totals::default()));
        answered.send_prompt("and now this", None);
        assert_eq!(answered.entries().len(), 2, "no note after an answer");

        let mut stopped = Session::new();
        stopped.prompt("go");
        stopped.note_outcome(&Err(crate::AgentError::Cancelled));
        stopped.send_prompt("and now this", None);
        let entries = stopped.entries();
        assert_eq!(entries.len(), 3, "prompt, the note, the new prompt");
        assert_eq!(
            match &entries[1] {
                Entry::User { body: UserBody::Note(t), .. } => t.text.as_str(),
                other => panic!("expected the note aside, got {other:?}"),
            },
            STOPPED_BY_USER
        );
    }

    /// Rewinding to an answer is the opposite call: the answer stays, and the
    /// conversation continues from it.
    #[test]
    fn rewinding_to_an_answer_keeps_it() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "here".into() })]);
        let reply = s.rewind_nodes().pop().map(|n| n.id()).expect("an answer");
        s.prompt("and then");

        assert_eq!(s.rollback_to(reply), 1);
        assert!(s.history().any(|e| e.id() == reply), "the answer stays");
        assert_eq!(s.unsent_text(reply), None, "nothing goes back to the editor");
    }

    /// The one user message that legitimately carries more than one block.
    #[test]
    fn summaries_ride_the_first_user_message_rather_than_one_of_their_own() {
        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Text(MsgText { text: "done".into() })]);
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
}
