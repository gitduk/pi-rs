//! Request identity and response state referencing the workspace's shared content.

pub(crate) mod legacy;
mod lookup;
pub use lookup::Lookup;

use super::{Location, database};
use anyhow::{Context, Result, ensure};
use message::{MessageId, SessionId, WireKind, WireMessage};
use rusqlite::{OptionalExtension, Transaction, params};
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

/// Request records are append-only even when the conversation is rewound.
pub fn write_from<T: Serialize>(
    at: &Location,
    session: &SessionId,
    base: usize,
    entries: &[T],
) -> Result<()> {
    let mut c = database::connect(&at.database)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    write_in(&tx, at, session, base, entries)?;
    tx.commit()?;
    Ok(())
}

pub fn write_in<T: Serialize>(
    tx: &Transaction<'_>,
    at: &Location,
    session: &SessionId,
    base: usize,
    entries: &[T],
) -> Result<()> {
    ensure!(
        &at.session == session,
        "request archive belongs to another session"
    );
    let from: usize = tx.query_row(
        "SELECT COALESCE(MAX(position)+1,0) FROM requests WHERE session=?1",
        [session.as_str()],
        |r| r.get(0),
    )?;
    ensure!(base <= from, "request archive is missing an earlier save");
    let end = base
        .checked_add(entries.len())
        .context("request archive size overflow")?;
    for (offset, entry) in entries.iter().enumerate().skip(from.saturating_sub(base)) {
        let mut metadata = serde_json::to_value(entry)?;
        let body = metadata["message"]
            .as_object_mut()
            .context("request record has no message")?
            .remove("body")
            .context("request has no body")?;
        let body_id = database::intern(tx, &body)?;
        let mut wire = metadata["message"].clone();
        wire["body"] = body;
        let wire: message::WireMessage = serde_json::from_value(wire)?;
        ensure!(
            &wire.session == session,
            "request belongs to another session"
        );
        let request = match &wire.kind {
            WireKind::Request(_) => None,
            WireKind::Response { request, .. } => Some(request.as_str()),
        };
        let trigger = wire.automatic_trigger();
        tx.execute(
            "INSERT INTO requests VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                session.as_str(),
                base + offset,
                wire.id.as_str(),
                request,
                trigger.map(|t| t.0),
                trigger.map(|t| t.1),
                serde_json::to_string(&metadata)?,
                body_id
            ],
        )?;
    }
    tx.execute("INSERT INTO request_commits VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET count=MAX(count,excluded.count)", params![session.as_str(),end])?;
    Ok(())
}

pub fn visit(at: &Location, mut visit: impl FnMut(usize, Entry) -> Result<()>) -> Result<()> {
    Lookup::open(at)?
        .context("request archive has no records")?
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

pub fn write<T: Serialize>(at: &Location, session: &SessionId, entries: &[T]) -> Result<()> {
    if let Some(lookup) = Lookup::open(at)? {
        ensure!(
            entries.len() >= lookup.count(),
            "request archive cannot discard recorded interactions"
        );
    }
    write_from(at, session, 0, entries)
}

pub(crate) fn decode(metadata: String, body: String) -> Result<Entry> {
    let mut value: serde_json::Value = serde_json::from_str(&metadata)?;
    value["message"]["body"] = serde_json::from_str(&body)?;
    Ok(serde_json::from_value(value)?)
}

pub(crate) fn count(c: &rusqlite::Connection, at: &Location) -> Result<Option<usize>> {
    Ok(c.query_row(
        "SELECT count FROM request_commits WHERE session=?1",
        [at.session.as_str()],
        |r| r.get(0),
    )
    .optional()?)
}
