//! Services: tools declared with `service = true`, run by pi itself rather
//! than offered to the model. One process serves every session on the
//! machine: each session puts a unix socket in `$PI_HOME`'s runtime place,
//! and the service, one per machine under a lock it holds, connects to every
//! socket and speaks the same fd 3 protocol. What a service sends becomes the
//! session's input; what it reports as status is drawn on the status line.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use message::{Block, InputKind, Request, WireKind, WireMessage};
use pi_tools::scripts::wire::{self, Id, Incoming, Stream as Listening};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio::sync::mpsc;

use crate::execution::ingress::{Ingress, SubmitError};

/// Where the lock fd is handed to a service, named to it by this variable.
const LOCK_FD: i32 = 3;
/// How often a pi looks again for services no lock holds.
const HEAL: Duration = Duration::from_secs(60);
// Detach a peer that stops draining its frames.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SERVICE_CAPABILITIES: &[&str] = &[
    "initialize",
    "info",
    "send",
    "event",
    "listen:session",
    "status",
];

/// The service side of pi: which services run, and what the sessions'
/// services last reported. Dormant until armed, so a surface that never arms it
/// — a test, a one-shot run — starts none.
#[derive(Default)]
pub struct Services {
    places: OnceLock<Places>,
    /// Session id → the socket bound for it, unlinked when dropped.
    sockets: Mutex<HashMap<String, Socket>>,
    /// Each session's status slots: a connection's latest text, kept only
    /// while it is up. A row draws its session's, never another lane's.
    slots: Mutex<HashMap<String, Slots>>,
    /// Where the next connection's slot number comes from.
    next_slot: Arc<AtomicU64>,
    /// Services this pi started, reaped here so they leave no zombies.
    children: Mutex<Vec<Child>>,
}

struct Places {
    tools: PathBuf,
    /// Reads `tools` across heal rounds, so its stamps cache holds.
    discover: pi_tools::scripts::Dir,
    locks: PathBuf,
    logs: PathBuf,
    sessions: PathBuf,
}

/// A session's socket, unlinked and stopped when pi leaves it.
struct Socket {
    path: PathBuf,
    accept: tokio::task::AbortHandle,
    conns: Arc<Mutex<Option<Vec<tokio::task::AbortHandle>>>>,
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.accept.abort();
        if let Some(list) = self.conns.lock().expect("lock poisoned").take() {
            for handle in list {
                handle.abort();
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One session's status slots: each live connection's latest text, by
/// slot number.
type Slots = Arc<Mutex<BTreeMap<u64, String>>>;

/// One live connection's place among the statuses, given up when it ends.
struct Slot(Slots, u64);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.lock().expect("lock poisoned").remove(&self.1);
    }
}

impl Services {
    /// The status texts this session's service connections last reported.
    pub fn statuses(&self, session: &str) -> Vec<String> {
        self.slots
            .lock()
            .expect("lock poisoned")
            .get(session)
            .map(|slots| {
                slots
                    .lock()
                    .expect("lock poisoned")
                    .values()
                    .filter(|text| !text.is_empty())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Start every declared service no lock holds, and keep looking: a
    /// crashed service is restarted by whichever pi looks next.
    pub fn arm(self: &Arc<Self>) {
        let Some(home) = pi_store::dir() else {
            return;
        };
        let tools = home.join("tools");
        let places = Places {
            sessions: std::env::var_os("XDG_RUNTIME_DIR")
                .map(|dir| PathBuf::from(dir).join("pi").join("sessions"))
                .unwrap_or_else(|| home.join("run").join("sessions")),
            discover: pi_tools::scripts::Dir::new(tools.clone()),
            tools,
            locks: home.join("services"),
            logs: home.join("logs").join("services"),
        };
        let made = [&places.tools, &places.locks, &places.logs, &places.sessions]
            .into_iter()
            .try_for_each(std::fs::create_dir_all);
        if let Err(why) = made {
            tracing::warn!(target: "pi::services", %why, "services stay off: no place for them");
            return;
        }
        if self.places.set(places).is_err() {
            return;
        }
        tracing::info!(target: "pi::services", "armed");
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(me) = weak.upgrade() {
                me.heal();
                // Only this task's own hold: the surface that armed it is gone.
                if Arc::strong_count(&me) == 1 {
                    return;
                }
                tokio::time::sleep(HEAL).await;
            }
        });
    }

    /// Serve `session` from `ingress`: a socket the session's services
    /// connect to, whose frames become this session's inputs.
    pub(crate) fn bind(&self, session: &str, ingress: Arc<Ingress>) {
        let sessions = self.places.get().map(|places| places.sessions.clone());
        let Some(sessions) = sessions else {
            return;
        };
        let mut sockets = self.sockets.lock().expect("lock poisoned");
        sockets.remove(session);
        let path = sessions.join(format!("{session}.sock"));
        let _ = std::fs::remove_file(&path);
        let listener = match std::os::unix::net::UnixListener::bind(&path).and_then(|listener| {
            listener.set_nonblocking(true)?;
            UnixListener::from_std(listener)
        }) {
            Ok(listener) => listener,
            Err(why) => {
                let _ = std::fs::remove_file(&path);
                tracing::warn!(target: "pi::services", session, %why, "session socket not bound");
                return;
            }
        };
        tracing::info!(target: "pi::services", session, path = %path.display(), "session socket bound");
        let slots = self
            .slots
            .lock()
            .expect("lock poisoned")
            .entry(session.to_string())
            .or_default()
            .clone();
        let next_slot = self.next_slot.clone();
        let session_name = session.to_string();
        let conns = Arc::new(Mutex::new(Some(Vec::<tokio::task::AbortHandle>::new())));
        let accept_conns = conns.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let slot = next_slot.fetch_add(1, Ordering::Relaxed);
                let handle = tokio::spawn(serve(
                    stream,
                    session_name.clone(),
                    ingress.clone(),
                    slots.clone(),
                    slot,
                ))
                .abort_handle();
                let mut guard = accept_conns.lock().expect("lock poisoned");
                match guard.as_mut() {
                    Some(list) => {
                        list.retain(|h| !h.is_finished());
                        list.push(handle);
                    }
                    None => handle.abort(),
                }
            }
        })
        .abort_handle();
        sockets.insert(
            session.to_string(),
            Socket {
                path,
                accept,
                conns,
            },
        );
    }

    /// Stop serving `session` on its socket.
    pub(crate) fn unbind(&self, session: &str) {
        self.sockets.lock().expect("lock poisoned").remove(session);
    }

    /// One pass of the heal loop: reap what this pi started, and start what
    /// no lock holds.
    fn heal(&self) {
        let mut children = self.children.lock().expect("lock poisoned");
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        drop(children);
        let Some(places) = self.places.get() else {
            return;
        };
        for service in places.discover.services() {
            self.start_if_free(places, &service);
        }
    }

    /// Hold the lock for a service, or leave it: the service itself holds
    /// the lock fd for its lifetime, so `flock` saying no elsewhere is the
    /// whole liveness answer.
    fn start_if_free(&self, places: &Places, service: &pi_tools::scripts::Service) {
        let lock = places.locks.join(format!("{}.lock", service.name));
        let Ok(file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
        else {
            return;
        };
        // SAFETY: flock only, no allocation between call and check.
        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if held != 0 {
            let why = std::io::Error::last_os_error();
            if why.kind() != std::io::ErrorKind::WouldBlock {
                tracing::warn!(target: "pi::services", service = &service.name, %why, "lock not taken");
            }
            return;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(places.logs.join(format!("{}.log", service.name)))
            .ok();
        let mut cmd = service.command();
        cmd.env("PI_SERVICE_LOCK_FD", LOCK_FD.to_string())
            .env("PI_SESSIONS_DIR", &places.sessions)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log.map(Stdio::from).unwrap_or(Stdio::null()));
        let lock_fd = file.as_raw_fd();
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                // dup2 clears close-on-exec on the copy; a lock already at 3
                // has to have it cleared by hand.
                let ok = if lock_fd == LOCK_FD {
                    libc::fcntl(LOCK_FD, libc::F_SETFD, 0)
                } else {
                    libc::dup2(lock_fd, LOCK_FD)
                };
                if ok < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                tracing::info!(target: "pi::services", service = &service.name, pid = child.id(), "service started");
                self.children.lock().expect("lock poisoned").push(child);
            }
            Err(why) => {
                tracing::warn!(target: "pi::services", service = &service.name, %why, "service not started");
            }
        }
    }
}

/// Spawned `Listen` tasks for one connection, aborted when the connection ends.
struct Listeners(Vec<tokio::task::AbortHandle>);

impl Drop for Listeners {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.abort();
        }
    }
}

/// One connection from a service: every frame it speaks, answered.
async fn serve(
    stream: UnixStream,
    session: String,
    ingress: Arc<Ingress>,
    slots: Slots,
    slot: u64,
) {
    let _guard = Slot(slots.clone(), slot);
    let (read, mut write) = stream.into_split();
    let (to, mut from) = mpsc::channel::<Value>(16);
    let writer = tokio::spawn(async move {
        while let Some(frame) = from.recv().await {
            let Ok(bytes) = wire::encode(&frame) else {
                continue;
            };
            if !matches!(
                tokio::time::timeout(WRITE_TIMEOUT, write.write_all(&bytes)).await,
                Ok(Ok(()))
            ) {
                return;
            }
        }
    });
    let mut frames = wire::Frames::new(read);
    let mut conn = Conn {
        session,
        ingress,
        slots,
        slot,
        nonce: crate::execution::messages::new_id(),
        client: String::new(),
        to,
        listeners: Listeners(Vec::new()),
    };
    while let Ok(frame) = frames.next().await {
        // Whether the connection stays up: a frame pi will not serve ends it.
        if !conn.answer(frame).await {
            break;
        }
    }
    drop(conn);
    let _ = writer.await;
}

/// Per-connection state for serving frames from one connected service peer.
struct Conn {
    session: String,
    ingress: Arc<Ingress>,
    slots: Slots,
    slot: u64,
    nonce: message::MessageId,
    client: String,
    to: mpsc::Sender<Value>,
    listeners: Listeners,
}

impl Conn {
    async fn send(&self, frame: Value) -> bool {
        self.to.send(frame).await.is_ok()
    }

    /// Answer a frame pi will not do, by id when it has one.
    async fn refuse(&self, id: Option<&Id>, code: &str, why: &str) -> bool {
        tracing::warn!(target: "pi::services", %why, "frame refused");
        self.send(wire::error(id, code, why)).await
    }

    /// One line from a service: a request, a notice, or a reply with no question
    /// behind it. Whether the connection stays up.
    async fn answer(&mut self, frame: Value) -> bool {
        if frame.get("op").is_none() {
            let closed = match frame.get("id") {
                Some(id) => match Id::deserialize(id) {
                    Ok(id) => wire::failure(&id, "closed", "no question is waiting under that id"),
                    Err(_) => wire::unreadable("frame", "a reply needs a usable id"),
                },
                None => wire::unreadable("frame", "a reply needs an id"),
            };
            return self.send(closed).await;
        }
        let incoming = match wire::decode(&frame) {
            Ok(incoming) => incoming,
            Err(err_frame) => {
                tracing::warn!(target: "pi::services", frame = %err_frame, "frame rejected");
                return self.send(err_frame).await;
            }
        };
        match incoming {
            Incoming::Initialize {
                id,
                protocol,
                client: name,
            } => {
                let frame = wire::initialize(
                    &id,
                    protocol,
                    "service",
                    Some(&self.session),
                    &self.ingress.root,
                    SERVICE_CAPABILITIES,
                );
                if protocol == wire::PROTOCOL {
                    let name = name.trim();
                    if !name.is_empty() {
                        self.client = name.to_string();
                    }
                    tracing::info!(
                        target: "pi::services",
                        session = %self.session,
                        client = %self.client,
                        "service initialized"
                    );
                }
                self.send(frame).await
            }
            Incoming::Info { id } => {
                self.send(wire::info(
                    &id,
                    Some(&self.session),
                    &self.ingress.root,
                    self.ingress.is_busy(),
                ))
                .await
            }
            Incoming::Send {
                id,
                text,
                note,
                more,
            } => {
                if more {
                    return self
                        .refuse(
                            id.as_ref(),
                            "frame",
                            "a service may not hold the checkout open",
                        )
                        .await;
                }
                if !note.is_empty() {
                    return self
                        .refuse(id.as_ref(), "frame", "a service may not attach turn notes")
                        .await;
                }
                // The person's words, not the person's authority: a service relays
                // what was said, never a command.
                if text.starts_with('!') {
                    return self
                        .refuse(
                            id.as_ref(),
                            "denied",
                            "a line from a service may not run shell commands",
                        )
                        .await;
                }
                self.submit_wire(
                    id.as_ref(),
                    WireKind::Request(Request::ToModel(InputKind::User)),
                    vec![Block::Text(text)],
                )
                .await
            }
            Incoming::Event {
                id,
                text,
                source,
                reason,
                key,
            } => {
                let event = key
                    .filter(|k| !k.trim().is_empty())
                    .or_else(|| id.as_ref().map(ToString::to_string))
                    .unwrap_or_else(|| crate::execution::messages::new_id().as_str().to_string());
                self.submit_wire(
                    id.as_ref(),
                    WireKind::Request(Request::ToModel(InputKind::Automatic)),
                    vec![
                        Block::Text(text),
                        Block::Data(json!({
                            "source": source,
                            "reason": reason,
                            "event": event,
                        })),
                    ],
                )
                .await
            }
            Incoming::Reply { id, .. } => {
                self.refuse(id.as_ref(), "frame", "a service may not answer questions")
                    .await
            }
            Incoming::Ask { id, .. } => {
                self.refuse(
                    id.as_ref(),
                    "frame",
                    "a service may not ask; it may send and report",
                )
                .await
            }
            Incoming::Listen {
                id,
                to: Listening::Session,
                cursor,
            } => match self.ingress.messages.observe(cursor) {
                Ok(observation) => {
                    let cursor = observation.cursor().clone();
                    if !self.send(wire::reply(&id, json!({"cursor": cursor}))).await {
                        return false;
                    }
                    let to = self.to.clone();
                    let task = tokio::spawn(async move {
                        let mut observation = observation;
                        while let Ok(event) = observation.next().await {
                            let frame = wire::message(&event.cursor, event.message.as_ref());
                            if to.send(frame).await.is_err() {
                                return;
                            }
                        }
                    });
                    self.listeners.0.push(task.abort_handle());
                    true
                }
                Err(why) => {
                    self.send(wire::failure(&id, "internal", why.to_string()))
                        .await
                }
            },
            Incoming::Listen {
                id,
                to: Listening::Questions,
                cursor: _,
            } => {
                self.send(wire::failure(
                    &id,
                    "frame",
                    "a service may not listen to questions",
                ))
                .await
            }
            Incoming::Status { text } => {
                let mut guard = self.slots.lock().expect("lock poisoned");
                match text {
                    Some(text) if !text.trim().is_empty() => {
                        guard.insert(self.slot, text);
                    }
                    _ => {
                        guard.remove(&self.slot);
                    }
                }
                true
            }
            Incoming::Stop {} => true,
        }
    }

    async fn submit_wire(&self, id: Option<&Id>, kind: WireKind, body: Vec<Block>) -> bool {
        let msg_id = match request_message_id(id, &self.client, &self.nonce) {
            Ok(msg_id) => msg_id,
            Err(why) => return self.refuse(id, "frame", &why).await,
        };
        let session = match message::SessionId::new(self.session.clone()) {
            Ok(session) => session,
            Err(why) => {
                tracing::warn!(target: "pi::services", %why, "send refused");
                return self.refuse(id, "frame", &why.to_string()).await;
            }
        };
        let wire = WireMessage {
            id: msg_id,
            session,
            by: None,
            at: agent::session::now(),
            kind,
            body,
        };
        let ingress = self.ingress.clone();
        let submitted = tokio::task::spawn_blocking(move || ingress.submit(0, wire))
            .await
            .map_err(|e| SubmitError::Rejected(e.to_string()))
            .and_then(|res| res);
        match submitted {
            Ok(receipt) => match id {
                Some(id) => {
                    self.send(wire::accepted(id, receipt.request.message().as_str()))
                        .await
                }
                None => true,
            },
            Err(err) => {
                let code = match &err {
                    SubmitError::Closed => "session",
                    SubmitError::Rejected(_) => "frame",
                };
                self.refuse(id, code, &err.to_string()).await
            }
        }
    }
}

fn request_message_id(
    id: Option<&Id>,
    client: &str,
    nonce: &message::MessageId,
) -> Result<message::MessageId, String> {
    let Some(id) = id else {
        return Ok(crate::execution::messages::new_id());
    };
    let raw = if client.is_empty() {
        format!("conn:{}:{id}", nonce.as_str())
    } else {
        format!("svc:{client}:{id}")
    };
    message::MessageId::new(raw).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::messages::Messages;
    use crate::execution::queue::Queue;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn test_services(dir: &std::path::Path) -> Arc<Services> {
        let services = Arc::new(Services::default());
        let _ = services.places.set(Places {
            tools: dir.join("tools"),
            discover: pi_tools::scripts::Dir::new(dir.join("tools")),
            locks: dir.join("locks"),
            logs: dir.join("logs"),
            sessions: dir.to_path_buf(),
        });
        services
    }

    async fn exchange(
        write: &mut tokio::net::unix::OwnedWriteHalf,
        read: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
        frame: Value,
    ) -> Value {
        let mut line = serde_json::to_vec(&frame).unwrap();
        line.push(b'\n');
        write.write_all(&line).await.unwrap();
        let mut resp = String::new();
        read.read_line(&mut resp).await.unwrap();
        serde_json::from_str(&resp).unwrap()
    }

    #[tokio::test]
    async fn bind_serves_protocol_5_and_unbind_aborts_open_connections() {
        let dir = tempfile::tempdir().unwrap();
        let services = test_services(dir.path());
        let session = message::SessionId::new("svc-s1".into()).unwrap();
        let messages = Messages::new(session);
        let queue = Queue::default();
        let ingress = Arc::new(Ingress::new(
            dir.path().to_path_buf(),
            1,
            messages,
            queue.clone(),
        ));
        services.bind("svc-s1", ingress.clone());

        let sock = dir.path().join("svc-s1.sock");
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut r = BufReader::new(r);

        let hello = exchange(
            &mut w,
            &mut r,
            json!({"id": 1, "op": "initialize", "protocol": 5, "client": "sensor"}),
        )
        .await;
        assert_eq!(hello["result"]["mode"], "service");
        assert_eq!(hello["result"]["session"], "svc-s1");

        let info = exchange(&mut w, &mut r, json!({"id": 2, "op": "info"})).await;
        assert_eq!(info["result"]["busy"], false);

        w.write_all(b"{\"op\":\"status\",\"text\":\"ok\"}\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while services.statuses("svc-s1") != vec!["ok".to_string()] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let sent = exchange(
            &mut w,
            &mut r,
            json!({"id": "r1", "op": "send", "text": "hello"}),
        )
        .await;
        assert_eq!(sent["result"]["accepted"], true);
        let msg_id = sent["result"]["message"].as_str().unwrap().to_string();

        // Retry with the same id succeeds idempotently and queues once.
        let retry = exchange(
            &mut w,
            &mut r,
            json!({"id": "r1", "op": "send", "text": "hello"}),
        )
        .await;
        assert_eq!(retry["result"]["message"], msg_id);
        assert_eq!(queue.lock().len(), 1);

        let info_busy = exchange(&mut w, &mut r, json!({"id": 3, "op": "info"})).await;
        assert_eq!(info_busy["result"]["busy"], true);

        // Clearing status removes the slot; unbinding drops socket and open connection.
        w.write_all(b"{\"op\":\"status\",\"text\":null}\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !services.statuses("svc-s1").is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        ingress.close();
        let closed = exchange(
            &mut w,
            &mut r,
            json!({"id": "r2", "op": "event", "text": "x", "source": "sensor", "reason": "disk"}),
        )
        .await;
        assert_eq!(closed["error"]["code"], "session");

        services.unbind("svc-s1");
        let mut eof = String::new();
        let bytes = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut eof))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, 0, "unbind must close open service connections");
        assert!(!sock.exists());
    }
}
