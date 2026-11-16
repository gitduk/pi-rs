//! Request identity and response state in the session's append-only file.

mod lookup;
pub use lookup::{Lookup, Record};

use super::{Location, Transaction, file};
use anyhow::{Result, ensure};
use message::{MessageId, SessionId, WireKind, WireMessage};
use serde::{Deserialize, Serialize};

pub const FORMAT: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub format: u8,
    pub session: SessionId,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub message: WireMessage,
    pub parent: Option<MessageId>,
    pub native_call: Option<String>,
}

pub fn write_from<T: Serialize>(
    at: &Location,
    session: &SessionId,
    base: usize,
    entries: &[T],
) -> Result<()> {
    file::update(at, |tx| write_in(tx, at, session, base, entries))
}

/// Stage requests alongside the conversation, published in the same durable line.
pub fn write_in<T: Serialize>(
    tx: &mut Transaction,
    at: &Location,
    session: &SessionId,
    base: usize,
    entries: &[T],
) -> Result<()> {
    ensure!(
        &at.session == session && &tx.state.session == session,
        "request archive belongs to another session"
    );
    let from = tx.state.requests.unwrap_or(0);
    ensure!(base <= from, "request archive is missing an earlier save");
    let retained = Lookup::open(at)?;
    let end = base
        .checked_add(entries.len())
        .ok_or_else(|| anyhow::anyhow!("request archive size overflow"))?;
    for (offset, entry) in entries.iter().enumerate() {
        let position = base + offset;
        if position < from {
            let previous = retained
                .as_ref()
                .map(|lookup| lookup.at(position, lookup.count()))
                .transpose()?
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("missing recorded interaction"))?;
            ensure!(
                serde_json::to_value(&previous)? == serde_json::to_value(entry)?,
                "saved interaction conflicts with retained content"
            );
            continue;
        }
        let entry: Entry = serde_json::from_value(serde_json::to_value(entry)?)?;
        ensure!(
            &entry.message.session == session,
            "request belongs to another session"
        );
        validate_new(tx, retained.as_ref(), &entry)?;
        tx.request(base + offset, entry);
    }
    tx.state.requests = Some(from.max(end));
    Ok(())
}

fn validate_new(tx: &mut Transaction, retained: Option<&Lookup>, entry: &Entry) -> Result<()> {
    ensure!(
        retained
            .map(|lookup| lookup.position(&entry.message.id, lookup.count()))
            .transpose()?
            .flatten()
            .is_none()
            && tx.identities.insert(entry.message.id.clone()),
        "conflicting request identifier"
    );
    let response = match &entry.message.kind {
        WireKind::Request(_) => None,
        WireKind::Response { request, .. } => Some(request),
    };
    if let Some(request) = response {
        ensure!(
            request != &entry.message.id,
            "response cannot refer to itself"
        );
        ensure!(
            tx.requests.contains(request)
                || retained.is_some_and(|lookup| lookup.has_request(request)),
            "response refers to an unknown request"
        );
        ensure!(
            !retained.is_some_and(|lookup| lookup.has_response(request))
                && tx.responses.insert(request.clone()),
            "request already has a response"
        );
    }
    if matches!(entry.message.kind, WireKind::Request(_)) {
        tx.requests.insert(entry.message.id.clone());
    }
    Ok(())
}

pub fn visit(at: &Location, mut visit: impl FnMut(usize, Entry) -> Result<()>) -> Result<()> {
    Lookup::open(at)?
        .ok_or_else(|| anyhow::anyhow!("request archive has no records"))?
        .visit(&mut visit)
}

pub fn read(at: &Location) -> Result<Option<Archive>> {
    let Some(lookup) = Lookup::open(at)? else {
        return Ok(None);
    };
    let mut entries = Vec::with_capacity(lookup.count());
    lookup.visit(|_, entry| {
        entries.push(entry);
        Ok(())
    })?;
    Ok(Some(Archive {
        format: FORMAT,
        session: at.session.clone(),
        entries,
    }))
}
