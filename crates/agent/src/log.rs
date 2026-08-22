use std::collections::{HashMap, HashSet};

use brain::message::{Message, Text, ToolCallId, ToolResultContent, UserContent};
use serde::{Deserialize, Serialize};
use tools::todo::Todo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EntryId(pub u64);

/// One tool result the model no longer sees in full.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Elision {
    pub call: ToolCallId,
    pub notice: String,
}

/// A record of one compaction pass. It says what the model stopped seeing; it
/// does not remove anything, so the log keeps the whole session.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Compaction {
    /// Entries the view skips entirely.
    pub dropped: Vec<EntryId>,
    /// Tool results the view shows as a notice instead of their content.
    pub elisions: Vec<Elision>,
    /// Stands in for the dropped entries. Absent when nothing was dropped, or
    /// when summarizing failed — the entries still go, unsummarized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub tokens_before: usize,
    pub tokens_after: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    Message {
        id: EntryId,
        message: Message,
    },
    Compaction {
        id: EntryId,
        record: Compaction,
    },
    /// The task list as it stood. State is derived by taking the last one, so
    /// it survives compaction and comes back with a resumed session.
    Todos {
        id: EntryId,
        items: Vec<Todo>,
    },
    /// Text the view appends to an earlier user turn. Resuming a session that
    /// died mid-turn cannot start a second user message in a row — both wires
    /// require the roles to alternate — and rewriting the stored message would
    /// give up the one property this log exists for.
    Amend {
        id: EntryId,
        target: EntryId,
        text: String,
    },
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Message { id, .. }
            | Entry::Compaction { id, .. }
            | Entry::Amend { id, .. }
            | Entry::Todos { id, .. } => *id,
        }
    }
}

/// Append-only session history.
///
/// What the model sees is *derived* from this, never stored in place of it:
/// compaction writes a record and the view applies it. Anything else loses the
/// session the moment it grows long enough to need shrinking.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Log {
    entries: Vec<Entry>,
    next: u64,
}

impl Log {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        let mut log = Self::new();
        for m in messages {
            log.push(m);
        }
        log
    }

    pub fn push(&mut self, message: Message) -> EntryId {
        let id = EntryId(self.next);
        self.next += 1;
        self.entries.push(Entry::Message { id, message });
        id
    }

    pub fn record(&mut self, record: Compaction) -> EntryId {
        let id = EntryId(self.next);
        self.next += 1;
        self.entries.push(Entry::Compaction { id, record });
        id
    }

    /// Record the task list. At most one item may be in progress: two are a
    /// plan the agent is not actually following.
    pub fn set_todos(&mut self, mut items: Vec<Todo>) -> EntryId {
        tools::todo::normalize(&mut items);
        let id = EntryId(self.next);
        self.next += 1;
        self.entries.push(Entry::Todos { id, items });
        id
    }

    /// The task list as it now stands.
    pub fn todos(&self) -> &[Todo] {
        self.entries
            .iter()
            .rev()
            .find_map(|e| match e {
                Entry::Todos { items, .. } => Some(items.as_slice()),
                _ => None,
            })
            .unwrap_or(&[])
    }

    pub fn amend(&mut self, target: EntryId, text: impl Into<String>) -> EntryId {
        let id = EntryId(self.next);
        self.next += 1;
        self.entries.push(Entry::Amend {
            id,
            target,
            text: text.into(),
        });
        id
    }

    /// Graft a new prompt onto a session that may have died mid-turn.
    ///
    /// An assistant turn whose calls were never answered has to leave the view:
    /// an unanswered `tool_use` makes the next request invalid. What remains
    /// then usually ends on tool results, and the prompt joins that turn rather
    /// than starting one the wire would reject.
    pub fn resume(&mut self, prompt: impl Into<String>) {
        let unanswered: Vec<EntryId> = self
            .live()
            .iter()
            .rev()
            .take_while(|(_, m)| {
                matches!(m, Message::Assistant { .. }) && m.tool_calls().next().is_some()
            })
            .map(|(id, _)| *id)
            .collect();
        if !unanswered.is_empty() {
            self.record(Compaction {
                dropped: unanswered,
                ..Default::default()
            });
        }

        match self.live().last() {
            Some((id, Message::User { .. })) => {
                let id = *id;
                self.amend(id, prompt);
            }
            _ => {
                self.push(Message::user(prompt.into()));
            }
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        !self
            .entries
            .iter()
            .any(|e| matches!(e, Entry::Message { .. }))
    }

    /// Every message entry, whether or not the view still shows it.
    pub fn messages(&self) -> impl Iterator<Item = (EntryId, &Message)> {
        self.entries.iter().filter_map(|e| match e {
            Entry::Message { id, message } => Some((*id, message)),
            _ => None,
        })
    }

    /// Message entries the view still shows, before elisions are applied.
    pub fn live(&self) -> Vec<(EntryId, &Message)> {
        let dropped = self.dropped();
        self.messages()
            .filter(|(id, _)| !dropped.contains(id))
            .collect()
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

    /// Later passes win: a result elided as superseded and later aged out shows
    /// the newer notice.
    fn elisions(&self) -> HashMap<&ToolCallId, &str> {
        let mut out = HashMap::new();
        for e in &self.entries {
            if let Entry::Compaction { record, .. } = e {
                for el in &record.elisions {
                    out.insert(&el.call, el.notice.as_str());
                }
            }
        }
        out
    }

    /// Summaries still in force, oldest first. A compaction entry can itself be
    /// dropped — that is how a fresh summary replaces the one it folded in,
    /// instead of the view accumulating one section per pass.
    pub fn summaries(&self) -> Vec<&str> {
        let dropped = self.dropped();
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Compaction { id, record } if !dropped.contains(id) => {
                    record.summary.as_deref()
                }
                _ => None,
            })
            .collect()
    }

    /// The messages behind a set of ids, in log order.
    pub fn messages_for(&self, ids: &[EntryId]) -> Vec<&Message> {
        self.messages()
            .filter(|(id, _)| ids.contains(id))
            .map(|(_, m)| m)
            .collect()
    }

    /// Ids of compaction entries carrying a summary, for a later pass to retire.
    pub fn summary_entries(&self) -> Vec<EntryId> {
        let dropped = self.dropped();
        self.entries
            .iter()
            .filter_map(|e| match e {
                Entry::Compaction { id, record }
                    if record.summary.is_some() && !dropped.contains(id) =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect()
    }

    fn amendments(&self) -> HashMap<EntryId, Vec<&str>> {
        let mut out: HashMap<EntryId, Vec<&str>> = HashMap::new();
        for e in &self.entries {
            if let Entry::Amend { target, text, .. } = e {
                out.entry(*target).or_default().push(text);
            }
        }
        out
    }

    /// What goes on the wire.
    ///
    /// A summary rides on the opening turn rather than as a message of its own:
    /// dropping whole exchanges leaves the history starting on an assistant
    /// turn, and both wires require the roles to alternate.
    pub fn context(&self) -> Vec<Message> {
        let elisions = self.elisions();
        let amendments = self.amendments();
        let summaries = self.summaries();
        let mut first_user = true;

        self.live()
            .into_iter()
            .map(|(id, m)| {
                let Message::User { content } = m else {
                    return m.clone();
                };
                let mut content: Vec<UserContent> =
                    content.iter().map(|b| apply(b, &elisions)).collect();
                if first_user {
                    first_user = false;
                    for s in &summaries {
                        content.push(UserContent::Text(Text {
                            text: format!("<earlier-work>\n{s}\n</earlier-work>"),
                        }));
                    }
                }
                for text in amendments.get(&id).into_iter().flatten() {
                    content.push(UserContent::Text(Text {
                        text: (*text).to_string(),
                    }));
                }
                Message::User { content }
            })
            .collect()
    }
}

fn apply(block: &UserContent, elisions: &HashMap<&ToolCallId, &str>) -> UserContent {
    let UserContent::ToolResult(r) = block else {
        return block.clone();
    };
    let Some(notice) = elisions.get(&r.call) else {
        return block.clone();
    };
    let mut out = r.clone();
    out.content = vec![ToolResultContent::Text(Text {
        text: (*notice).to_string(),
    })];
    out.useless = false;
    UserContent::ToolResult(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain::message::{AssistantContent, ToolCall, ToolResult};
    use serde_json::json;

    fn call(id: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: ToolCallId(id.into()),
                provider: None,
                name: "read".into(),
                args: json!({ "path": "a.rs" }),
            })],
        }
    }

    fn result(id: &str, body: &str) -> Message {
        Message::tool_results(vec![ToolResult::text(ToolCallId(id.into()), "read", body)])
    }

    fn log() -> Log {
        Log::from_messages([
            Message::user("go"),
            call("c1"),
            result("c1", "first body"),
            call("c2"),
            result("c2", "second body"),
        ])
    }

    #[test]
    fn a_fresh_log_shows_every_message() {
        assert_eq!(log().context().len(), 5);
    }

    #[test]
    fn an_elision_changes_the_view_and_leaves_the_log_whole() {
        let mut l = log();
        l.record(Compaction {
            elisions: vec![Elision {
                call: ToolCallId("c1".into()),
                notice: "[gone]".into(),
            }],
            ..Default::default()
        });

        let view = l.context();
        assert_eq!(view.len(), 5, "elision replaces content, never a message");
        assert!(format!("{:?}", view[2]).contains("[gone]"));
        // The record is what changed; the message entry still holds its body.
        assert!(
            l.messages()
                .any(|(_, m)| format!("{m:?}").contains("first body"))
        );
    }

    #[test]
    fn a_dropped_entry_leaves_the_view_but_stays_in_the_log() {
        let mut l = log();
        let ids: Vec<_> = l.messages().map(|(id, _)| id).collect();
        l.record(Compaction {
            dropped: vec![ids[1], ids[2]],
            ..Default::default()
        });

        assert_eq!(l.context().len(), 3);
        assert_eq!(l.messages().count(), 5);
        assert_eq!(l.context()[0].text(), "go");
    }

    #[test]
    fn a_later_pass_overrides_an_earlier_notice() {
        let mut l = log();
        for notice in ["[first notice]", "[second notice]"] {
            l.record(Compaction {
                elisions: vec![Elision {
                    call: ToolCallId("c1".into()),
                    notice: notice.into(),
                }],
                ..Default::default()
            });
        }
        assert!(format!("{:?}", l.context()[2]).contains("[second notice]"));
    }

    #[test]
    fn the_log_round_trips_through_json_with_its_records() {
        let mut l = log();
        l.record(Compaction {
            dropped: vec![EntryId(1)],
            elisions: vec![Elision {
                call: ToolCallId("c2".into()),
                notice: "[x]".into(),
            }],
            summary: Some("did some work".into()),
            tokens_before: 900,
            tokens_after: 100,
        });
        let back: Log = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        assert_eq!(back.context().len(), l.context().len());
    }

    #[test]
    fn ids_keep_climbing_so_a_reloaded_log_cannot_collide() {
        let mut l = log();
        l.record(Compaction::default());
        let last = l.push(Message::user("more"));
        let back: Log = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        let mut back = back;
        assert!(back.push(Message::user("later")) > last);
    }
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use brain::message::{AssistantContent, ToolCall, ToolResult};
    use serde_json::json;

    fn call(id: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: ToolCallId(id.into()),
                provider: None,
                name: "read".into(),
                args: json!({}),
            })],
        }
    }

    fn results(id: &str) -> Message {
        Message::tool_results(vec![ToolResult::text(
            ToolCallId(id.into()),
            "read",
            "body",
        )])
    }

    fn roles(log: &Log) -> Vec<&'static str> {
        log.context()
            .iter()
            .map(|m| match m {
                Message::System { .. } => "system",
                Message::User { .. } => "user",
                Message::Assistant { .. } => "assistant",
            })
            .collect()
    }

    #[test]
    fn a_prompt_after_tool_results_joins_that_turn() {
        let mut l = Log::from_messages([Message::user("hi"), call("c1"), results("c1")]);
        l.resume("and now?");

        // Two user turns in a row are rejected outright by both wires.
        assert_eq!(roles(&l), vec!["user", "assistant", "user"]);
        assert_eq!(l.context()[2].text(), "and now?");
        // The graft is a record, not a rewrite: the stored message is untouched.
        assert_eq!(l.messages().count(), 3);
        assert!(!format!("{:?}", l.messages().nth(2).unwrap().1).contains("and now?"));
    }

    #[test]
    fn an_unanswered_call_leaves_the_view_before_the_prompt_lands() {
        let mut l = Log::from_messages([
            Message::user("hi"),
            Message::assistant_text("ok"),
            call("c9"),
        ]);
        l.resume("next");

        assert_eq!(roles(&l), vec!["user", "assistant", "user"]);
        assert_eq!(l.context()[2].text(), "next");
        // Three original messages plus the new prompt: the unanswered call left
        // the view without leaving the log.
        assert_eq!(l.messages().count(), 4);
        assert!(
            l.messages()
                .any(|(_, m)| m.tool_calls().any(|c| c.id.0 == "c9"))
        );
        assert!(!l.context().iter().any(|m| m.tool_calls().next().is_some()));
    }

    #[test]
    fn a_clean_transcript_gains_one_turn() {
        let mut l = Log::from_messages([Message::user("hi"), Message::assistant_text("done")]);
        l.resume("more");
        assert_eq!(roles(&l), vec!["user", "assistant", "user"]);
    }

    #[test]
    fn an_empty_log_starts_the_conversation() {
        let mut l = Log::new();
        l.resume("first");
        assert_eq!(l.context().len(), 1);
        assert_eq!(l.context()[0].text(), "first");
    }
}
