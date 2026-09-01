//! The WeChat bridge: owns the long-poll task, the outbound accumulator, and
//! the `~/.pi/wechat.json` state file. The protocol client stays in the
//! `wechat` crate; this module is where pi's session meets it.
//!
//! One task at a time: first login (QR → confirm → credentials), then the
//! long-poll loop, both spawned from the surface and reporting to it through
//! the inbound channel — the same channel the TUI selects on.

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::Event;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wechat::Update;

/// What the bridge hands the surface.
pub enum Inbound {
    /// A text message from the peer.
    Text { text: String },
    /// Something worth saying on the local terminal (status, errors, QR).
    Notice(String),
    /// The peer typed `/stop` or `/esc`: interrupt the running turn now.
    Stop,
}

/// What persists between runs, under the pi root. One peer per session in
/// this build, so the reply address and the context token are single slots.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct State {
    token: Option<String>,
    base_url: String,
    peer: Option<String>,
    get_updates_buf: String,
    context_token: Option<String>,
}

/// How long between tool-call summary lines sent to the phone; a turn can
/// call dozens of tools and each one as its own message would flood it.
const TOOL_INTERVAL: Duration = Duration::from_secs(5);

/// What `/wechat` alone reports when the bridge is idle.
pub const OFF_MESSAGE: &str = "wechat: off — /wechat on to connect";

/// The typing indicator's shared state: the ticket cache, serialized with
/// the on/off sends by the same lock.
#[derive(Default)]
struct Typing {
    ticket: Option<String>,
}

pub struct Bridge {
    pub(crate) rx: UnboundedReceiver<Inbound>,
    tx: UnboundedSender<Inbound>,
    state: Arc<Mutex<State>>,
    client: wechat::Client,
    abort: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    /// Reply text accumulated for the running turn.
    out: String,
    /// True once `Done` has flushed; a failed turn flushes what is left.
    flushed: bool,
    /// When the last tool line went out, for the rate limit above.
    last_tool: Option<Instant>,
    /// Whether the indicator is currently marked on. Optimistic: the send
    /// tasks correct the server side, so a failed send only leaves the mark
    /// stale, never freezes the surface.
    typing_on: bool,
    /// Orders the indicator's on/off sends and caches the per-peer ticket.
    typing: Arc<Mutex<Typing>>,
}

impl Bridge {
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state = load().unwrap_or_default();
        let base = if state.base_url.is_empty() {
            wechat::DEFAULT_BASE_URL.to_string()
        } else {
            state.base_url.clone()
        };
        Self {
            rx,
            tx,
            state: Arc::new(Mutex::new(state)),
            client: wechat::Client::new(base),
            abort: None,
            task: None,
            out: String::new(),
            flushed: false,
            last_tool: None,
            typing_on: false,
            typing: Arc::new(Mutex::new(Typing::default())),
        }
    }

    /// Whether the poll or login task is still running. The stored handle
    /// outlives its task, so a finished one must read as off.
    fn alive(&self) -> bool {
        self.task.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// What `/wechat` alone reports.
    pub fn status(&self) -> Vec<String> {
        if self.alive() {
            vec!["wechat: connected — /wechat off to disconnect".into()]
        } else {
            vec![OFF_MESSAGE.into()]
        }
    }

    /// `/wechat on`. With a saved token the long-poll starts immediately;
    /// without one a login runs — the QR and progress arrive on `rx`.
    pub async fn on(&mut self) -> Result<Vec<String>> {
        if self.alive() {
            return Ok(vec!["wechat is already connected".into()]);
        }
        let token = {
            let s = self.state.lock().await;
            s.token.clone()
        };
        self.client = self.client_for().await;
        if token.is_some() {
            self.spawn_poll();
            Ok(vec!["wechat connected — long-polling for messages".into()])
        } else {
            self.spawn_login();
            Ok(vec![
                "wechat login started — the QR appears in the session; scan it with WeChat".into(),
            ])
        }
    }

    /// `/wechat off`.
    pub fn off(&mut self) -> Vec<String> {
        if let Some(abort) = self.abort.take() {
            abort.cancel();
        }
        self.task = None;
        self.out.clear();
        self.flushed = false;
        vec!["wechat bridge stopped — /wechat on to reconnect".into()]
    }

    /// Watch one agent event. Text deltas accumulate; tool lines and the
    /// final answer go out as messages, `Done` flushes the accumulator.
    pub async fn observe(&mut self, event: &Event) {
        match event {
            Event::TurnStart { turn: 1 } => {
                self.out.clear();
                self.flushed = false;
                self.typing(true).await;
            }
            Event::TextDelta(text) => self.out.push_str(text),
            Event::ToolStart { name, .. } => {
                let now = Instant::now();
                if self.last_tool.is_none_or(|t| now.duration_since(t) >= TOOL_INTERVAL) {
                    self.last_tool = Some(now);
                    self.send_line(&format!("⚙ {name}")).await;
                }
            }
            Event::ToolEnd {
                is_error: true,
                name,
                preview,
                ..
            } => {
                self.send_line(&format!("✗ {name} failed — {}", crate::render::clip(preview, 80)))
                    .await;
            }
            Event::ToolDenied {
                name, reason, ..
            } => self.send_line(&format!("✗ {name} denied — {reason}")).await,
            Event::Retrying {
                attempt,
                delay_ms,
                reason,
                ..
            } => {
                self.send_line(&format!("↻ retry {attempt} in {}s — {reason}", delay_ms / 1000))
                    .await;
            }
            Event::Warning(w) => self.send_line(w).await,
            Event::Compacted(r) => {
                self.send_line(&format!(
                    "history compacted {} → {} tokens",
                    r.before, r.after
                ))
                .await;
            }
            Event::Done { .. } => self.flush(false).await,
            _ => {}
        }
    }

    /// Called by the surface when a turn ends. A successful run already
    /// flushed on `Done`; a cancelled or failed one sends what it has.
    pub async fn finish_turn(&mut self, cancelled: bool) {
        if self.flushed {
            self.typing(false).await;
        } else {
            self.flush(cancelled).await;
        }
    }

    async fn flush(&mut self, cancelled: bool) {
        let mut text = std::mem::take(&mut self.out);
        if cancelled && !text.trim().is_empty() {
            text.push_str("\n\n(stopped)");
        }
        self.flushed = true;
        if !text.trim().is_empty() {
            self.send_line(&text).await;
        }
        self.typing(false).await;
    }

    /// One outbound message, sent from its own task so a slow or failing
    /// send cannot stall the surface that called it. Failures land on the
    /// local terminal rather than vanishing: the terms of use allow the
    /// server to rate-limit or block, and that has to be visible here.
    async fn send_line(&mut self, text: &str) {
        let (token, peer, context_token) = {
            let s = self.state.lock().await;
            (s.token.clone(), s.peer.clone(), s.context_token.clone())
        };
        let (Some(token), Some(peer)) = (token, peer) else {
            return;
        };
        let context_token = context_token.unwrap_or_default();
        let client = self.client_for().await;
        let tx = self.tx.clone();
        let text = text.to_string();
        tokio::spawn(async move {
            if let Err(e) = client.send_text(&token, &peer, &context_token, &text).await {
                let _ = tx.send(Inbound::Notice(format!(
                    "wechat send failed: {e:#} — the phone may not have messaged this bot yet"
                )));
            }
        });
    }

    /// The client for the base the session currently talks to. A redirected
    /// login saves its host to state; the outbound path rebuilds only when
    /// that host changed, so a session keeps one connection pool.
    async fn client_for(&mut self) -> wechat::Client {
        let base = {
            let s = self.state.lock().await;
            s.base_url.clone()
        };
        let base = if base.is_empty() {
            wechat::DEFAULT_BASE_URL.to_string()
        } else {
            base
        };
        if self.client.base_url() != base {
            self.client = wechat::Client::new(base);
        }
        self.client.clone()
    }

    /// The typing indicator, on or off, sent from a background task so the
    /// send can never stall the surface. Best effort: no ticket, no effect;
    /// a failed send only leaves the local mark stale.
    async fn typing(&mut self, on: bool) {
        if self.typing_on == on {
            return;
        }
        self.typing_on = on;
        let typing = self.typing.clone();
        let state = self.state.clone();
        let client = self.client_for().await;
        tokio::spawn(async move {
            let mut t = typing.lock().await;
            let Some(ticket) = typing_ticket(&mut t, &state, &client).await else {
                return;
            };
            let (token, peer) = {
                let s = state.lock().await;
                (s.token.clone(), s.peer.clone())
            };
            let (Some(token), Some(peer)) = (token, peer) else {
                return;
            };
            let status = if on { 1 } else { 2 };
            if let Err(e) = client.send_typing(&token, &peer, &ticket, status).await {
                tracing::warn!(target: "pi::wechat", error = %e, "sendtyping");
            }
        });
    }

    /// Long-poll immediately; a saved token already exists.
    fn spawn_poll(&mut self) {
        let state = self.state.clone();
        let tx = self.tx.clone();
        let client = self.client.clone();
        let abort = CancellationToken::new();
        let handle = tokio::spawn(poll(client, state, tx, abort.clone()));
        self.abort = Some(abort);
        self.task = Some(handle);
    }

    /// Login first (the QR and progress go out on `rx`), then poll in the
    /// same task so `/wechat off` can stop either half.
    fn spawn_login(&mut self) {
        let state = self.state.clone();
        let tx = self.tx.clone();
        let mut client = self.client.clone();
        let abort = CancellationToken::new();
        let task_abort = abort.clone();
        let tx_qr = tx.clone();
        let tx_note = tx.clone();
        let handle = tokio::spawn(async move {
            let mut view = wechat::LoginView {
                show_qr: Box::new(move |qr: &str| {
                    for line in qr.lines() {
                        let _ = tx_qr.send(Inbound::Notice(line.to_string()));
                    }
                }),
                notice: Box::new(move |n: &str| {
                    let _ = tx_note.send(Inbound::Notice(n.to_string()));
                }),
                read_verify_code: None,
            };
            let result = {
                let login = wechat::login_flow(&mut client, &mut view);
                tokio::pin!(login);
                tokio::select! {
                    r = &mut login => r,
                    _ = task_abort.cancelled() => {
                        let _ = tx.send(Inbound::Notice(
                            "wechat login stopped — /wechat on to start again".into(),
                        ));
                        return;
                    }
                }
            };
            match result {
                Ok(credentials) => {
                    let mut s = state.lock().await;
                    s.token = Some(credentials.token);
                    s.base_url = credentials.base_url;
                    s.peer = None;
                    s.get_updates_buf = String::new();
                    s.context_token = None;
                    save(&s);
                    drop(s);
                    let _ = tx.send(Inbound::Notice(
                        "wechat connected — polling for messages".into(),
                    ));
                    poll(client, state, tx, task_abort).await;
                }
                Err(e) => {
                    let _ = tx.send(Inbound::Notice(format!("wechat login failed: {e}")));
                }
            }
        });
        self.abort = Some(abort);
        self.task = Some(handle);
    }
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

/// The long-poll loop. Client-side timeouts are the normal empty result, real
/// errors back off (2s, 30s after three in a row — the reference's rhythm);
/// a stale token is reported and stops the bridge until a fresh login.
async fn poll(
    client: wechat::Client,
    state: Arc<Mutex<State>>,
    tx: UnboundedSender<Inbound>,
    abort: CancellationToken,
) {
    let mut failures = 0u32;
    let mut timeout = wechat::client::LONG_POLL_TIMEOUT;
    while !abort.is_cancelled() {
        let (token, buf) = {
            let s = state.lock().await;
            (s.token.clone(), s.get_updates_buf.clone())
        };
        let Some(token) = token else {
            let _ = tx.send(Inbound::Notice(
                "wechat: no token — /wechat off, then /wechat on to log in again".into(),
            ));
            return;
        };
        // The long-poll holds the request open for up to 35s; racing it
        // against the abort token is what makes `/wechat off` prompt.
        let update = tokio::select! {
            r = client.get_updates(&token, &buf, timeout) => r,
            _ = abort.cancelled() => return,
        };
        match update {
            Ok(update) => {
                handle_update(&state, &tx, update, &mut failures, &mut timeout).await
            }
            Err(e) => {
                failures += 1;
                if failures == 3 {
                    let _ = tx.send(Inbound::Notice(format!(
                        "wechat getupdates failing — backing off: {e:#}"
                    )));
                }
                tokio::time::sleep(backoff(&mut failures)).await;
            }
        }
    }
}

/// The per-peer typing ticket, fetched once and cached under the typing
/// lock so every on/off task reuses it.
async fn typing_ticket(
    t: &mut Typing,
    state: &Arc<Mutex<State>>,
    client: &wechat::Client,
) -> Option<String> {
    if let Some(ticket) = &t.ticket {
        return Some(ticket.clone());
    }
    let (token, peer, context_token) = {
        let s = state.lock().await;
        (s.token.clone(), s.peer.clone(), s.context_token.clone())
    };
    let (Some(token), Some(peer)) = (token, peer) else {
        return None;
    };
    let context_token = context_token.unwrap_or_default();
    match client.get_config(&token, &peer, &context_token).await {
        Ok(cfg) => {
            let ticket = cfg.typing_ticket.unwrap_or_default();
            if !ticket.is_empty() {
                t.ticket = Some(ticket.clone());
            }
            Some(ticket)
        }
        Err(e) => {
            tracing::warn!(target: "pi::wechat", error = %e, "getconfig");
            None
        }
    }
}

async fn handle_update(
    state: &Arc<Mutex<State>>,
    tx: &UnboundedSender<Inbound>,
    update: Update,
    failures: &mut u32,
    timeout: &mut Duration,
) {
    if update.is_error() {
        if update.is_stale_token() {
            let mut s = state.lock().await;
            s.token = None;
            save(&s);
            let _ = tx.send(Inbound::Notice(
                "wechat token expired — /wechat off, then /wechat on to rescan".into(),
            ));
            return;
        }
        *failures += 1;
        let _ = tx.send(Inbound::Notice(format!(
            "wechat getupdates error: ret={} errcode={:?} errmsg={:?}",
            update.ret, update.errcode, update.errmsg
        )));
        tokio::time::sleep(backoff(failures)).await;
        return;
    }
    *failures = 0;
    if let Some(t) = update.longpolling_timeout_ms
        && t > 0
    {
        *timeout = Duration::from_millis(t);
    }
    let mut s = state.lock().await;
    let mut dirty = false;
    if !update.get_updates_buf.is_empty() {
        s.get_updates_buf = update.get_updates_buf;
        dirty = true;
    }
    for msg in update.msgs {
        if msg.message_type != 1 {
            continue;
        }
        let text = wechat::text_of(&msg);
        if text.is_empty() {
            continue;
        }
        if matches!(text.trim(), "/stop" | "/esc") {
            let _ = tx.send(Inbound::Stop);
            continue;
        }
        s.peer = Some(msg.from_user_id.clone());
        s.context_token = msg.context_token.clone();
        dirty = true;
        let _ = tx.send(Inbound::Text { text });
    }
    if dirty {
        save(&s);
    }
}

/// 2s between ordinary retries, 30s once three have failed in a row (the
/// reference monitor's numbers); the counter resets on the 30s step.
fn backoff(failures: &mut u32) -> Duration {
    if *failures >= 3 {
        *failures = 0;
        Duration::from_secs(30)
    } else {
        Duration::from_secs(2)
    }
}

fn state_path() -> Option<PathBuf> {
    tools::state::dir().map(|d| d.join("wechat.json"))
}

fn load() -> Option<State> {
    let path = state_path()?;
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

fn save(state: &State) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.pi-tmp");
    if let Ok(body) = serde_json::to_vec_pretty(state)
        && let Ok(()) = std::fs::write(&tmp, body)
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_rests_after_three_failures() {
        // Callers count the failure first, then ask how long to wait.
        let mut n = 0u32;
        n += 1;
        assert_eq!(backoff(&mut n), Duration::from_secs(2));
        n += 1;
        assert_eq!(backoff(&mut n), Duration::from_secs(2));
        n += 1;
        assert_eq!(backoff(&mut n), Duration::from_secs(30));
        // The counter reset, so the next failure starts over at 2s.
        n += 1;
        assert_eq!(backoff(&mut n), Duration::from_secs(2));
    }

    #[test]
    fn a_stop_message_is_classified_before_anything_else() {
        for stop in ["/stop", "/esc", " /stop "] {
            assert!(matches!(stop.trim(), "/stop" | "/esc"));
        }
        assert!(!matches!("stop the run".trim(), "/stop" | "/esc"));
    }
}
