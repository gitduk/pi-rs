//! Services: tools declared with `service = true`, run by pi itself rather
//! than offered to the model. One process serves every session on the
//! machine: each session puts a unix socket in `$PI_HOME`'s runtime place,
//! and the service, one per machine under a lock it holds, connects to every
//! socket and speaks the same fd 3 protocol. What a service says becomes the
//! session's input; what it reports as status is drawn on the status line.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use message::{Block, InputKind, Request, WireKind, WireMessage};
use pi_tools::scripts::wire::{
    self, Id, Incoming, MAX_FRAME_BYTES, PROTOCOL, Speaker, Stream as Listening,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio::sync::mpsc;

use crate::execution::ingress::Ingress;

/// Where the lock fd is handed to a service, named to it by this variable.
const LOCK_FD: i32 = 3;
/// How often a pi looks again for services no lock holds.
const HEAL: Duration = Duration::from_secs(60);
// Detach a peer that stops draining its frames.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE: usize = MAX_FRAME_BYTES + 1;

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
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.accept.abort();
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
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let slot = next_slot.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(serve(
                    stream,
                    session_name.clone(),
                    ingress.clone(),
                    slots.clone(),
                    slot,
                ));
            }
        })
        .abort_handle();
        sockets.insert(session.to_string(), Socket { path, accept });
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
    let mut frames = Frames {
        reader: BufReader::new(read),
        line: Vec::new(),
        skipping: false,
        open: true,
    };
    let mut listeners = Listeners(Vec::new());
    while let Ok(frame) = frames.next().await {
        // Whether the connection stays up: a frame pi will not serve ends it.
        let stays = answer(frame, &session, &ingress, &slots, slot, &to, &mut listeners).await;
        if !stays {
            break;
        }
    }
    drop(listeners);
    drop(to);
    let _ = writer.await;
}

/// One line from a service: a request, a notice, or a reply with no question
/// behind it. Whether the connection stays up.
async fn answer(
    frame: Value,
    session: &str,
    ingress: &Arc<Ingress>,
    slots: &Slots,
    slot: u64,
    to: &mpsc::Sender<Value>,
    listeners: &mut Listeners,
) -> bool {
    if frame.get("op").is_none() {
        let id = frame.get("id").cloned();
        let closed = match id {
            Some(id) => match serde_json::from_value::<Id>(id) {
                Ok(id) => wire::failure(&id, "closed", "no question is waiting under that id"),
                Err(_) => wire::unreadable("frame", "a reply needs a usable id"),
            },
            None => wire::unreadable("frame", "a reply needs an id"),
        };
        return to.send(closed).await.is_ok();
    }
    let incoming: Incoming = match serde_json::from_value(frame) {
        Ok(incoming) => incoming,
        Err(error) => {
            tracing::warn!(target: "pi::services", %error, "frame rejected");
            return to
                .send(wire::unreadable("frame", format!("invalid frame: {error}")))
                .await
                .is_ok();
        }
    };
    if let Err((code, why)) = incoming.validate() {
        tracing::warn!(target: "pi::services", %why, "frame rejected");
        let said = match incoming.id() {
            Some(id) => wire::failure(id, code, why),
            None => wire::unreadable(code, why),
        };
        return to.send(said).await.is_ok();
    }
    match incoming {
        Incoming::Initialize { id, protocol } => {
            let frame = if protocol == PROTOCOL {
                wire::reply(&id, json!({"protocol": PROTOCOL}))
            } else {
                wire::failure(
                    &id,
                    "protocol",
                    format!("this pi speaks {PROTOCOL}; the service asked for {protocol}"),
                )
            };
            to.send(frame).await.is_ok()
        }
        Incoming::Say {
            id,
            re: None,
            stop: true,
            ..
        } => refuse(id.as_ref(), "stop names the message it closes", to).await,
        Incoming::Say {
            id,
            text,
            speaker,
            note: _,
            more,
            re: None,
            stop: false,
            source,
            reason,
        } => {
            if more {
                return refuse(id.as_ref(), "a service may not hold the checkout open", to).await;
            }
            let (kind, body) = match speaker {
                Speaker::Person => {
                    // The person's words, not the person's authority: a
                    // service relays what was said, never a command.
                    if text.starts_with('!') {
                        return refuse(
                            id.as_ref(),
                            "a line from a service may not run shell commands",
                            to,
                        )
                        .await;
                    }
                    (
                        WireKind::Request(Request::ToModel(InputKind::User)),
                        vec![Block::Text(text)],
                    )
                }
                Speaker::Task => {
                    let event = crate::execution::messages::new_id();
                    (
                        WireKind::Request(Request::ToModel(InputKind::Automatic)),
                        vec![
                            Block::Text(text),
                            Block::Data(json!({
                                "source": source,
                                "reason": reason,
                                "event": event.as_str(),
                            })),
                        ],
                    )
                }
            };
            let wire = WireMessage {
                id: crate::execution::messages::new_id(),
                session: match message::SessionId::new(session.to_string()) {
                    Ok(session) => session,
                    Err(why) => {
                        tracing::warn!(target: "pi::services", %why, "say refused");
                        return refuse(id.as_ref(), &why.to_string(), to).await;
                    }
                },
                by: None,
                at: agent::session::now(),
                kind,
                body,
            };
            match ingress.submit(0, wire) {
                Ok(_) => match id {
                    Some(id) => to
                        .send(wire::reply(&id, json!({"accepted": true})))
                        .await
                        .is_ok(),
                    None => true,
                },
                Err(why) => refuse(id.as_ref(), &why, to).await,
            }
        }
        Incoming::Say {
            id, re: Some(_), ..
        } => refuse(id.as_ref(), "a service may not answer questions", to).await,
        Incoming::Ask { id, .. } => {
            refuse(
                id.as_ref(),
                "a service may not ask; it may say and report",
                to,
            )
            .await
        }
        Incoming::Listen {
            id,
            to: Listening::Session,
            cursor,
        } => match ingress.messages.observe(cursor) {
            Ok(observation) => {
                let cursor = observation.cursor().clone();
                if to
                    .send(wire::reply(&id, json!({"cursor": cursor})))
                    .await
                    .is_err()
                {
                    return false;
                }
                let to = to.clone();
                let task = tokio::spawn(async move {
                    let mut observation = observation;
                    loop {
                        match observation.next().await {
                            Ok(event) => {
                                let frame = wire::message(&event.cursor, event.message.as_ref());
                                if to.send(frame).await.is_err() {
                                    return;
                                }
                            }
                            Err(_) => return,
                        }
                    }
                });
                listeners.0.push(task.abort_handle());
                true
            }
            Err(why) => to
                .send(wire::failure(&id, "internal", why.to_string()))
                .await
                .is_ok(),
        },
        Incoming::Listen {
            id,
            to: Listening::Questions,
            cursor: _,
        } => to
            .send(wire::failure(
                &id,
                "frame",
                "a service may not listen to questions",
            ))
            .await
            .is_ok(),
        Incoming::Status { text } => {
            slots.lock().expect("lock poisoned").insert(slot, text);
            true
        }
        Incoming::Stop {} => true,
    }
}

/// Answer a frame pi will not do, by id when it has one.
async fn refuse(id: Option<&Id>, why: &str, to: &mpsc::Sender<Value>) -> bool {
    tracing::warn!(target: "pi::services", %why, "frame refused");
    let frame = match id {
        Some(id) => wire::failure(id, "frame", why),
        None => wire::unreadable("frame", why),
    };
    to.send(frame).await.is_ok()
}

/// One line at a time off a socket, capped like the pipe a script speaks to.
struct Frames {
    reader: BufReader<OwnedReadHalf>,
    line: Vec<u8>,
    skipping: bool,
    open: bool,
}

impl Frames {
    async fn next(&mut self) -> Result<Value, String> {
        while self.open {
            let room = (MAX_LINE + 1 - self.line.len()) as u64;
            let read = (&mut self.reader)
                .take(room)
                .read_until(b'\n', &mut self.line)
                .await;
            let ended = matches!(read, Ok(0) | Err(_));
            if ended {
                self.open = false;
            } else if self.line.last() != Some(&b'\n') && self.line.len() > MAX_LINE {
                self.line.clear();
                self.skipping = true;
                continue;
            } else if self.line.last() != Some(&b'\n') {
                continue;
            }
            let line = std::mem::take(&mut self.line);
            if std::mem::take(&mut self.skipping) || line.len() > MAX_LINE {
                return Err("frame exceeds the byte limit".into());
            }
            if line.is_empty() && ended {
                return Err("frame connection closed".into());
            }
            return serde_json::from_slice(&line).map_err(|e| format!("invalid frame: {e}"));
        }
        std::future::pending().await
    }
}
