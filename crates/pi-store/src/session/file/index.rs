//! Session-owned offsets and derived state. No conversation bodies are cached here.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use message::MessageId;
use serde_json::Value;
use transcript::{Entry, EntryId};

use super::record::{Address, Item, Record, Start};
use super::{Location, Metadata, State};

type PendingRequest<'a> = (usize, &'a Item, RequestAddress, Arc<Value>);

#[derive(Debug, Clone, Copy)]
pub(in crate::session) struct RequestAddress {
    pub(in crate::session) record: Address,
    pub(in crate::session) call: Option<usize>,
}

#[derive(Debug, Clone)]
pub(in crate::session) struct RequestIndex {
    pub(in crate::session) address: RequestAddress,
    pub(in crate::session) original: Option<usize>,
}

#[derive(Debug)]
pub(in crate::session) struct Index {
    pub(in crate::session) state: State,
    pub(in crate::session) end: u64,
    pub(in crate::session) context: Vec<(EntryId, Address)>,
    pub(super) answers: Vec<Address>,
    pub(in crate::session) requests: Vec<RequestIndex>,
    pub(in crate::session) ids: HashMap<MessageId, usize>,
    pub(in crate::session) responses: HashMap<MessageId, usize>,
    pub(in crate::session) triggers: HashMap<(String, String), usize>,
    pub(in crate::session) unbound: HashMap<MessageId, Address>,
}

impl Index {
    pub(in crate::session) fn new(session: message::SessionId) -> Self {
        Self {
            state: State::new(session),
            end: 0,
            context: Vec::new(),
            answers: Vec::new(),
            requests: Vec::new(),
            ids: HashMap::new(),
            responses: HashMap::new(),
            triggers: HashMap::new(),
            unbound: HashMap::new(),
        }
    }

    pub(in crate::session) fn apply(
        &mut self,
        at: &Location,
        offset: u64,
        records: &[Record],
    ) -> Result<()> {
        let mut requests = Vec::new();
        for (part, record) in records.iter().enumerate() {
            let address = Address(offset, part);
            self.fact(at, record, address, &mut requests)?;
        }
        requests.sort_by_key(|(position, ..)| *position);
        for (position, item, address, canonical) in requests {
            self.request(at, position, item, address, &canonical)?;
        }
        self.state.entries = self.context.len();
        self.state.last = self.context.last().map(|(id, _)| *id);
        Ok(())
    }

    fn fact<'a>(
        &mut self,
        at: &Location,
        record: &'a Record,
        address: Address,
        requests: &mut Vec<PendingRequest<'a>>,
    ) -> Result<()> {
        match record {
            Record::Session(start) => self.start(at, start),
            Record::Rewind { from } => self.rewind(at, *from),
            Record::Meta { fields } => self.patch(fields),
            _ => {
                let item = record.item().context("invalid session record")?;
                let canonical = Arc::new(item.resolved_body(at, 0)?);
                self.context(record, address, &canonical)?;
                self.stage_request(item, address, None, &canonical, requests);
                for (call, request) in item.calls.iter().enumerate() {
                    self.stage_request(request, address, Some(call), &canonical, requests);
                }
                Ok(())
            }
        }
    }

    fn rewind(&mut self, at: &Location, from: usize) -> Result<()> {
        ensure!(from <= self.context.len(), "invalid conversation rewind");
        self.context.truncate(from);
        self.state.interrupted = None;
        self.summarize(at)?;
        Ok(())
    }

    fn start(&mut self, at: &Location, start: &Start) -> Result<()> {
        ensure!(
            self.end == 0 && self.state.metadata.is_none(),
            "duplicate session header"
        );
        ensure!(
            start.format == super::FORMAT && start.id == at.session,
            "unsupported session header"
        );
        self.state.requests = Some(0);
        self.state.metadata = Some(Metadata {
            workspace: start.workspace.clone(),
            model: start.model.clone(),
            name: start.name.clone(),
            created: start.created,
            spent: start.spent.unwrap_or_default(),
            parent: start.parent.clone(),
            touched: start.created,
            rounds: 0,
            prompt: String::new(),
        });
        Ok(())
    }

    fn context(&mut self, record: &Record, address: Address, canonical: &Value) -> Result<()> {
        let item = record.item().context("invalid session record")?;
        if let Some(metadata) = &mut self.state.metadata {
            metadata.spent.add(
                &item.usage.unwrap_or_default(),
                item.cost.unwrap_or_default(),
            );
        }
        let Some(entry) = item.entry else {
            return Ok(());
        };
        ensure!(
            self.context.last().is_none_or(|(last, _)| entry > *last),
            "non-monotonic context identifier"
        );
        let restored = record.entry(canonical)?;
        if matches!(record, Record::Input(_)) {
            self.answers.clear();
        } else if matches!(record, Record::Assistant(_)) {
            self.answers.push(address);
        }
        self.context.push((entry, address));
        self.state.next = self.state.next.max(entry.0 + 1);
        self.extend_summary(&restored);
        if let Some(id) = item
            .id
            .as_str()
            .map(|id| MessageId::new(id.to_owned()))
            .transpose()?
        {
            self.unbound.remove(&id);
        }
        Ok(())
    }

    fn extend_summary(&mut self, entry: &Entry) {
        let Some(metadata) = &mut self.state.metadata else {
            return;
        };
        metadata.touched = entry.at();
        if let Entry::Ask { ask, .. } = entry {
            if metadata.rounds == 0 {
                metadata.prompt = crate::text::clip(ask.shown_text(), super::super::RESUME_WIDTH);
            }
            metadata.rounds += 1;
        }
    }

    fn stage_request<'a>(
        &self,
        item: &'a Item,
        record: Address,
        call: Option<usize>,
        canonical: &Arc<Value>,
        requests: &mut Vec<PendingRequest<'a>>,
    ) {
        if let Some(position) = item.position {
            requests.push((
                position,
                item,
                RequestAddress { record, call },
                canonical.clone(),
            ));
        }
    }

    fn request(
        &mut self,
        at: &Location,
        position: usize,
        item: &Item,
        address: RequestAddress,
        canonical: &Value,
    ) -> Result<()> {
        ensure!(position == self.requests.len(), "missing request position");
        let entry = item.message(at, address.record, canonical)?;
        ensure!(
            !self.ids.contains_key(&entry.message.id),
            "conflicting request identifier"
        );
        let original = if let Some(request) = &item.request {
            ensure!(
                !self.responses.contains_key(request),
                "request already has a response"
            );
            let position = self
                .ids
                .get(request)
                .copied()
                .context("response refers to an unknown request")?;
            self.unbound.remove(request);
            self.responses.insert(request.clone(), self.requests.len());
            Some(position)
        } else {
            None
        };
        if let Some((source, event)) = entry.message.automatic_trigger() {
            self.triggers
                .entry((source.to_owned(), event.to_owned()))
                .or_insert(position);
        }
        if matches!(
            entry.message.kind,
            message::WireKind::Request(message::Request::ToModel(_))
        ) && item.entry.is_none()
        {
            self.unbound
                .insert(entry.message.id.clone(), address.record);
        }
        self.ids.insert(entry.message.id, position);
        self.requests.push(RequestIndex { address, original });
        self.state.requests = Some(self.requests.len());
        Ok(())
    }

    fn patch(&mut self, fields: &std::collections::BTreeMap<String, Value>) -> Result<()> {
        for (key, value) in fields {
            match key.as_str() {
                "next" => self.state.next = serde_json::from_value(value.clone())?,
                "interrupted" => self.state.interrupted = serde_json::from_value(value.clone())?,
                _ => {
                    let metadata = self
                        .state
                        .metadata
                        .as_mut()
                        .context("missing session header")?;
                    let mut updated = serde_json::to_value(&*metadata)?;
                    ensure!(
                        updated.get(key).is_some(),
                        "unknown session metadata field {key}"
                    );
                    updated[key] = value.clone();
                    *metadata = serde_json::from_value(updated)?;
                }
            }
        }
        Ok(())
    }

    fn summarize(&mut self, at: &Location) -> Result<()> {
        if let Some(metadata) = &mut self.state.metadata {
            metadata.rounds = 0;
            metadata.prompt.clear();
            metadata.touched = metadata.created;
        }
        self.answers.clear();
        let mut reader = super::Reader::new(at)?;
        for position in 0..self.context.len() {
            let address = self.context[position].1;
            let record = reader.record(address)?;
            if matches!(record, Record::Input(_)) {
                self.answers.clear();
            } else if matches!(record, Record::Assistant(_)) {
                self.answers.push(address);
            }
            let entry = reader.entry(address)?;
            self.extend_summary(&entry);
        }
        Ok(())
    }
}
