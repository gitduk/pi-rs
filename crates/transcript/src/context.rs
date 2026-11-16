//! Effective model messages that borrow retained payloads from the session.

use std::borrow::Cow;
use std::collections::HashMap;

use llm::estimate;
use llm::message::{self, AssistantContent, Image, Text, ToolResult, ToolResultContent};
use llm::model::ModelSpec;

use crate::{Entry, EntryId, Seen, Session, injected_summary, omitted_args};

/// One effective message; only synthesized blocks are owned.
#[derive(Debug)]
pub enum Message<'a> {
    User {
        content: Vec<UserContent<'a>>,
    },
    Assistant {
        content: Vec<Cow<'a, AssistantContent>>,
    },
}

/// A user-side block without copying text, images or tool results.
#[derive(Debug)]
pub enum UserContent<'a> {
    Text(Cow<'a, str>),
    Image(&'a Image),
    ToolResult(Cow<'a, ToolResult>),
}

impl Message<'_> {
    /// The same conservative budget used by an owned model message.
    pub fn tokens(&self, spec: &ModelSpec) -> usize {
        estimate::MESSAGE_OVERHEAD
            + match self {
                Self::User { content } => content.iter().map(UserContent::tokens).sum::<usize>(),
                Self::Assistant { content } => content
                    .iter()
                    .map(|block| estimate::assistant_block(block, spec))
                    .sum(),
            }
    }

    /// Materialize a message when an outgoing request needs owned data.
    pub fn into_owned(self) -> message::Message {
        match self {
            Self::User { content } => message::Message::User {
                content: content.into_iter().map(UserContent::into_owned).collect(),
            },
            Self::Assistant { content } => message::Message::Assistant {
                content: content.into_iter().map(Cow::into_owned).collect(),
            },
        }
    }

    fn result(&self) -> Option<&ToolResult> {
        match self {
            Self::User { content } => match content.as_slice() {
                [UserContent::ToolResult(result)] => Some(result),
                _ => None,
            },
            Self::Assistant { .. } => None,
        }
    }
}

impl UserContent<'_> {
    /// Budget this block without materializing its payload.
    pub fn tokens(&self) -> usize {
        match self {
            Self::Text(text) => estimate::text(text),
            Self::Image(image) => estimate::image(image),
            Self::ToolResult(result) => estimate::tool_result(result),
        }
    }

    pub(super) fn into_owned(self) -> message::UserContent {
        match self {
            Self::Text(text) => message::UserContent::Text(Text {
                text: text.into_owned(),
            }),
            Self::Image(image) => message::UserContent::Image(image.clone()),
            Self::ToolResult(result) => message::UserContent::ToolResult(result.into_owned()),
        }
    }
}

impl Session {
    /// Project effective messages while borrowing retained content.
    pub fn context_view(&self) -> Vec<Message<'_>> {
        let summaries = self.summaries();
        let gone = self.block_omissions();
        let mut messages = Vec::new();
        let mut first_user = true;
        for seen in self.view() {
            let mut content = match seen {
                Seen::As(Entry::Answer { id, blocks, .. })
                | Seen::Omitted {
                    entry: Entry::Answer { id, blocks, .. },
                    ..
                } => {
                    messages.push(Message::Assistant {
                        content: assistant_blocks(blocks, *id, &gone),
                    });
                    continue;
                }
                Seen::As(entry) => user_blocks(entry),
                Seen::Omitted { entry, notice } => omitted_blocks(entry, notice),
            };
            if first_user {
                first_user = false;
                content.extend(
                    summaries
                        .iter()
                        .map(|summary| UserContent::Text(Cow::Owned(injected_summary(summary)))),
                );
            }
            messages.push(Message::User { content });
        }
        order_results(&mut messages);
        messages
    }
}

pub(super) fn assistant_blocks<'a>(
    blocks: &'a [AssistantContent],
    id: EntryId,
    gone: &HashMap<(EntryId, usize), &str>,
) -> Vec<Cow<'a, AssistantContent>> {
    blocks
        .iter()
        .enumerate()
        .map(|(at, block)| match (block, gone.get(&(id, at))) {
            (AssistantContent::ToolCall(call), Some(notice)) => {
                Cow::Owned(AssistantContent::ToolCall(omitted_args(call, notice)))
            }
            _ => Cow::Borrowed(block),
        })
        .collect()
}

pub(super) fn user_blocks(entry: &Entry) -> Vec<UserContent<'_>> {
    match entry {
        Entry::Ask { ask, .. } => {
            let mut content: Vec<_> = ask
                .relayed
                .iter()
                .map(|note| UserContent::Text(Cow::Borrowed(note)))
                .collect();
            // Image-only prompts do not send an empty text block.
            if !ask.text.is_empty() || ask.images.is_empty() {
                content.push(UserContent::Text(Cow::Borrowed(&ask.text)));
            }
            content.extend(ask.images.iter().map(UserContent::Image));
            content
        }
        Entry::Bash { run, .. } => vec![UserContent::Text(Cow::Borrowed(&run.text))],
        Entry::Note { note, .. } => vec![UserContent::Text(Cow::Borrowed(note))],
        Entry::Tool { result, .. } => vec![UserContent::ToolResult(Cow::Borrowed(result))],
        Entry::Answer { .. }
        | Entry::Compaction { .. }
        | Entry::Plain { .. }
        | Entry::Notice { .. }
        | Entry::Tally { .. } => Vec::new(),
    }
}

pub(super) fn omitted_blocks<'a>(entry: &'a Entry, notice: &'a str) -> Vec<UserContent<'a>> {
    match entry {
        Entry::Tool { result, .. } => vec![UserContent::ToolResult(Cow::Owned(ToolResult {
            call: result.call.clone(),
            name: result.name.clone(),
            content: vec![ToolResultContent::Text(Text {
                text: notice.to_owned(),
            })],
            is_error: result.is_error,
        }))],
        _ => vec![UserContent::Text(Cow::Borrowed(notice))],
    }
}

// Retained results follow completion order; outgoing messages follow call order.
fn order_results(messages: &mut [Message<'_>]) {
    for at in 0..messages.len() {
        let (before, after) = messages.split_at_mut(at + 1);
        let Message::Assistant { content } = &before[at] else {
            continue;
        };
        let count = after
            .iter()
            .take_while(|message| message.result().is_some())
            .count();
        if count < 2 {
            continue;
        }
        let order: HashMap<_, _> = content
            .iter()
            .filter_map(|block| match block.as_ref() {
                AssistantContent::ToolCall(call) => Some(call.id.as_str()),
                AssistantContent::Text(_) | AssistantContent::Reasoning(_) => None,
            })
            .enumerate()
            .map(|(at, id)| (id, at))
            .collect();
        after[..count].sort_by_key(|message| {
            let result = message
                .result()
                .expect("only single-result messages are sorted");
            order
                .get(result.call.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
    }
}
