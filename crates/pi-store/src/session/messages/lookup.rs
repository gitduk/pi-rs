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
            "SELECT position FROM requests WHERE session=?1 AND {column}=?2 AND position<?3"
        );
        let position: Option<usize> = self
            .connection
            .query_row(
                &sql,
                params![self.session().as_str(), id.as_str(), before.min(self.count)],
                |r| r.get(0),
            )
            .optional()?;
        position
            .map(|p| self.at(p, before))
            .transpose()
            .map(Option::flatten)
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
