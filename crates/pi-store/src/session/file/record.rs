//! Durable facts and references to their canonical bodies.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::{Context, Result, bail, ensure};
use message::{Block, MessageId, Request, Status};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use transcript::{Entry, EntryId, archive::Annotation};

use super::{Location, Metadata};

/// A record inside one complete JSONL line. References never use mutable positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::session) struct Address(pub u64, pub usize);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::session) struct Reference {
    pub source: Address,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(in crate::session) enum Body {
    Reference {
        reference: Reference,
    },
    Join {
        join: Vec<Body>,
    },
    Template {
        template: Value,
        fields: BTreeMap<String, Body>,
    },
    Literal(Value),
}

impl Body {
    pub fn reference(source: Address, path: impl Into<String>) -> Self {
        Self::Reference {
            reference: Reference {
                source,
                path: path.into(),
            },
        }
    }

    pub fn resolve(
        &self,
        at: &Location,
        current: Option<(Address, &Value)>,
        depth: usize,
    ) -> Result<Value> {
        ensure!(
            depth < super::REFERENCE_DEPTH,
            "session body reference is too deep"
        );
        match self {
            Self::Literal(value) => Ok(value.clone()),
            Self::Reference { reference } => {
                let body = match current {
                    Some((address, body)) if address == reference.source => Cow::Borrowed(body),
                    _ => Cow::Owned(super::read_body(at, reference.source, depth + 1)?),
                };
                body.pointer(&reference.path)
                    .cloned()
                    .context("session body reference is missing")
            }
            Self::Join { join } => Self::join(join, at, current, depth),
            Self::Template { template, fields } => Self::fill(template, fields, at, current, depth),
        }
    }
    fn join(
        join: &[Body],
        at: &Location,
        current: Option<(Address, &Value)>,
        depth: usize,
    ) -> Result<Value> {
        let mut text = String::new();
        for part in join {
            let value = part.resolve(at, current, depth + 1)?;
            text.push_str(
                value
                    .as_str()
                    .context("session text reference is not text")?,
            );
        }
        Ok(Value::String(text))
    }
    fn fill(
        template: &Value,
        fields: &BTreeMap<String, Body>,
        at: &Location,
        current: Option<(Address, &Value)>,
        depth: usize,
    ) -> Result<Value> {
        let mut value = template.clone();
        for (path, field) in fields {
            *value
                .pointer_mut(path)
                .context("invalid session body template")? =
                field.resolve(at, current, depth + 1)?;
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(in crate::session) enum Outcome {
    Name(Completion),
    Failure { failed: message::Failure },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::session) enum Completion {
    Done,
    Stopped,
}

impl From<&Status> for Outcome {
    fn from(status: &Status) -> Self {
        match status {
            Status::Done => Self::Name(Completion::Done),
            Status::Stopped => Self::Name(Completion::Stopped),
            Status::Failed(failed) => Self::Failure {
                failed: failed.clone(),
            },
        }
    }
}

impl From<Outcome> for Status {
    fn from(status: Outcome) -> Self {
        match status {
            Outcome::Name(Completion::Done) => Self::Done,
            Outcome::Name(Completion::Stopped) => Self::Stopped,
            Outcome::Failure { failed } => Self::Failed(failed),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::session) struct Item {
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    pub at: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<Body>,
    /// Present only when the protocol and context projections differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_body: Option<Vec<Body>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Outcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Request>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_call: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<llm::stream::Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

impl Item {
    pub fn new(id: Value, at: u64, body: Vec<Body>) -> Self {
        Self {
            id,
            at,
            body,
            entry: None,
            message_body: None,
            position: None,
            request: None,
            status: None,
            kind: None,
            by: None,
            parent: None,
            native_call: None,
            message_at: None,
            annotation: None,
            calls: Vec::new(),
            usage: None,
            cost: None,
        }
    }

    pub fn resolved_body(&self, at: &Location, depth: usize) -> Result<Value> {
        self.body
            .iter()
            .map(|part| part.resolve(at, None, depth))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array)
    }

    pub fn message(
        &self,
        at: &Location,
        address: Address,
        canonical: &Value,
    ) -> Result<super::super::messages::Entry> {
        let body = if let Some(body) = &self.message_body {
            let blocks = body
                .iter()
                .map(|part| part.resolve(at, Some((address, canonical)), 0))
                .collect::<Result<Vec<_>>>()?;
            serde_json::from_value(Value::Array(blocks))?
        } else {
            canonical.as_array().context("invalid session body")?;
            Vec::<Block>::deserialize(canonical)?
        };
        let kind = match (&self.request, &self.status) {
            (Some(request), Some(status)) => message::WireKind::Response {
                request: request.clone(),
                status: status.clone().into(),
            },
            (None, None) => message::WireKind::Request(
                self.kind
                    .clone()
                    .unwrap_or(Request::ToModel(message::InputKind::User)),
            ),
            _ => bail!("incomplete interaction outcome"),
        };
        Ok(super::super::messages::Entry {
            message: message::WireMessage {
                id: serde_json::from_value(self.id.clone())?,
                session: at.session.clone(),
                at: self.message_at.unwrap_or(self.at),
                by: self.by.clone(),
                kind,
                body,
            },
            parent: self.parent.clone(),
            native_call: self.native_call.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::session) struct Start {
    pub format: u8,
    pub id: message::SessionId,
    pub workspace: String,
    pub model: String,
    pub created: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent: Option<llm::Totals>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(in crate::session) enum Record {
    Session(Start),
    Input(Item),
    Assistant(Item),
    ToolResult(Item),
    Finished(Item),
    Note(Item),
    Compact(Item),
    Rewind {
        from: usize,
    },
    Meta {
        #[serde(flatten)]
        fields: BTreeMap<String, Value>,
    },
}

impl Record {
    pub fn item(&self) -> Option<&Item> {
        match self {
            Self::Input(item)
            | Self::Assistant(item)
            | Self::ToolResult(item)
            | Self::Finished(item)
            | Self::Note(item)
            | Self::Compact(item) => Some(item),
            _ => None,
        }
    }
    pub fn item_mut(&mut self) -> Option<&mut Item> {
        match self {
            Self::Input(item)
            | Self::Assistant(item)
            | Self::ToolResult(item)
            | Self::Finished(item)
            | Self::Note(item)
            | Self::Compact(item) => Some(item),
            _ => None,
        }
    }
    fn annotation_kind(&self) -> &'static str {
        match self {
            Self::Input(_) => "ask",
            Self::Assistant(_) => "answer",
            Self::ToolResult(_) => "tool",
            Self::Note(_) => "note",
            Self::Compact(_) => "compaction",
            _ => "",
        }
    }

    pub fn from_entry(entry: &Entry) -> Result<Self> {
        let archived = transcript::archive::Record::from_entry(entry);
        let mut annotation = serde_json::to_value(archived.metadata)?;
        let map = annotation.as_object_mut().expect("annotation is an object");
        let kind = map.remove("kind").context("missing context kind")?;
        map.remove("id");
        map.remove("at");
        map.retain(|_, value| !value.is_null());
        let mut body = content_body(archived.messages)?;
        if let Some(text) = map.remove("text") {
            body.push(Body::Literal(json!({"type":"text", "value":text})));
        }
        let mut item = Item::new(json!(entry.id()), entry.at(), body);
        item.entry = Some(entry.id());
        let mut record = Self::context_record(entry, item);
        if kind.as_str() != Some(record.annotation_kind()) {
            map.insert("kind".into(), kind);
        }
        if !map.is_empty() {
            record.item_mut().expect("context record").annotation = Some(annotation);
        }
        Ok(record)
    }

    fn context_record(entry: &Entry, item: Item) -> Self {
        match entry {
            Entry::Ask { .. } | Entry::Bash { .. } => Self::Input(item),
            Entry::Answer { .. } => Self::Assistant(item),
            Entry::Tool { .. } => Self::ToolResult(item),
            Entry::Compaction { .. } => Self::Compact(item),
            _ => Self::Note(item),
        }
    }

    pub fn entry(&self, body: &Value) -> Result<Entry> {
        let item = self.item().context("record has no context")?;
        let id = item.entry.context("record has no context identifier")?;
        let mut annotation = item.annotation.clone().unwrap_or_else(|| json!({}));
        annotation["id"] = json!(id);
        annotation["at"] = json!(item.at);
        if annotation.get("kind").is_none() {
            annotation["kind"] = json!(self.annotation_kind());
        }
        let kind = annotation["kind"]
            .as_str()
            .context("invalid context kind")?
            .to_owned();
        let messages = context_content(&kind, body, &mut annotation)?;
        let metadata: Annotation = serde_json::from_value(annotation)?;
        Ok(transcript::archive::Record { metadata, messages }.into_entry()?)
    }
}

fn context_content(
    kind: &str,
    body: &Value,
    annotation: &mut Value,
) -> Result<Vec<message::conversation::Content>> {
    use message::conversation::Content;
    let blocks = body.as_array().context("invalid context body")?;
    match kind {
        "ask" | "bash" | "note" | "context" => {
            Ok(vec![Content::User(Vec::<Block>::deserialize(body)?)])
        }
        "answer" => answer_content(blocks),
        "tool" => {
            ensure!(blocks.len() == 1, "invalid tool result body");
            Ok(vec![Content::ToolResult(message::ToolResult::deserialize(
                &blocks[0]["value"],
            )?)])
        }
        "plain" | "notice" | "tally" => {
            annotation["text"] = blocks.first().context("missing note text")?["value"].clone();
            Ok(Vec::new())
        }
        "compaction" => Ok(Vec::new()),
        _ => bail!("unknown context annotation"),
    }
}

fn answer_content(blocks: &[Value]) -> Result<Vec<message::conversation::Content>> {
    use message::conversation::Content;
    blocks
        .iter()
        .map(|block| {
            if block["type"] == "tool_call" {
                Ok(Content::ToolCall(message::ToolCall::deserialize(
                    &block["value"],
                )?))
            } else {
                Ok(Content::Assistant(vec![Block::deserialize(block)?]))
            }
        })
        .collect::<Result<Vec<_>>>()
}

fn content_body(messages: Vec<message::conversation::Content>) -> Result<Vec<Body>> {
    use message::conversation::Content;
    let mut body = Vec::new();
    for content in messages {
        match content {
            Content::User(blocks) | Content::Assistant(blocks) => body.extend(
                blocks
                    .into_iter()
                    .map(|block| serde_json::to_value(block).map(Body::Literal))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Content::ToolCall(call) => {
                body.push(Body::Literal(json!({"type":"tool_call", "value":call})))
            }
            Content::ToolResult(result) => {
                body.push(Body::Literal(json!({"type":"tool_result", "value":result})))
            }
        }
    }
    Ok(body)
}

impl Start {
    pub fn new(id: message::SessionId, metadata: &Metadata) -> Self {
        Self {
            format: super::FORMAT,
            id,
            workspace: metadata.workspace.clone(),
            model: metadata.model.clone(),
            created: metadata.created,
            name: metadata.name.clone(),
            parent: metadata.parent.clone(),
            spent: (metadata.spent != llm::Totals::default()).then_some(metadata.spent),
        }
    }
}
