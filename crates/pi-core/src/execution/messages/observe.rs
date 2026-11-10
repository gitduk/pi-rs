//! Cursor-based observation of the session log, independent of execution.

use super::{Error, LIMITS, Messages, Table};
use message::{Message, RequestId, WireKind};
use pi_store::session::messages::Lookup;
use std::path::PathBuf;
use std::sync::Arc;

pub use ::execution::channel::{Cursor, Observed};

/// Pulls from the retained log. A slow observer retains no queue of messages.
pub struct Observation {
    messages: Messages,
    cursor: Cursor,
    changed: tokio::sync::watch::Receiver<usize>,
    reading: Option<tokio::task::JoinHandle<Result<Arc<Message>, Error>>>,
}

enum Next {
    Memory(Option<Arc<Message>>),
    Archive { path: PathBuf, before: usize },
}

impl Messages {
    /// The current end of the log, suitable for a new live observation.
    pub fn cursor(&self) -> Cursor {
        let table = self.lock();
        Cursor {
            session: table.session.clone(),
            next: table.base + table.order.len(),
        }
    }

    /// Locate the original input so a receiver can attach after acceptance
    /// without racing a fast completion.
    pub fn request_cursor(&self, request: &RequestId) -> Result<Cursor, Error> {
        let mut table = self.lock();
        if !table.ensure_request(request.message())? {
            return Err(Error::Parent);
        }
        Ok(Cursor {
            session: table.session.clone(),
            next: table.requests[request.message()].request_position,
        })
    }

    /// Observe from a saved cursor, or include unfinished inputs when none is given.
    /// Registration and the initial position are captured under the same lock.
    pub fn observe(&self, cursor: Option<Cursor>) -> Result<Observation, Error> {
        let table = self.lock();
        let end = table.base + table.order.len();
        let cursor = cursor.unwrap_or_else(|| Cursor {
            session: table.session.clone(),
            next: table
                .requests
                .values()
                .filter(|r| r.parent.is_none() && r.response.is_none())
                .map(|r| r.request_position)
                .min()
                .unwrap_or(end),
        });
        if cursor.session != table.session {
            return Err(Error::Session);
        }
        if cursor.next > end {
            return Err(Error::Cursor);
        }
        Ok(Observation {
            messages: self.clone(),
            cursor,
            changed: table.changed.subscribe(),
            reading: None,
        })
    }
}

impl Observation {
    pub fn cursor(&self) -> &Cursor {
        &self.cursor
    }

    /// Advance only after a complete message has been read and validated.
    /// Cancelling this wait does not skip a message or cancel its request.
    pub async fn next(&mut self) -> Result<Observed, Error> {
        loop {
            let message = if let Some(reading) = &mut self.reading {
                let result = reading.await;
                self.reading = None;
                Some(result.map_err(|error| Error::Storage(error.into()))??)
            } else {
                let next = {
                    let table = self.messages.lock();
                    table.next_observed(self.cursor.next)?
                };
                match next {
                    Next::Memory(message) => message,
                    Next::Archive { path, before } => {
                        let session = self.cursor.session.clone();
                        let at = self.cursor.next;
                        self.reading = Some(tokio::task::spawn_blocking(move || {
                            read_committed(&path, &session, at, before)
                        }));
                        continue;
                    }
                }
            };
            if let Some(message) = message {
                self.cursor.next += 1;
                return Ok(Observed {
                    cursor: self.cursor.clone(),
                    message,
                });
            }
            self.changed
                .changed()
                .await
                .map_err(|_| Error::ObservationClosed)?;
        }
    }
}

impl Table {
    pub(super) fn publish(&self) {
        self.changed.send_replace(self.base + self.order.len());
    }

    fn next_observed(&self, at: usize) -> Result<Next, Error> {
        if at >= self.base + self.order.len() {
            return Ok(Next::Memory(None));
        }
        if at >= self.base {
            let id = &self.order[at - self.base];
            if let Some(record) = self.requests.get(id) {
                return Ok(Next::Memory(Some(record.request.clone())));
            }
            let request = self.responses.get(id).ok_or(Error::Cursor)?;
            let response = self.requests[request]
                .response
                .as_ref()
                .ok_or(Error::Cursor)?;
            return Ok(Next::Memory(Some(response.clone())));
        }
        let archive = self.archive.as_ref().ok_or(Error::ArchiveUnavailable)?;
        Ok(Next::Archive {
            path: archive.directory().to_path_buf(),
            before: self.base,
        })
    }
}

fn read_committed(
    path: &std::path::Path,
    session: &message::SessionId,
    at: usize,
    before: usize,
) -> Result<Arc<Message>, Error> {
    let archive = Lookup::committed(path, session, before)?;
    let entry = archive.at(at, before)?.ok_or(Error::Cursor)?;
    if &entry.message.session != session {
        return Err(Error::Session);
    }
    let original = match &entry.message.kind {
        WireKind::Request(_) => None,
        WireKind::Response { request, .. } => {
            let original = archive.get(request, at)?.ok_or(Error::Parent)?;
            Some(original.message.validate(None, LIMITS)?)
        }
    };
    Ok(Arc::new(entry.message.validate(original.as_ref(), LIMITS)?))
}

#[async_trait::async_trait]
impl ::execution::channel::Observation for Observation {
    fn cursor(&self) -> &Cursor {
        self.cursor()
    }
    async fn next(&mut self) -> Result<Observed, String> {
        self.next().await.map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use message::{Block, InputKind, Request, SessionId, Status, WireMessage};

    #[test]
    #[expect(
        clippy::await_holding_lock,
        reason = "prove that a cold read does not need the live message table"
    )]
    fn cancelled_cold_read_resumes_without_holding_the_live_message_table() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let messages = Messages::new(SessionId::new("cold-read".into()).unwrap());
            let wire = WireMessage {
                id: super::super::new_id(),
                session: messages.session(),
                by: None,
                at: 1,
                kind: WireKind::Request(Request::ToModel(InputKind::User)),
                body: vec![Block::Text("input".into())],
            };
            let id = messages.submit(wire).unwrap();
            let mut owner = messages.start_next().unwrap();
            owner.complete(Status::Done, Vec::new()).unwrap();
            messages.snapshot().save(dir.path()).unwrap();
            let mut observer = messages
                .observe(Some(Cursor {
                    session: messages.session(),
                    next: 0,
                }))
                .unwrap();
            let (release, waiting) = std::sync::mpsc::channel();
            let (began, started) = std::sync::mpsc::channel();
            let occupied = tokio::task::spawn_blocking(move || {
                began.send(()).unwrap();
                waiting.recv().unwrap();
            });
            started.recv().unwrap();
            let mut next = Box::pin(observer.next());
            assert!(futures::poll!(next.as_mut()).is_pending());
            drop(next);
            let table = messages.lock();
            release.send(()).unwrap();
            let observed = tokio::time::timeout(std::time::Duration::from_secs(2), observer.next())
                .await
                .expect("disk read must finish while the live table is locked")
                .unwrap();
            assert_eq!(observed.message.id(), id.message());
            assert_eq!(observed.cursor.next, 1);
            drop(table);
            occupied.await.unwrap();
        });
    }
}
