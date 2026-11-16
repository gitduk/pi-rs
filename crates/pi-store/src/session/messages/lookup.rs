//! Request views over the session's own offset index.

use anyhow::{Result, ensure};
use message::{MessageId, SessionId};

use super::{Entry, Location, file};

pub struct Lookup {
    location: Location,
    count: usize,
}

/// A committed address can be read after releasing the live request table.
pub struct Record {
    location: Location,
    address: file::RequestAddress,
}

impl Record {
    pub fn read(&self) -> Result<Entry> {
        let entry = file::request(&self.location, self.address)?;
        ensure!(
            entry.message.session == self.location.session,
            "interaction belongs to another session"
        );
        Ok(entry)
    }
}

impl Lookup {
    pub fn open(at: &Location) -> Result<Option<Self>> {
        let index = at.refresh()?;
        let Some(count) = index.state.requests else {
            return Ok(None);
        };
        Ok(Some(Self {
            location: at.clone(),
            count,
        }))
    }
    pub fn refresh(&mut self) -> Result<()> {
        self.count = self.location.refresh()?.requests.len();
        Ok(())
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
            .location
            .cached_index()
            .ids
            .get(id)
            .copied()
            .filter(|position| *position < before.min(self.count)))
    }
    pub(super) fn has_request(&self, id: &MessageId) -> bool {
        let index = self.location.cached_index();
        index.ids.get(id).is_some_and(|position| {
            *position < self.count && index.requests[*position].original.is_none()
        })
    }
    pub(super) fn has_response(&self, id: &MessageId) -> bool {
        self.location.cached_index().responses.contains_key(id)
    }
    pub fn automatic_request(
        &self,
        source: &str,
        event: &str,
        before: usize,
    ) -> Result<Option<MessageId>> {
        let position = self
            .location
            .cached_index()
            .triggers
            .get(&(source.into(), event.into()))
            .copied();
        match position {
            Some(position) => Ok(self.at(position, before)?.map(|entry| entry.message.id)),
            None => Ok(None),
        }
    }
    pub fn get(&self, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        match self.position(id, before)? {
            Some(position) => self.at(position, before),
            None => Ok(None),
        }
    }
    pub fn response(&self, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        let position = self.location.cached_index().responses.get(id).copied();
        match position {
            Some(position) => self.at(position, before),
            None => Ok(None),
        }
    }
    pub(super) fn visit(&self, mut visit: impl FnMut(usize, Entry) -> Result<()>) -> Result<()> {
        let addresses = self.location.cached_index().requests[..self.count]
            .iter()
            .map(|request| request.address)
            .collect::<Vec<_>>();
        let mut reader = file::Reader::new(&self.location)?;
        for (position, address) in addresses.into_iter().enumerate() {
            visit(position, reader.request(address)?)?;
        }
        Ok(())
    }
    pub fn record(&self, position: usize, before: usize) -> Option<Record> {
        if position >= before.min(self.count) {
            return None;
        }
        let index = self.location.cached_index();
        Some(Record {
            location: self.location.clone(),
            address: index.requests[position].address,
        })
    }
    pub fn original(&self, position: usize, before: usize) -> Option<Record> {
        if position >= before.min(self.count) {
            return None;
        }
        let original = self.location.cached_index().requests[position].original;
        original.and_then(|original| self.record(original, position))
    }
    pub fn at(&self, position: usize, before: usize) -> Result<Option<Entry>> {
        self.record(position, before)
            .map(|record| record.read())
            .transpose()
    }
}
