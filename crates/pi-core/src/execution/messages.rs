//! Session-owned semantic requests and their single terminal responses.

mod admission;
mod observe;
pub use admission::{Admission, AdmittedInput};
pub use observe::{Cursor, Observation, Observed};

pub use message::Failure;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use message::{
    Block, Kind, Limits, Message, MessageId, Request, RequestId, SessionId, Status, WireKind,
    WireMessage,
};

pub(crate) const LIMITS: Limits = Limits {
    bytes: message::DEFAULT_MESSAGE_BYTES,
    blocks: 4096,
    options: 256,
};
const CAPACITY: usize = 4096;
const COMPLETED_CACHE: usize = 256;
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const INPUT_QUEUE_LIMIT: usize = 256;
const INPUT_QUEUE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Messages(Arc<Mutex<Table>>);

struct Table {
    session: SessionId,
    changed: tokio::sync::watch::Sender<usize>,
    base: usize,
    archive: Option<pi_store::session::messages::Lookup>,
    completed: VecDeque<MessageId>,
    completed_bytes: usize,
    cancellation_save: bool,
    cancellation_epoch: u64,
    order: Vec<MessageId>,
    requests: HashMap<MessageId, Record>,
    triggers: HashMap<(String, String), MessageId>,
    responses: HashMap<MessageId, MessageId>,
    waiting: VecDeque<MessageId>,
    queued_bytes: usize,
    active: Option<MessageId>,
}

struct Record {
    request: Arc<Message>,
    request_position: usize,
    response_position: Option<usize>,
    parent: Option<MessageId>,
    response: Option<Arc<Message>>,
    abandoned: bool,
    pending_children: usize,
    native_call: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Storage(#[from] anyhow::Error),
    #[error(transparent)]
    Invalid(#[from] message::InvalidMessage),
    #[error("message belongs to another session")]
    Session,
    #[error("message identifier conflicts with an existing message")]
    Conflict,
    #[error("another input is already scheduled")]
    Busy,
    #[error("request table is full")]
    Full,
    #[error("request must belong to the active input")]
    Parent,
    #[error("request still has pending children")]
    Children,
    #[error("request already completed")]
    Completed,
    #[error("message archive is temporarily unavailable")]
    ArchiveUnavailable,
    #[error("causal message is not recorded in this session")]
    Cause,
    #[error("observation cursor is outside the session log")]
    Cursor,
    #[error("message observation closed")]
    ObservationClosed,
}

#[derive(serde::Serialize)]
pub struct Snapshot {
    #[serde(skip)]
    owner: std::sync::Weak<Mutex<Table>>,
    base: usize,
    format: u8,
    session: SessionId,
    entries: Vec<Saved>,
}

#[derive(serde::Serialize)]
struct Saved {
    #[serde(serialize_with = "serialize_message")]
    message: Arc<Message>,
    parent: Option<MessageId>,
    native_call: Option<String>,
}

fn serialize_message<S: serde::Serializer>(
    message: &Arc<Message>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(message.as_ref(), serializer)
}

impl Snapshot {
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        pi_store::session::messages::write_from(path, &self.session, self.base, &self.entries)?;
        let archive = pi_store::session::messages::Lookup::open(path)?
            .ok_or_else(|| anyhow::anyhow!("message archive was not committed"))?;
        if let Some(owner) = self.owner.upgrade() {
            let mut table = owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let end = self.base + self.entries.len();
            if end > table.base {
                let count = end - table.base;
                anyhow::ensure!(
                    count <= table.order.len(),
                    "message save exceeds pending entries"
                );
                table.order.drain(..count);
                table.base = end;
            }
            table.archive = Some(archive);
            table.evict();
        }
        Ok(())
    }
}

impl Messages {
    pub fn new(session: SessionId) -> Self {
        Self(Arc::new(Mutex::new(Table {
            session,
            changed: tokio::sync::watch::channel(0).0,
            base: 0,
            archive: None,
            completed: VecDeque::new(),
            completed_bytes: 0,
            cancellation_save: false,
            cancellation_epoch: 0,
            order: Vec::new(),
            requests: HashMap::new(),
            triggers: HashMap::new(),
            responses: HashMap::new(),
            waiting: VecDeque::new(),
            queued_bytes: 0,
            active: None,
        })))
    }

    pub fn snapshot(&self) -> Snapshot {
        let table = self.lock();
        let entries = table
            .order
            .iter()
            .map(|id| {
                if let Some(record) = table.requests.get(id) {
                    Saved {
                        message: record.request.clone(),
                        parent: record.parent.clone(),
                        native_call: record.native_call.clone(),
                    }
                } else {
                    let record = &table.requests[&table.responses[id]];
                    Saved {
                        message: record.response.as_ref().expect("recorded response").clone(),
                        parent: None,
                        native_call: None,
                    }
                }
            })
            .collect();
        Snapshot {
            owner: Arc::downgrade(&self.0),
            base: table.base,
            format: pi_store::session::messages::FORMAT,
            session: table.session.clone(),
            entries,
        }
    }

    pub fn load(session: SessionId, path: &std::path::Path) -> anyhow::Result<Self> {
        let Some(archive) = pi_store::session::messages::Lookup::open(path)? else {
            return Ok(Self::new(session));
        };
        anyhow::ensure!(
            archive.session() == &session,
            "message archive belongs to another session"
        );
        let count = archive.count();
        let messages = Self::new(session);
        let interrupted = {
            let mut table = messages.lock();
            table.archive = Some(archive);
            pi_store::session::messages::visit(path, |position, entry| {
                anyhow::ensure!(
                    table
                        .archive
                        .as_ref()
                        .expect("restoring archive")
                        .position(&entry.message.id, count)?
                        == Some(position),
                    "message archive identifier index mismatch"
                );
                table.restore_entry(entry)?;
                table.base = position + 1;
                table.order.clear();
                table.evict();
                Ok(())
            })?;
            table.close_interrupted()?
        };
        if interrupted {
            messages.snapshot().save(path)?;
        }
        Ok(messages)
    }

    /// Admit a new input or return the existing immutable request for an exact retry.
    #[cfg(test)]
    fn submit(&self, wire: WireMessage) -> Result<RequestId, Error> {
        let message = wire.validate(None, LIMITS)?;
        if !matches!(message.kind(), Kind::Request(Request::ToModel(_))) {
            return Err(Error::Parent);
        }
        let mut table = self.lock();
        let (id, _) = table.insert_input(message)?;
        Ok(id)
    }

    #[cfg(test)]
    fn start_next(&self) -> Option<Responder> {
        let mut table = self.lock();
        if table.active.is_some() {
            return None;
        }
        let id = table.waiting.pop_front()?;
        table.queued_bytes -= table.requests[&id].request.encoded_bytes();
        table.active = Some(id.clone());
        Some(Responder {
            messages: self.clone(),
            original: table.requests[&id].request.clone(),
            finished: false,
        })
    }

    /// Only the owner of the active input may create a model child request.
    pub fn child(
        &self,
        parent: &RequestId,
        wire: WireMessage,
        native_call: Option<String>,
    ) -> Result<Responder, Error> {
        if native_call.as_ref().is_some_and(|id| id.trim().is_empty()) {
            return Err(Error::Conflict);
        }
        let message = wire.validate(None, LIMITS)?;
        if !matches!(message.kind(), Kind::Request(Request::FromModel(_))) {
            return Err(Error::Parent);
        }
        let mut table = self.lock();
        if table.active.as_ref() != Some(parent.message())
            || table
                .requests
                .get(parent.message())
                .is_none_or(|r| r.abandoned || r.response.is_some())
        {
            return Err(Error::Parent);
        }
        let (id, fresh) = table.insert(message, Some(parent.message().clone()))?;
        if !fresh {
            return Err(Error::Conflict);
        }
        table
            .requests
            .get_mut(id.message())
            .expect("child request")
            .native_call = native_call;
        Ok(Responder {
            messages: self.clone(),
            original: table.requests[id.message()].request.clone(),
            finished: false,
        })
    }

    pub fn native_call(&self, request: &RequestId) -> Result<Option<String>, Error> {
        let mut table = self.lock();
        if !table.ensure_request(request.message())? {
            return Ok(None);
        }
        Ok(table.requests[request.message()].native_call.clone())
    }

    pub fn response(&self, request: &RequestId) -> Result<Option<Arc<Message>>, Error> {
        let mut table = self.lock();
        if !table.ensure_request(request.message())? {
            return Ok(None);
        }
        Ok(table.requests[request.message()].response.clone())
    }

    pub fn request(&self, id: &MessageId) -> Result<Option<Arc<Message>>, Error> {
        let mut table = self.lock();
        if !table.ensure_request(id)? {
            return Ok(None);
        }
        Ok(Some(table.requests[id].request.clone()))
    }

    /// Close the disk handle before the host moves the session directory.
    pub fn detach_archive(&self) -> Option<std::path::PathBuf> {
        self.lock()
            .archive
            .take()
            .map(|archive| archive.directory().to_path_buf())
    }

    pub fn attach_archive(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let archive = pi_store::session::messages::Lookup::open(path)?
            .ok_or_else(|| anyhow::anyhow!("message archive is missing"))?;
        let mut table = self.lock();
        anyhow::ensure!(
            archive.session() == &table.session && archive.count() >= table.base,
            "message archive does not match the current session"
        );
        table.archive = Some(archive);
        Ok(())
    }

    pub fn session(&self) -> SessionId {
        self.lock().session.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Table {
    fn restore_entry(&mut self, entry: pi_store::session::messages::Entry) -> Result<(), Error> {
        match &entry.message.kind {
            WireKind::Request(request) => {
                match request {
                    Request::ToModel(_)
                        if entry.parent.is_some() || entry.native_call.is_some() =>
                    {
                        return Err(Error::Parent);
                    }
                    Request::FromModel(_) => {
                        let parent = entry.parent.as_ref().ok_or(Error::Parent)?;
                        let record = self.requests.get(parent).ok_or(Error::Parent)?;
                        if record.response.is_some()
                            || !matches!(record.request.kind(), Kind::Request(Request::ToModel(_)))
                        {
                            return Err(Error::Parent);
                        }
                        if entry
                            .native_call
                            .as_ref()
                            .is_some_and(|id| id.trim().is_empty())
                        {
                            return Err(Error::Conflict);
                        }
                    }
                    Request::ToModel(_) => {}
                }
                let message = entry.message.validate(None, LIMITS)?;
                let (id, fresh) = self.insert(message, entry.parent)?;
                if !fresh {
                    return Err(Error::Conflict);
                }
                self.requests
                    .get_mut(id.message())
                    .expect("restored request")
                    .native_call = entry.native_call;
            }
            WireKind::Response { request, .. } => {
                if entry.parent.is_some() || entry.native_call.is_some() {
                    return Err(Error::Parent);
                }
                let id = request.clone();
                if self.requests.get(&id).is_none_or(|r| r.response.is_some()) {
                    return Err(Error::Conflict);
                }
                self.finish(&id, entry.message)?;
            }
        }
        Ok(())
    }

    fn close_interrupted(&mut self) -> Result<bool, Error> {
        // Interrupted work closes children first and never executes again.
        let mut pending: Vec<_> = self
            .requests
            .iter()
            .filter(|(_, r)| r.response.is_none())
            .map(|(id, r)| (r.parent.is_none(), id.clone()))
            .collect();
        pending.sort_by_key(|(root, _)| *root);
        let interrupted = !pending.is_empty();
        for (_, request) in pending {
            let wire = WireMessage {
                id: new_id(),
                session: self.session.clone(),
                by: Some(request.clone()),
                at: agent::session::now(),
                kind: WireKind::Response {
                    request: request.clone(),
                    status: Status::Stopped,
                },
                body: Vec::new(),
            };
            self.finish(&request, wire)?;
        }
        Ok(interrupted)
    }

    fn contains(&self, id: &MessageId) -> Result<bool, Error> {
        if self.requests.contains_key(id) || self.responses.contains_key(id) {
            return Ok(true);
        }
        Ok(match &self.archive {
            Some(archive) => archive.position(id, self.base)?.is_some(),
            None if self.base > 0 => return Err(Error::ArchiveUnavailable),
            None => false,
        })
    }

    fn ensure_request(&mut self, id: &MessageId) -> Result<bool, Error> {
        if self.requests.contains_key(id) {
            return Ok(true);
        }
        let Some(archive) = &self.archive else {
            return if self.base > 0 {
                Err(Error::ArchiveUnavailable)
            } else {
                Ok(false)
            };
        };
        let Some(entry) = archive.get(id, self.base)? else {
            return Ok(false);
        };
        if !matches!(&entry.message.kind, WireKind::Request(_)) {
            return Err(Error::Conflict);
        }
        let request = Arc::new(entry.message.validate(None, LIMITS)?);
        if !matches!(request.kind(), Kind::Request(_)) {
            return Err(Error::Conflict);
        }
        let request_position = archive.position(id, self.base)?.ok_or(Error::Parent)?;
        let response = archive.response(id, self.base)?;
        let (response, response_position) = match response {
            Some(entry) => {
                let response_position = archive
                    .position(&entry.message.id, self.base)?
                    .ok_or(Error::Parent)?;
                let message = Arc::new(entry.message.validate(Some(&request), LIMITS)?);
                self.responses.insert(message.id().clone(), id.clone());
                (Some(message), Some(response_position))
            }
            None => (None, None),
        };
        if let Some(response) = &response {
            self.completed_bytes += request.encoded_bytes() + response.encoded_bytes();
            self.completed.push_back(id.clone());
        }
        if let Some((source, event)) = request.automatic_trigger() {
            self.triggers
                .insert((source.into(), event.into()), id.clone());
        }
        self.requests.insert(
            id.clone(),
            Record {
                request,
                request_position,
                response_position,
                parent: entry.parent,
                response,
                native_call: entry.native_call,
                abandoned: false,
                pending_children: 0,
            },
        );
        self.evict();
        Ok(true)
    }

    fn evict(&mut self) {
        while self.completed.len() > COMPLETED_CACHE || self.completed_bytes > CACHE_BYTES {
            let Some(id) = self.completed.front() else {
                break;
            };
            let record = &self.requests[id];
            if record.request_position >= self.base
                || record.response_position.is_none_or(|at| at >= self.base)
            {
                break;
            }
            let id = self.completed.pop_front().expect("completed request");
            let record = self.requests.remove(&id).expect("cached completed request");
            if let Some((source, event)) = record.request.automatic_trigger() {
                self.triggers.remove(&(source.into(), event.into()));
            }
            if let Some(response) = record.response {
                self.completed_bytes -= record.request.encoded_bytes() + response.encoded_bytes();
                self.responses.remove(response.id());
            }
        }
    }

    fn known_request(
        &mut self,
        message: &Message,
        parent: &Option<MessageId>,
    ) -> Result<Option<RequestId>, Error> {
        if message.session() != &self.session {
            return Err(Error::Session);
        }
        if self.responses.contains_key(message.id()) {
            return Err(Error::Conflict);
        }
        if self.ensure_request(message.id())? {
            let record = &self.requests[message.id()];
            if record.request.as_ref() != message || record.parent.as_ref() != parent.as_ref() {
                return Err(Error::Conflict);
            }
            return Ok(Some(record.request.request_id().expect("stored request")));
        }
        if let Some((source, event)) = message.automatic_trigger() {
            let key = (source.to_string(), event.to_string());
            let prior = match self.triggers.get(&key) {
                Some(id) => Some(id.clone()),
                None => match &self.archive {
                    Some(archive) => archive.automatic_request(source, event, self.base)?,
                    None if self.base > 0 => return Err(Error::ArchiveUnavailable),
                    None => None,
                },
            };
            if let Some(prior) = prior {
                if !self.ensure_request(&prior)? {
                    return Err(Error::Conflict);
                }
                let original = &self.requests[&prior].request;
                if original.kind() != message.kind()
                    || original.body() != message.body()
                    || original.by() != message.by()
                {
                    return Err(Error::Conflict);
                }
                return Ok(Some(original.request_id().expect("automatic input")));
            }
        }
        Ok(None)
    }

    fn insert_input(&mut self, message: Message) -> Result<(RequestId, bool), Error> {
        if self.waiting.len() >= INPUT_QUEUE_LIMIT
            || self
                .queued_bytes
                .checked_add(message.encoded_bytes())
                .is_none_or(|bytes| bytes > INPUT_QUEUE_BYTES)
        {
            return self
                .known_request(&message, &None)?
                .map(|id| (id, false))
                .ok_or(Error::Full);
        }
        let bytes = message.encoded_bytes();
        let (id, fresh) = self.insert(message, None)?;
        if fresh {
            self.waiting.push_back(id.message().clone());
            self.queued_bytes += bytes;
        }
        Ok((id, fresh))
    }

    fn insert(
        &mut self,
        message: Message,
        parent: Option<MessageId>,
    ) -> Result<(RequestId, bool), Error> {
        if let Some(id) = self.known_request(&message, &parent)? {
            return Ok((id, false));
        }
        if self.requests.len() >= CAPACITY {
            return Err(Error::Full);
        }
        if let Some(id) = message.by()
            && !self.contains(id)?
        {
            return Err(Error::Cause);
        }
        let id = message.request_id().ok_or(Error::Parent)?;
        let request_position = self.base + self.order.len();
        if let Some((source, event)) = message.automatic_trigger() {
            self.triggers
                .insert((source.into(), event.into()), id.message().clone());
        }
        self.order.push(message.id().clone());
        self.requests.insert(
            message.id().clone(),
            Record {
                request: Arc::new(message),
                request_position,
                response_position: None,
                parent: parent.clone(),
                response: None,
                abandoned: false,
                pending_children: 0,
                native_call: None,
            },
        );
        if let Some(parent) = parent {
            self.requests
                .get_mut(&parent)
                .ok_or(Error::Parent)?
                .pending_children += 1;
        }
        self.publish();
        Ok((id, true))
    }

    fn abandon(&mut self, request: &MessageId) {
        if let Some(record) = self.requests.get_mut(request) {
            if record.response.is_some() {
                return;
            }
            record.abandoned = true;
        } else {
            return;
        }
        if self.requests[request].pending_children > 0 {
            return;
        }
        let wire = WireMessage {
            id: new_id(),
            session: self.session.clone(),
            by: Some(request.clone()),
            at: agent::session::now(),
            kind: WireKind::Response {
                request: request.clone(),
                status: Status::Failed(Failure {
                    code: "OWNER_DROPPED".into(),
                    message: "request owner exited before returning a result".into(),
                }),
            },
            body: Vec::new(),
        };
        let _ = self.finish(request, wire);
    }

    fn finish(&mut self, request: &MessageId, wire: WireMessage) -> Result<Arc<Message>, Error> {
        if !self.ensure_request(request)? {
            return Err(Error::Parent);
        }
        let record = &self.requests[request];
        let response = wire.validate(Some(&record.request), LIMITS)?;
        if let Some(existing) = &record.response {
            return if **existing == response {
                Ok(existing.clone())
            } else {
                Err(Error::Completed)
            };
        }
        if self.contains(response.id())? {
            return Err(Error::Conflict);
        }
        if let Some(id) = response.by()
            && !self.contains(id)?
        {
            return Err(Error::Cause);
        }
        if self.requests[request].pending_children > 0 {
            return Err(Error::Children);
        }
        let response = Arc::new(response);
        self.requests
            .get_mut(request)
            .expect("pending request")
            .response_position = Some(self.base + self.order.len());
        self.completed.push_back(request.clone());
        self.completed_bytes +=
            self.requests[request].request.encoded_bytes() + response.encoded_bytes();
        self.order.push(response.id().clone());
        self.responses
            .insert(response.id().clone(), request.clone());
        self.requests
            .get_mut(request)
            .expect("validated request")
            .response = Some(response.clone());
        if self.active.as_ref() == Some(request) {
            self.active = None;
        }
        self.publish();
        let parent = self.requests[request].parent.clone();
        if let Some(parent) = parent {
            let record = self.requests.get_mut(&parent).expect("stored parent");
            record.pending_children -= 1;
            if record.abandoned {
                self.abandon(&parent);
            }
        }
        self.evict();
        Ok(response)
    }
}

/// A scoped result port. Invalid candidates do not consume it; successful completion does.
/// Dropping an unfinished port records a terminal failure without blocking on cleanup.
pub struct Responder {
    messages: Messages,
    original: Arc<Message>,
    finished: bool,
}

impl Responder {
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(crate) fn response(&self) -> Result<Option<Arc<Message>>, Error> {
        self.messages
            .response(&self.original.request_id().expect("response owner"))
    }

    pub fn messages(&self) -> Messages {
        self.messages.clone()
    }
    pub fn request(&self) -> Arc<Message> {
        self.original.clone()
    }

    pub fn finish(&mut self, wire: WireMessage) -> Result<Arc<Message>, Error> {
        let response = self.messages.lock().finish(self.original.id(), wire)?;
        self.finished = true;
        Ok(response)
    }

    pub fn complete(&mut self, status: Status, body: Vec<Block>) -> Result<Arc<Message>, Error> {
        self.finish(WireMessage {
            id: new_id(),
            session: self.original.session().clone(),
            by: Some(self.original.id().clone()),
            at: agent::session::now(),
            kind: WireKind::Response {
                request: self.original.id().clone(),
                status,
            },
            body,
        })
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.messages.lock().abandon(self.original.id());
    }
}

pub fn new_id() -> MessageId {
    MessageId::new(pi_store::session::new_id()).expect("generated nonempty identifier")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(messages: &Messages) -> WireMessage {
        WireMessage {
            id: new_id(),
            session: messages.session(),
            by: None,
            at: 1,
            kind: WireKind::Request(Request::ToModel(message::InputKind::User)),
            body: vec![Block::Text("work".into())],
        }
    }

    #[tokio::test]
    async fn observation_resumes_after_cancellation_and_keeps_full_responses() {
        let messages = Messages::new(SessionId::new("observe".into()).unwrap());
        let mut observation = messages.observe(None).unwrap();
        let start = messages.cursor();
        let mut waiting = Box::pin(observation.next());
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(waiting);
        let wire = input(&messages);
        let Admission::Accepted(input) = messages.admit_wire(wire.clone()).unwrap() else {
            panic!()
        };
        let first = observation.next().await.unwrap();
        assert_eq!(first.message.id(), &wire.id);
        assert_eq!(first.cursor.next, start.next + 1);
        // Dropping a subscription is independent of the accepted execution.
        let cursor = observation.cursor().clone();
        drop(observation);
        let (_, mut owner) = input.start().unwrap();
        let response = owner
            .complete(Status::Done, vec![Block::Text("complete answer".into())])
            .unwrap();
        let mut resumed = messages.observe(Some(cursor)).unwrap();
        let next = resumed.next().await.unwrap();
        assert_eq!(next.message, response);
        assert_eq!(next.cursor.next, start.next + 2);
        assert!(matches!(
            messages.admit_wire(wire).unwrap(),
            Admission::Existing {
                response: Some(_),
                ..
            }
        ));
        let mut waiting = Box::pin(resumed.next());
        assert!(
            futures::poll!(waiting.as_mut()).is_pending(),
            "a retry does not publish twice"
        );
    }

    #[tokio::test]
    async fn slow_observers_resume_evicted_messages_from_disk_without_pinning_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("observe-archive".into()).unwrap();
        let messages = Messages::new(session.clone());
        let start = messages.cursor();
        let mut slow = messages.observe(Some(start.clone())).unwrap();
        let mut ids = Vec::new();
        for _ in 0..COMPLETED_CACHE + 2 {
            let id = messages.submit(input(&messages)).unwrap();
            let mut owner = messages.start_next().unwrap();
            ids.push(id.message().clone());
            ids.push(owner.complete(Status::Done, vec![]).unwrap().id().clone());
        }
        messages.snapshot().save(dir.path()).unwrap();
        assert!(!messages.lock().requests.contains_key(&ids[0]));
        assert!(messages.lock().requests.len() <= COMPLETED_CACHE);
        assert_eq!(slow.next().await.unwrap().message.id(), &ids[0]);
        let cursor = slow.cursor().clone();
        drop(slow);
        let restored = Messages::load(session, dir.path()).unwrap();
        let mut resumed = restored.observe(Some(cursor)).unwrap();
        for id in &ids[1..] {
            assert_eq!(resumed.next().await.unwrap().message.id(), id);
        }
        assert_eq!(resumed.cursor(), &restored.cursor());
        assert!(matches!(
            restored.observe(Some(Cursor {
                session: SessionId::new("other".into()).unwrap(),
                next: 0
            })),
            Err(Error::Session)
        ));
        assert!(matches!(
            restored.observe(Some(Cursor {
                session: restored.session(),
                next: ids.len() + 1
            })),
            Err(Error::Cursor)
        ));
        // Inspection does not submit or re-open any work.
        assert!(restored.start_next().is_none());
    }

    #[test]
    fn duplicate_input_is_not_executed_again_and_conflicting_payload_is_rejected() {
        let messages = Messages::new(SessionId::new("s".into()).unwrap());
        let wire = input(&messages);
        let id = messages.submit(wire.clone()).unwrap();
        assert_eq!(messages.submit(wire.clone()).unwrap(), id);
        let mut changed = wire;
        changed.body = vec![Block::Text("different".into())];
        assert!(matches!(messages.submit(changed), Err(Error::Conflict)));
        let mut owner = messages.start_next().unwrap();
        assert!(messages.start_next().is_none());
        owner.complete(Status::Done, vec![]).unwrap();
        assert!(messages.start_next().is_none());
        assert!(messages.response(&id).unwrap().is_some());
    }

    #[test]
    fn invalid_response_does_not_claim_the_terminal_slot() {
        let messages = Messages::new(SessionId::new("s".into()).unwrap());
        let id = messages.submit(input(&messages)).unwrap();
        let mut owner = messages.start_next().unwrap();
        let wrong = WireMessage {
            id: new_id(),
            session: SessionId::new("other".into()).unwrap(),
            by: None,
            at: 1,
            kind: WireKind::Response {
                request: id.message().clone(),
                status: Status::Done,
            },
            body: vec![],
        };
        assert!(owner.finish(wrong).is_err());
        assert!(messages.response(&id).unwrap().is_none());
        owner.complete(Status::Done, vec![]).unwrap();
        assert!(matches!(
            owner.complete(Status::Stopped, vec![]),
            Err(Error::Completed)
        ));
    }

    #[test]
    fn abandoned_parent_waits_for_its_children_and_cannot_gain_more() {
        let messages = Messages::new(SessionId::new("s".into()).unwrap());
        let parent = messages.submit(input(&messages)).unwrap();
        let mut owner = messages.start_next().unwrap();
        let call = || WireMessage {
            id: new_id(),
            session: messages.session(),
            by: Some(parent.message().clone()),
            at: 1,
            kind: WireKind::Request(Request::FromModel(message::ModelRequest::Tool {
                name: "read".into(),
            })),
            body: vec![Block::Data(serde_json::json!({}))],
        };
        let mut child = messages.child(&parent, call(), None).unwrap();
        assert!(matches!(
            owner.complete(Status::Done, vec![]),
            Err(Error::Children)
        ));
        drop(owner);
        assert!(messages.response(&parent).unwrap().is_none());
        assert!(matches!(
            messages.child(&parent, call(), None),
            Err(Error::Parent)
        ));
        child.complete(Status::Done, vec![]).unwrap();
        assert!(messages.response(&parent).unwrap().is_some());
        assert!(messages.start_next().is_none());
    }
    #[test]
    fn completed_messages_keep_their_ids_and_deduplication_after_reload() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("persisted".into()).unwrap();
        let messages = Messages::new(session.clone());
        let wire = input(&messages);
        let id = messages.submit(wire.clone()).unwrap();
        let mut owner = messages.start_next().unwrap();
        let response = owner
            .complete(Status::Done, vec![Block::Text("result".into())])
            .unwrap();
        messages.snapshot().save(dir.path()).unwrap();
        let restored = Messages::load(session, dir.path()).unwrap();
        assert_eq!(restored.response(&id).unwrap().unwrap(), response);
        assert_eq!(restored.submit(wire).unwrap(), id);
        assert!(restored.start_next().is_none());
        restored.snapshot().save(dir.path()).unwrap();
        assert_eq!(
            pi_store::session::messages::read(dir.path())
                .unwrap()
                .unwrap()
                .entries
                .len(),
            2
        );
    }

    #[test]
    fn interrupted_children_are_closed_before_the_parent_without_reexecution() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("interrupted".into()).unwrap();
        let messages = Messages::new(session.clone());
        let parent = messages.submit(input(&messages)).unwrap();
        let _owner = messages.start_next().unwrap();
        let child = messages
            .child(
                &parent,
                WireMessage {
                    id: new_id(),
                    session: session.clone(),
                    by: Some(parent.message().clone()),
                    at: 1,
                    kind: WireKind::Request(Request::FromModel(message::ModelRequest::Tool {
                        name: "bash".into(),
                    })),
                    body: vec![Block::Data(serde_json::json!({}))],
                },
                Some("native-1".into()),
            )
            .unwrap();
        let child_id = child.request().request_id().unwrap();
        messages.snapshot().save(dir.path()).unwrap();
        // This segment was never committed to the index and must not be replayed.
        std::fs::write(dir.path().join("999-1000.json"), "incomplete").unwrap();
        let restored = Messages::load(session.clone(), dir.path()).unwrap();
        assert!(restored.start_next().is_none());
        for id in [&parent, &child_id] {
            assert!(matches!(
                restored.response(id).unwrap().unwrap().kind(),
                Kind::Response {
                    status: Status::Stopped,
                    ..
                }
            ));
        }
        assert_eq!(
            restored.native_call(&child_id).unwrap().as_deref(),
            Some("native-1")
        );
        let repeated = Messages::load(session, dir.path()).unwrap();
        assert_eq!(
            restored.response(&parent).unwrap(),
            repeated.response(&parent).unwrap()
        );
        assert_eq!(
            restored.response(&child_id).unwrap(),
            repeated.response(&child_id).unwrap()
        );
    }
    #[test]
    fn long_sessions_evict_committed_records_and_still_deduplicate_cold_requests() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("long-session".into()).unwrap();
        let messages = Messages::new(session.clone());
        let first = input(&messages);
        let first_id = messages.submit(first.clone()).unwrap();
        messages
            .start_next()
            .unwrap()
            .complete(Status::Done, vec![])
            .unwrap();
        for at in 0..CAPACITY + 32 {
            messages.submit(input(&messages)).unwrap();
            messages
                .start_next()
                .unwrap()
                .complete(Status::Done, vec![])
                .unwrap();
            if at % 64 == 0 {
                messages.snapshot().save(dir.path()).unwrap();
            }
        }
        messages.snapshot().save(dir.path()).unwrap();
        assert!(messages.lock().requests.len() <= COMPLETED_CACHE);
        assert!(messages.lock().completed_bytes <= CACHE_BYTES);
        assert!(messages.lock().order.is_empty());
        assert!(!messages.lock().requests.contains_key(first_id.message()));
        assert_eq!(messages.submit(first.clone()).unwrap(), first_id);
        assert!(messages.response(&first_id).unwrap().is_some());
        assert!(messages.start_next().is_none());
        let restored = Messages::load(session, dir.path()).unwrap();
        assert!(restored.lock().requests.len() <= COMPLETED_CACHE);
        assert_eq!(restored.submit(first).unwrap(), first_id);
        assert!(restored.start_next().is_none());
        restored.submit(input(&restored)).unwrap();
        restored
            .start_next()
            .unwrap()
            .complete(Status::Done, vec![])
            .unwrap();
        restored.snapshot().save(dir.path()).unwrap();
    }

    #[test]
    fn save_failure_retains_pending_records_and_overlapping_saves_append_once() {
        let dir = tempfile::tempdir().unwrap();
        let messages = Messages::new(SessionId::new("save-order".into()).unwrap());
        messages.submit(input(&messages)).unwrap();
        messages
            .start_next()
            .unwrap()
            .complete(Status::Done, vec![])
            .unwrap();
        let first = messages.snapshot();
        messages.submit(input(&messages)).unwrap();
        messages
            .start_next()
            .unwrap()
            .complete(Status::Done, vec![])
            .unwrap();
        let second = messages.snapshot();
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, "occupied").unwrap();
        assert!(first.save(&blocked).is_err());
        assert_eq!(messages.lock().base, 0);
        assert_eq!(messages.lock().order.len(), 4);
        let archive = dir.path().join("archive");
        first.save(&archive).unwrap();
        assert_eq!(messages.lock().order.len(), 2);
        second.save(&archive).unwrap();
        first.save(&archive).unwrap();
        assert!(messages.lock().order.is_empty());
        assert_eq!(
            pi_store::session::messages::read(&archive)
                .unwrap()
                .unwrap()
                .entries
                .len(),
            4
        );
    }
    #[test]
    fn moving_the_archive_reopens_cold_queries_at_the_new_directory() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        let messages = Messages::new(SessionId::new("moved".into()).unwrap());
        let first = input(&messages);
        let id = messages.submit(first.clone()).unwrap();
        messages
            .start_next()
            .unwrap()
            .complete(Status::Done, vec![])
            .unwrap();
        for _ in 0..COMPLETED_CACHE {
            messages.submit(input(&messages)).unwrap();
            messages
                .start_next()
                .unwrap()
                .complete(Status::Done, vec![])
                .unwrap();
        }
        messages.snapshot().save(&old).unwrap();
        assert!(!messages.lock().requests.contains_key(id.message()));
        assert_eq!(messages.detach_archive().unwrap(), old);
        std::fs::rename(&old, &new).unwrap();
        messages.attach_archive(&new).unwrap();
        assert_eq!(messages.submit(first).unwrap(), id);
        assert!(messages.response(&id).unwrap().is_some());
        messages.submit(input(&messages)).unwrap();
        messages
            .start_next()
            .unwrap()
            .complete(Status::Done, vec![])
            .unwrap();
        messages.snapshot().save(&new).unwrap();
    }
    #[test]
    fn queued_inputs_keep_their_identity_and_drop_closes_only_the_owned_request() {
        let messages = Messages::new(SessionId::new("admission".into()).unwrap());
        let prompt = || agent::session::Prompt {
            text: "work".into(),
            images: vec![],
            shown: None,
            relayed: None,
        };
        let first = messages
            .admit(crate::driver::Origin::Typed, prompt())
            .unwrap();
        let first_id = first.message().request_id().unwrap();
        let second = messages
            .admit(crate::driver::Origin::Input(1), prompt())
            .unwrap();
        let second_id = second.message().request_id().unwrap();
        assert!(first.is_next());
        assert!(!second.is_next());
        drop(first);
        assert!(matches!(
            messages.response(&first_id).unwrap().unwrap().kind(),
            Kind::Response {
                status: Status::Stopped,
                ..
            }
        ));
        assert!(second.is_next());
        let (_, mut responder) = second.start().unwrap();
        assert_eq!(responder.request().request_id().unwrap(), second_id);
        responder.complete(Status::Done, vec![]).unwrap();
        assert_eq!(messages.lock().requests.len(), 2);
    }

    #[test]
    fn explicit_input_retries_do_not_gain_another_execution_ticket_or_parse_commands() {
        let messages = Messages::new(SessionId::new("explicit-input".into()).unwrap());
        let mut wire = input(&messages);
        wire.body = vec![Block::Text("/new".into())];
        let Admission::Accepted(admitted) = messages.admit_wire(wire.clone()).unwrap() else {
            panic!("new input")
        };
        let id = admitted.message().request_id().unwrap();
        assert_eq!(admitted.prompt().text, "/new");
        assert!(matches!(
            messages.admit_wire(wire.clone()).unwrap(),
            Admission::Existing { response: None, .. }
        ));
        let (_, mut responder) = admitted.start().unwrap();
        let response = responder.complete(Status::Done, vec![]).unwrap();
        let Admission::Existing {
            request,
            response: existing,
        } = messages.admit_wire(wire.clone()).unwrap()
        else {
            panic!("completed input")
        };
        assert_eq!(request, id);
        assert_eq!(existing.unwrap(), response);
        wire.session = SessionId::new("another-session".into()).unwrap();
        assert!(matches!(messages.admit_wire(wire), Err(Error::Session)));
        assert!(messages.start_next().is_none());
    }
    #[test]
    fn automatic_events_deduplicate_across_message_ids_after_cache_eviction_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("events".into()).unwrap();
        let messages = Messages::new(session.clone());
        let mut wire = input(&messages);
        wire.kind = WireKind::Request(Request::ToModel(message::InputKind::Automatic));
        wire.body = vec![Block::Data(
            serde_json::json!({"source":"task-1", "reason":"completed", "event":"run-1"}),
        )];
        let Admission::Accepted(admitted) = messages.admit_wire(wire.clone()).unwrap() else {
            panic!("new event")
        };
        let id = admitted.message().request_id().unwrap();
        let (_, mut port) = admitted.start().unwrap();
        let response = port.complete(Status::Done, vec![]).unwrap();
        for _ in 0..COMPLETED_CACHE {
            messages.submit(input(&messages)).unwrap();
            messages
                .start_next()
                .unwrap()
                .complete(Status::Done, vec![])
                .unwrap();
        }
        messages.snapshot().save(dir.path()).unwrap();
        let restored = Messages::load(session, dir.path()).unwrap();
        wire.id = new_id();
        wire.at += 1;
        let Admission::Existing {
            request,
            response: previous,
        } = restored.admit_wire(wire.clone()).unwrap()
        else {
            panic!("existing event")
        };
        assert_eq!(request, id);
        assert_eq!(previous.unwrap(), response);
        assert!(restored.start_next().is_none());
        wire.id = new_id();
        wire.body.push(Block::Text("different result".into()));
        assert!(matches!(restored.admit_wire(wire), Err(Error::Conflict)));
    }
    #[test]
    fn a_full_input_queue_allows_retries_and_current_tool_completion() {
        let messages = Messages::new(SessionId::new("queue-budget".into()).unwrap());
        let first_wire = input(&messages);
        let Admission::Accepted(first) = messages.admit_wire(first_wire).unwrap() else {
            panic!("first input")
        };
        let parent = first.message().request_id().unwrap();
        let (_, mut running) = first.start().unwrap();
        let repeated = input(&messages);
        let Admission::Accepted(waiting) = messages.admit_wire(repeated.clone()).unwrap() else {
            panic!("pending input")
        };
        let mut queue = vec![waiting];
        for _ in 1..INPUT_QUEUE_LIMIT {
            let Admission::Accepted(waiting) = messages.admit_wire(input(&messages)).unwrap()
            else {
                panic!("pending input")
            };
            queue.push(waiting);
        }
        assert!(matches!(
            messages.admit_wire(input(&messages)),
            Err(Error::Full)
        ));
        assert!(matches!(
            messages.admit_wire(repeated).unwrap(),
            Admission::Existing { response: None, .. }
        ));
        let mut child = messages
            .child(
                &parent,
                WireMessage {
                    id: new_id(),
                    session: messages.session(),
                    by: Some(parent.message().clone()),
                    at: 1,
                    kind: WireKind::Request(Request::FromModel(message::ModelRequest::Tool {
                        name: "read".into(),
                    })),
                    body: vec![Block::Data(serde_json::json!({}))],
                },
                None,
            )
            .unwrap();
        child.complete(Status::Done, vec![]).unwrap();
        running.complete(Status::Done, vec![]).unwrap();
        drop(queue.pop());
        let Admission::Accepted(replacement) = messages.admit_wire(input(&messages)).unwrap()
        else {
            panic!("replacement input")
        };
        drop(replacement);
        drop(queue);
        assert!(messages.lock().waiting.is_empty());
        assert_eq!(messages.lock().queued_bytes, 0);
    }
}
