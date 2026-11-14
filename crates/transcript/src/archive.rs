//! Rebuild the context projection from canonical conversation content and annotations.

use message::Block;
use message::conversation::Content;
use serde::{Deserialize, Serialize};

use crate::{AssistantContent, Compaction, Drawn, Entry, EntryId, Prompt, Text};

/// Entry annotations contain no duplicate conversation bodies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Annotation {
    Ask {
        id: EntryId,
        at: u64,
        shown: Option<String>,
        relayed: Option<String>,
    },
    Bash {
        id: EntryId,
        at: u64,
        shown: Option<String>,
        relayed: Option<String>,
    },
    Answer {
        id: EntryId,
        at: u64,
    },
    Tool {
        id: EntryId,
        at: u64,
        drawn: Drawn,
    },
    Note {
        id: EntryId,
        at: u64,
    },
    Compaction {
        id: EntryId,
        at: u64,
        record: Compaction,
    },
    Plain {
        id: EntryId,
        at: u64,
        text: String,
    },
    Notice {
        id: EntryId,
        at: u64,
        text: String,
    },
    Tally {
        id: EntryId,
        at: u64,
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub metadata: Annotation,
    pub messages: Vec<Content>,
}

fn prompt_body(prompt: &Prompt) -> Vec<Block> {
    std::iter::once(Block::Text(prompt.text.clone()))
        .chain(prompt.images.iter().cloned().map(Block::Image))
        .collect()
}

impl Record {
    pub fn from_entry(entry: &Entry) -> Self {
        let id = entry.id();
        let at = entry.at();
        let (metadata, messages) = match entry {
            Entry::Ask { ask, .. } => (
                Annotation::Ask {
                    id,
                    at,
                    shown: ask.shown.clone(),
                    relayed: ask.relayed.clone(),
                },
                vec![Content::User(prompt_body(ask))],
            ),
            Entry::Bash { run, .. } => (
                Annotation::Bash {
                    id,
                    at,
                    shown: run.shown.clone(),
                    relayed: run.relayed.clone(),
                },
                vec![Content::User(prompt_body(run))],
            ),
            Entry::Answer { blocks, .. } => (
                Annotation::Answer { id, at },
                blocks
                    .iter()
                    .map(|block| match block {
                        AssistantContent::Text(t) => {
                            Content::Assistant(vec![Block::Text(t.text.clone())])
                        }
                        AssistantContent::Reasoning(r) => {
                            Content::Assistant(vec![Block::Reasoning(r.clone())])
                        }
                        AssistantContent::ToolCall(c) => Content::ToolCall(c.clone()),
                    })
                    .collect(),
            ),
            Entry::Tool {
                result,
                preview,
                warned,
                took,
                ..
            } => (
                Annotation::Tool {
                    id,
                    at,
                    drawn: Drawn {
                        preview: preview.clone(),
                        warned: *warned,
                        took: *took,
                    },
                },
                vec![Content::ToolResult(result.clone())],
            ),
            Entry::Note { note, .. } => (
                Annotation::Note { id, at },
                vec![Content::User(vec![Block::Text(note.clone())])],
            ),
            Entry::Compaction { record, .. } => (
                Annotation::Compaction {
                    id,
                    at,
                    record: record.clone(),
                },
                vec![],
            ),
            Entry::Plain { text, .. } => (
                Annotation::Plain {
                    id,
                    at,
                    text: text.clone(),
                },
                vec![],
            ),
            Entry::Notice { text, .. } => (
                Annotation::Notice {
                    id,
                    at,
                    text: text.clone(),
                },
                vec![],
            ),
            Entry::Tally { text, .. } => (
                Annotation::Tally {
                    id,
                    at,
                    text: text.clone(),
                },
                vec![],
            ),
        };
        Self { metadata, messages }
    }

    pub fn into_entry(self) -> Result<Entry, serde_json::Error> {
        let Self { metadata, messages } = self;
        Ok(match metadata {
            Annotation::Ask {
                id,
                at,
                shown,
                relayed,
            } => Entry::Ask {
                id,
                at,
                ask: read_prompt(messages, shown, relayed)?,
            },
            Annotation::Bash {
                id,
                at,
                shown,
                relayed,
            } => Entry::Bash {
                id,
                at,
                run: read_prompt(messages, shown, relayed)?,
            },
            Annotation::Answer { id, at } => {
                let mut blocks = Vec::new();
                for message in messages {
                    match message {
                        Content::Assistant(body) => {
                            for block in body {
                                blocks.push(match block {
                                    Block::Text(text) => AssistantContent::Text(Text { text }),
                                    Block::Reasoning(r) => AssistantContent::Reasoning(r),
                                    Block::Image(_) | Block::Data(_) => {
                                        return Err(invalid("invalid assistant block"));
                                    }
                                });
                            }
                        }
                        Content::ToolCall(call) => blocks.push(AssistantContent::ToolCall(call)),
                        Content::User(_) | Content::ToolResult(_) => {
                            return Err(invalid("invalid model reply"));
                        }
                    }
                }
                Entry::Answer { id, at, blocks }
            }
            Annotation::Tool { id, at, drawn } => {
                let mut messages = messages.into_iter();
                let Some(Content::ToolResult(result)) = messages.next() else {
                    return Err(invalid("missing tool result"));
                };
                if messages.next().is_some() {
                    return Err(invalid("multiple tool results for one entry"));
                }
                Entry::Tool {
                    id,
                    at,
                    result,
                    preview: drawn.preview,
                    warned: drawn.warned,
                    took: drawn.took,
                }
            }
            Annotation::Note { id, at } => {
                let prompt = read_prompt(messages, None, None)?;
                if !prompt.images.is_empty() {
                    return Err(invalid("image in context note"));
                }
                Entry::Note {
                    id,
                    at,
                    note: prompt.text,
                }
            }
            Annotation::Compaction { id, at, record } => {
                no_content(&messages)?;
                Entry::Compaction { id, at, record }
            }
            Annotation::Plain { id, at, text } => {
                no_content(&messages)?;
                Entry::Plain { id, at, text }
            }
            Annotation::Notice { id, at, text } => {
                no_content(&messages)?;
                Entry::Notice { id, at, text }
            }
            Annotation::Tally { id, at, text } => {
                no_content(&messages)?;
                Entry::Tally { id, at, text }
            }
        })
    }
}

fn read_prompt(
    messages: Vec<Content>,
    shown: Option<String>,
    relayed: Option<String>,
) -> Result<Prompt, serde_json::Error> {
    let mut messages = messages.into_iter();
    let Some(Content::User(body)) = messages.next() else {
        return Err(invalid("missing input content"));
    };
    if messages.next().is_some() {
        return Err(invalid("multiple input messages for one entry"));
    }
    let mut body = body.into_iter();
    let Some(Block::Text(text)) = body.next() else {
        return Err(invalid("missing input text"));
    };
    let images = body
        .map(|block| match block {
            Block::Image(image) => Ok(image),
            Block::Text(_) | Block::Data(_) | Block::Reasoning(_) => {
                Err(invalid("invalid input attachment"))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(Prompt {
        text,
        images,
        shown,
        relayed,
    })
}

fn no_content(messages: &[Content]) -> Result<(), serde_json::Error> {
    if messages.is_empty() {
        Ok(())
    } else {
        Err(invalid("conversation content attached to an annotation"))
    }
}

fn invalid(message: &str) -> serde_json::Error {
    serde::de::Error::custom(message)
}
