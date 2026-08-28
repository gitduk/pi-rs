use std::collections::{HashMap, HashSet};

use brain::message::{
    Message, Text, ToolCall, ToolCallId, ToolResult, ToolResultContent, UserContent,
};
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
/// does not remove anything, so the session keeps everything.
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
}

impl Entry {
    pub fn id(&self) -> EntryId {
        match self {
            Entry::Message { id, .. } | Entry::Compaction { id, .. } | Entry::Todos { id, .. } => {
                *id
            }
        }
    }
}

/// The whole conversation: every message, tool result, and compaction record,
/// in order.
///
/// Held by the caller so a run that ends in an error still leaves behind
/// everything it produced. What the model sees is *derived* from this, never
/// stored in place of it: compaction writes a record and the view applies it.
/// Anything else loses the session the moment it grows long enough to need
/// shrinking.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    entries: Vec<Entry>,
    #[serde(default)]
    next: u64,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    /// A session that starts from a single user prompt.
    pub fn with_prompt(prompt: impl Into<String>) -> Self {
        let mut session = Self::new();
        session.push(Message::user(prompt));
        session
    }

    pub fn from_messages(messages: impl IntoIterator<Item = Message>) -> Self {
        let mut session = Self::new();
        for m in messages {
            session.push(m);
        }
        session
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

    /// Append a user turn as its own message. A session may end on tool
    /// results or another user message; the view merges adjacent user
    /// messages when it builds what goes on the wire.
    pub fn append_user(&mut self, text: impl Into<String>) -> EntryId {
        self.push(Message::user(text))
    }

    /// Continue with a new prompt, repairing a turn that may have died
    /// mid-call. An assistant turn whose tool calls were never answered would
    /// make the next request invalid (a `tool_use` with no `tool_result`);
    /// each is closed with an interrupted result instead, so the model knows
    /// it was stopped, and the prompt is appended as its own user message.
    pub fn send_prompt(&mut self, prompt: impl Into<String>) {
        let unanswered: Vec<ToolCall> = self
            .live()
            .iter()
            .rev()
            .take_while(|(_, m)| matches!(m, Message::Assistant { .. }))
            .flat_map(|(_, m)| m.tool_calls())
            .cloned()
            .collect();
        if !unanswered.is_empty() {
            let results = unanswered
                .into_iter()
                .map(|c| {
                    ToolResult::error(
                        c.id,
                        c.name,
                        "The tool call was interrupted before its result returned.",
                    )
                })
                .collect();
            self.push(Message::tool_results(results));
        }
        self.push(Message::user(prompt));
    }

    /// Live user messages that carry a prompt, in session order: each user
    /// message's own text blocks.
    pub fn prompts(&self) -> Vec<(EntryId, String)> {
        self.live()
            .into_iter()
            .filter_map(|(id, m)| prompt_text(m).map(|text| (id, text)))
            .collect()
    }

    /// The first thing the user asked, if anything. The text `prompts` would
    /// report first, without building the whole list for the one value.
    pub fn first_prompt(&self) -> Option<String> {
        self.live().into_iter().find_map(|(_, m)| prompt_text(m))
    }

    /// Rewind to a user message: everything after it leaves the session, and
    /// returns how many entries that was.
    pub fn rollback_to(&mut self, user: EntryId) -> usize {
        let Some(keep) = self
            .entries
            .iter()
            .position(|e| e.id() == user)
            .map(|i| i + 1)
        else {
            return 0;
        };
        let dropped = self.entries.len() - keep;
        self.entries.truncate(keep);
        dropped
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

    /// The messages behind a set of ids, in session order.
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

    /// What goes on the wire.
    ///
    /// A summary rides on the opening turn rather than as a message of its own:
    /// dropping whole exchanges leaves the history starting on an assistant
    /// turn. Adjacent user messages — a tool result followed by the prompt
    /// that closed it — are merged, since both wires require the roles to
    /// alternate.
    pub fn context(&self) -> Vec<Message> {
        let elisions = self.elisions();
        let summaries = self.summaries();
        let mut out: Vec<Message> = Vec::new();
        let mut first_user = true;

        for (_, m) in self.live() {
            let Message::User { content } = m else {
                out.push(m.clone());
                continue;
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
            if let Some(Message::User { content: prev }) = out.last_mut() {
                prev.extend(content);
            } else {
                out.push(Message::User { content });
            }
        }
        out
    }
}

/// A user message's prompt text: its own text blocks. `None` when the message
/// carries no text of its own.
fn prompt_text(m: &Message) -> Option<String> {
    let Message::User { content } = m else {
        return None;
    };
    let text: String = content
        .iter()
        .filter_map(|c| match c {
            UserContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(text)
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

    fn log() -> Session {
        Session::from_messages([
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
        let back: Session = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert_eq!(back, l);
        assert_eq!(back.context().len(), l.context().len());
    }

    #[test]
    fn ids_keep_climbing_so_a_reloaded_log_cannot_collide() {
        let mut l = log();
        l.record(Compaction::default());
        let last = l.push(Message::user("more"));
        let back: Session = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        let mut back = back;
        assert!(back.push(Message::user("later")) > last);
    }

    #[test]
    fn a_rewind_keeps_the_chosen_message_and_drops_what_follows() {
        let mut l = log();
        l.push(Message::user("second turn"));
        l.push(Message::assistant_text("second answer"));

        assert_eq!(l.rollback_to(EntryId(5)), 1);
        assert_eq!(l.context().len(), 5);
        assert_eq!(l.context()[4].text(), "second turn");
        assert_eq!(l.entries().last().unwrap().id(), EntryId(5));
    }

    #[test]
    fn a_rewind_to_the_first_message_keeps_only_it() {
        let mut l = log();
        assert_eq!(l.rollback_to(EntryId(0)), 4);
        assert_eq!(l.context().len(), 1);
        assert_eq!(l.context()[0].text(), "go");
    }

    #[test]
    fn a_rewind_to_the_last_message_drops_nothing() {
        let mut l = log();
        assert_eq!(l.rollback_to(EntryId(4)), 0);
        assert_eq!(l.context().len(), 5);
    }

    #[test]
    fn a_rewind_to_an_unknown_id_is_a_no_op() {
        let mut l = log();
        assert_eq!(l.rollback_to(EntryId(99)), 0);
        assert_eq!(l.context().len(), 5);
    }

    #[test]
    fn ids_keep_climbing_across_a_rewind() {
        let mut l = log();
        let before = l.push(Message::user("tail"));
        l.rollback_to(before);
        assert!(l.push(Message::user("later")) > before);
    }

    #[test]
    fn prompts_are_the_messages_with_text() {
        let l = log();
        let prompts = l.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, EntryId(0));
        assert_eq!(prompts[0].1, "go");
    }

    #[test]
    fn a_prompt_after_tool_results_is_its_own_message() {
        // A tool round ends on a tool-results user message; the next prompt
        // is its own user message, counted on its own.
        let mut l = log();
        l.send_prompt("continue");
        let prompts = l.prompts();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[1].0, EntryId(5));
        assert_eq!(prompts[1].1, "continue");
    }

    #[test]
    fn two_user_messages_stay_apart_in_prompts() {
        let mut l = Session::new();
        l.send_prompt("one");
        l.send_prompt("two");
        let prompts = l.prompts();
        assert_eq!(prompts[1].1, "two");
    }

    #[test]
    fn first_prompt_is_the_first_question_and_none_when_there_is_none() {
        let l = log();
        assert_eq!(l.first_prompt().as_deref(), Some("go"));

        assert_eq!(Session::new().first_prompt(), None);
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

    fn roles(log: &Session) -> Vec<&'static str> {
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
    fn a_prompt_after_tool_results_merges_on_the_wire() {
        let mut l = Session::from_messages([Message::user("hi"), call("c1"), results("c1")]);
        l.send_prompt("and now?");

        // The prompt is its own message, merged into the adjacent tool
        // results only when building what goes on the wire.
        assert_eq!(roles(&l), vec!["user", "assistant", "user"]);
        assert_eq!(l.context()[2].text(), "and now?");
        assert_eq!(l.messages().count(), 4);
    }

    #[test]
    fn an_unanswered_call_is_closed_with_an_interrupted_result() {
        let mut l = Session::from_messages([
            Message::user("hi"),
            Message::assistant_text("ok"),
            call("c9"),
        ]);
        l.send_prompt("next");

        assert_eq!(l.messages().count(), 5);
        assert!(
            l.messages()
                .any(|(_, m)| m.tool_calls().any(|c| c.id.0 == "c9"))
        );
        let ctx = l.context();
        let closed: Vec<_> = ctx
            .iter()
            .filter_map(|m| match m {
                Message::User { content } => Some(content),
                _ => None,
            })
            .flatten()
            .filter_map(|c| match c {
                UserContent::ToolResult(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].call.0, "c9");
        assert!(closed[0].is_error);
    }

    #[test]
    fn a_clean_transcript_gains_one_turn() {
        let mut l = Session::from_messages([Message::user("hi"), Message::assistant_text("done")]);
        l.send_prompt("more");
        assert_eq!(roles(&l), vec!["user", "assistant", "user"]);
    }

    #[test]
    fn an_empty_log_starts_the_conversation() {
        let mut l = Session::new();
        l.send_prompt("first");
        assert_eq!(l.context().len(), 1);
        assert_eq!(l.context()[0].text(), "first");
    }

    #[test]
    fn adjacent_user_turns_merge_when_building_the_wire() {
        let mut l = Session::new();
        l.append_user("first");
        l.append_user("second");
        l.append_user("third");
        // Each stays its own message in the log; the wire gets them merged.
        assert_eq!(l.messages().count(), 3);
        assert_eq!(roles(&l), vec!["user"]);
        assert_eq!(l.context()[0].text(), "firstsecondthird");
    }
}
