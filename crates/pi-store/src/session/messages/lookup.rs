//! Indexed request reads from the authoritative workspace database.

use super::{Entry, Location, database};
use anyhow::{Result, ensure};
use message::{MessageId, SessionId};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Lookup {
    location: Location,
    connection: Connection,
    count: usize,
}

impl Lookup {
    pub fn committed(at: &Location, session: &SessionId, count: usize) -> Result<Self> {
        ensure!(
            &at.session == session,
            "request archive belongs to another session"
        );
        let mut lookup =
            Self::open(at)?.ok_or_else(|| anyhow::anyhow!("request archive is unavailable"))?;
        ensure!(
            lookup.count >= count,
            "request archive does not contain this commit"
        );
        lookup.count = count;
        Ok(lookup)
    }

    pub fn open(at: &Location) -> Result<Option<Self>> {
        if !at.database.is_file() {
            return Ok(None);
        }
        let connection = database::connect(&at.database)?;
        let Some(count) = super::count(&connection, at)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            location: at.clone(),
            connection,
            count,
        }))
    }

    pub fn location(&self) -> &Location {
        &self.location
    }
    pub fn count(&self) -> usize {
        self.count
    }
    pub fn session(&self) -> &SessionId {
        &self.location.session
    }

    pub fn position(&self, id: &MessageId, before: usize) -> Result<Option<usize>> {
        Ok(self
            .connection
            .query_row(
                "SELECT position FROM requests WHERE session=?1 AND id=?2 AND position<?3",
                params![self.session().as_str(), id.as_str(), before.min(self.count)],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn automatic_request(
        &self,
        source: &str,
        event: &str,
        before: usize,
    ) -> Result<Option<MessageId>> {
        let id: Option<String>=self.connection.query_row("SELECT id FROM requests WHERE session=?1 AND source=?2 AND event=?3 AND position<?4 ORDER BY position LIMIT 1", params![self.session().as_str(),source,event,before.min(self.count)], |r|r.get(0)).optional()?;
        Ok(id.map(MessageId::new).transpose()?)
    }

    pub fn get(&self, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        self.find("id", id, before)
    }
    pub fn response(&self, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        self.find("request_id", id, before)
    }

    fn find(&self, column: &str, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        let sql = format!(
            "SELECT r.metadata,c.body FROM requests r JOIN body_values c ON c.id=r.body
             WHERE r.session=?1 AND r.{column}=?2 AND r.position<?3"
        );
        let row: Option<(String, String)> = self
            .connection
            .query_row(
                &sql,
                params![self.session().as_str(), id.as_str(), before.min(self.count)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(metadata, body)| super::decode(metadata, body))
            .transpose()
    }

    /// Read one committed prefix in order, validating positions and indexed identities.
    pub(super) fn visit(&self, mut visit: impl FnMut(usize, Entry) -> Result<()>) -> Result<()> {
        let mut statement = self.connection.prepare(
            "SELECT r.position,r.id,r.metadata,c.body FROM requests r JOIN body_values c ON c.id=r.body
             WHERE r.session=?1 AND r.position<?2 ORDER BY r.position"
        )?;
        let mut rows = statement.query(params![self.session().as_str(), self.count])?;
        let mut next = 0;
        while let Some(row) = rows.next()? {
            let position: usize = row.get(0)?;
            ensure!(position == next, "missing request position");
            let id: String = row.get(1)?;
            let entry = super::decode(row.get(2)?, row.get(3)?)?;
            ensure!(
                entry.message.id.as_str() == id,
                "message archive identifier index mismatch"
            );
            visit(position, entry)?;
            next += 1;
        }
        ensure!(next == self.count, "missing request position");
        Ok(())
    }

    pub fn at(&self, position: usize, before: usize) -> Result<Option<Entry>> {
        if position >= before.min(self.count) {
            return Ok(None);
        }
        let row:Option<(String,String)>=self.connection.query_row("SELECT r.metadata,c.body FROM requests r JOIN body_values c ON c.id=r.body WHERE r.session=?1 AND r.position=?2",params![self.session().as_str(),position],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        row.map(|(meta, body)| super::decode(meta, body))
            .transpose()
    }
}
