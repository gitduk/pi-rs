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

/// How long tool notices are held before going out as one message; a turn
/// can call dozens of tools and each as its own message would flood the phone.
const TOOL_INTERVAL: Duration = Duration::from_secs(5);

/// The byte budget for one outbound message. The protocol documents no limit
/// and the reference implementation never splits, so this is a floor we chose,
/// not a ceiling anyone published. Bytes rather than characters: nothing says
/// whether the server counts UTF-8 bytes or UTF-16 units, and for the CJK an
/// answer is likely to contain, bytes are the smaller of the two budgets.
const MESSAGE_LIMIT: usize = 2000;

/// Room held back for the `(n/m)` marker so a piece plus its marker still fits
/// the budget. Ten bytes at three digits a side, rounded up.
const MARKER_RESERVE: usize = 12;

/// The pause between pieces of one split message. The terms of use let the
/// server rate-limit, and a burst is what a rate limiter watches for; at this
/// length the reader cannot tell.
const PIECE_INTERVAL: Duration = Duration::from_millis(500);

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
    /// When the last tool batch went out, for the interval above.
    last_tool: Option<Instant>,
    /// Tool notices held since that send. Coalescing them is what keeps a
    /// parallel call — same instant as its sibling — from being lost.
    tool_buf: Vec<String>,
    /// Whether the indicator is currently marked on. Optimistic: the send
    /// tasks correct the server side, so a failed send only leaves the mark
    /// stale, never freezes the surface.
    typing_on: bool,
    /// Orders the indicator's on/off sends and caches the per-peer ticket.
    typing: Arc<Mutex<Typing>>,
    /// The outbound task last spawned. The next one awaits it, so a split
    /// answer's tail cannot be overtaken by the next turn's first notice.
    last_send: Option<JoinHandle<()>>,
    /// Stops the chain above. A single send was over before `off` could
    /// matter; a split one runs for seconds, and until this the phone kept
    /// receiving pieces after the bridge had reported itself stopped.
    outbound: CancellationToken,
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
            tool_buf: Vec::new(),
            typing_on: false,
            typing: Arc::new(Mutex::new(Typing::default())),
            last_send: None,
            outbound: CancellationToken::new(),
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
        self.outbound.cancel();
        self.outbound = CancellationToken::new();
        self.last_send = None;
        self.typing_on = false;
        self.out.clear();
        self.tool_buf.clear();
        self.last_tool = None;
        self.flushed = false;
        vec!["wechat bridge stopped — /wechat on to reconnect".into()]
    }

    /// Watch one agent event. Text deltas and tool lines accumulate; every
    /// other notice, and `Done`, flush what is held before saying its own.
    pub async fn observe(&mut self, event: &Event) {
        match event {
            Event::TurnStart { turn: 1 } => {
                self.out.clear();
                // A turn that ended out of sight never reached `finish_turn`,
                // and its leftovers would surface inside this turn's first batch.
                self.tool_buf.clear();
                self.flushed = false;
                // Or the previous turn's last send would swallow this one's
                // first tool line, seconds after the user asked for it.
                self.last_tool = None;
                self.typing(true).await;
            }
            Event::TextDelta(text) => self.out.push_str(text),
            Event::ToolStart { name, args, .. } => {
                self.tool_buf.push(tool_line(name, args));
                let now = Instant::now();
                if self.last_tool.is_none_or(|t| now.duration_since(t) >= TOOL_INTERVAL) {
                    self.last_tool = Some(now);
                    self.flush_tools().await;
                }
            }
            Event::ToolEnd {
                is_error: true,
                name,
                preview,
                ..
            } => {
                self.say(&format!("✗ {name} failed — {}", crate::render::clip(preview, 80)))
                    .await;
            }
            Event::ToolDenied {
                name, reason, ..
            } => self.say(&format!("✗ {name} denied — {reason}")).await,
            Event::Retrying {
                attempt,
                delay_ms,
                reason,
                ..
            } => {
                self.say(&format!("↻ retry {attempt} in {}s — {reason}", delay_ms / 1000))
                    .await;
            }
            Event::Warning(w) => self.say(w).await,
            Event::Compacted(r) => {
                self.say(&format!(
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
            self.flush_tools().await;
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
        // Before the answer: the turn's last batch is still held, and the
        // answer arriving first would read as the tools running after it.
        self.flush_tools().await;
        if !text.trim().is_empty() {
            self.send_line(&text).await;
        }
        self.typing(false).await;
    }

    /// Send what the interval has held as one multi-line message. Empty is
    /// the ordinary case — most drains find nothing to say.
    async fn flush_tools(&mut self) {
        if self.tool_buf.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.tool_buf).join("\n");
        self.send_line(&text).await;
    }

    /// One outbound line that is not a tool notice. Held tool lines go first,
    /// so the phone reads the turn in the order it happened.
    async fn say(&mut self, text: &str) {
        self.flush_tools().await;
        self.send_line(text).await;
    }

    /// One outbound message, sent from its own task so a slow or failing
    /// send cannot stall the surface that called it. Failures land on the
    /// local terminal rather than vanishing: the terms of use allow the
    /// server to rate-limit or block, and that has to be visible here.
    async fn send_line(&mut self, text: &str) {
        // The surface calls `observe` whether or not the bridge is on, and
        // `off` keeps the credentials, so an ended bridge would keep sending.
        if !self.alive() {
            return;
        }
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
        let pieces = split(text, MESSAGE_LIMIT);
        let previous = self.last_send.take();
        let stop = self.outbound.clone();
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let total = pieces.len();
            for (i, piece) in pieces.into_iter().enumerate() {
                if stop.is_cancelled() {
                    break;
                }
                if i > 0 {
                    tokio::select! {
                        () = tokio::time::sleep(PIECE_INTERVAL) => {}
                        () = stop.cancelled() => break,
                    }
                }
                // A later piece without the ones before it reads as garbage,
                // so a failed send ends the message rather than skipping a hole.
                if let Err(e) = client.send_text(&token, &peer, &context_token, &piece).await {
                    let part =
                        if total > 1 { format!(" (piece {}/{total})", i + 1) } else { String::new() };
                    let _ = tx.send(Inbound::Notice(format!(
                        "wechat send failed{part}: {e:#} — try sending a message from the phone first"
                    )));
                    break;
                }
            }
        });
        self.last_send = Some(handle);
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
        if !self.alive() || self.typing_on == on {
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
        // Keep the last valid token: a tokenless message must not erase it
        // (the reference stores only when one is present).
        if let Some(token) = &msg.context_token {
            s.context_token = Some(token.clone());
        }
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

/// The one-line tool notice the phone gets, same shape as the TUI's own
/// `describe`: the name plus the argument the summary picked out. Empty args
/// collapse to the bare name so the line never ends on a stray space.
fn tool_line(name: &str, args: &serde_json::Value) -> String {
    match crate::render::summarize(args) {
        summary if summary.is_empty() => format!("⚙ {name}"),
        summary => format!("⚙ {name} {summary}"),
    }
}

/// Cut an outbound message into pieces that each fit `limit` bytes. One that
/// already fits comes back whole and unmarked; anything longer is marked
/// `(n/m)`, so a reader on the phone can tell a message still arriving from one
/// that ended — a send the server blocks reports on the local terminal only,
/// and the phone would otherwise see a truncated answer as the whole answer.
fn split(text: &str, limit: usize) -> Vec<String> {
    // Against the real limit, not the loop's smaller budget: text that fits
    // unmarked should go out unmarked rather than become two marked pieces.
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let budget = limit.saturating_sub(MARKER_RESERVE).max(1);
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= budget {
            pieces.push(rest.to_string());
            break;
        }
        let (cut, skip) = boundary(rest, budget);
        pieces.push(rest[..cut].to_string());
        rest = &rest[cut + skip..];
    }
    let total = pieces.len();
    if total > 1 {
        for (i, piece) in pieces.iter_mut().enumerate() {
            piece.insert_str(0, &format!("({}/{total}) ", i + 1));
        }
    }
    pieces
}

/// Where to end a piece that overflows `budget`, and how many separator bytes
/// to drop after it: the last paragraph break within reach, else the last line
/// break, else the last space, else the last character boundary. A hard cut
/// drops nothing — indentation inside a code block is content, and the halves
/// have to rejoin exactly. A boundary in the first half of the budget is worse
/// than no boundary at all: taking it doubles the number of messages.
///
/// The cut is never zero: a budget shorter than the first character would
/// otherwise leave the caller's loop exactly where it started, forever.
fn boundary(rest: &str, budget: usize) -> (usize, usize) {
    let first = rest.chars().next().map_or(1, char::len_utf8);
    let mut end = budget.max(first);
    while end > first && !rest.is_char_boundary(end) {
        end -= 1;
    }
    let head = &rest[..end];
    for sep in ["\n\n", "\n", " "] {
        let Some(i) = head.rfind(sep).filter(|&i| i > end / 2) else {
            continue;
        };
        let skip = if sep == " " {
            1
        } else {
            rest[i..].bytes().take_while(|b| matches!(b, b'\n' | b'\r')).count()
        };
        return (i, skip);
    }
    (end, 0)
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

    #[test]
    fn a_message_within_the_budget_goes_out_whole_and_unmarked() {
        let text = "short enough";
        assert_eq!(split(text, 100), vec![text.to_string()]);
    }

    #[test]
    fn a_long_message_breaks_at_paragraphs_and_carries_its_count() {
        let para = "x".repeat(60);
        let text = format!("{para}\n\n{para}\n\n{para}");
        let pieces = split(&text, 100);
        assert_eq!(pieces.len(), 3);
        for (i, piece) in pieces.iter().enumerate() {
            assert!(piece.starts_with(&format!("({}/3) ", i + 1)), "{piece}");
            assert!(piece.len() <= 100, "{} bytes", piece.len());
            assert!(piece.ends_with('x'), "the break ate content: {piece}");
        }
    }

    #[test]
    fn a_run_with_no_boundary_is_cut_on_a_character_and_rejoins_exactly() {
        let text = "中".repeat(200);
        let pieces = split(&text, 100);
        assert!(pieces.len() > 1);
        let rejoined: String = pieces
            .iter()
            .map(|p| p.split_once(") ").expect("marker").1)
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn a_hard_cut_keeps_the_indentation_it_lands_on() {
        let text = format!("{}\n    indented tail", "x".repeat(120));
        let pieces = split(&text, 60);
        let tail = pieces.last().expect("a piece");
        assert!(tail.ends_with("    indented tail"), "{tail}");
    }

    #[test]
    fn a_budget_shorter_than_one_character_still_advances() {
        let pieces = split("中文中文", 1);
        assert_eq!(pieces.len(), 4);
    }

    #[test]
    fn a_boundary_too_early_in_the_budget_is_not_worth_taking() {
        // The only newline sits at byte 5; cutting there would send a
        // five-byte message and leave the rest just as long as before.
        let text = format!("head\n{}", "x".repeat(200));
        let pieces = split(&text, 100);
        assert!(pieces[0].len() > 50, "{}", pieces[0]);
    }

    #[test]
    fn a_tool_line_carries_the_summarized_argument() {
        let args = serde_json::json!({ "path": "crates/cli/src/wechat.rs" });
        assert_eq!(
            tool_line("edit", &args),
            "⚙ edit crates/cli/src/wechat.rs"
        );
        assert_eq!(tool_line("read", &serde_json::json!({})), "⚙ read");
    }

    fn started(name: &str) -> Event {
        Event::ToolStart {
            id: name.into(),
            name: name.into(),
            args: serde_json::json!({}),
        }
    }

    /// The bridge is not alive here, so every send is a no-op and the
    /// accumulator is the only thing under test.
    #[tokio::test]
    async fn calls_inside_the_interval_are_held_rather_than_dropped() {
        let mut b = Bridge::new();
        b.observe(&Event::TurnStart { turn: 1 }).await;
        // Three in the same instant: the shape a parallel call arrives in,
        // and the one the old rate limit threw away.
        for name in ["read", "grep", "bash"] {
            b.observe(&started(name)).await;
        }
        assert_eq!(b.tool_buf, ["⚙ grep", "⚙ bash"]);
        b.finish_turn(false).await;
        assert!(b.tool_buf.is_empty(), "{:?}", b.tool_buf);
    }

    #[tokio::test]
    async fn a_new_turn_reopens_the_interval() {
        let mut b = Bridge::new();
        b.observe(&Event::TurnStart { turn: 1 }).await;
        b.observe(&started("read")).await;
        assert!(b.last_tool.is_some());
        b.observe(&Event::TurnStart { turn: 1 }).await;
        b.observe(&started("grep")).await;
        assert!(b.tool_buf.is_empty(), "{:?}", b.tool_buf);
    }

    /// The lane-switch case: the turn's events stop reaching the bridge, so
    /// `finish_turn` never runs and the held lines outlive their turn.
    #[tokio::test]
    async fn a_turn_that_never_ended_leaves_nothing_for_the_next_one() {
        let mut b = Bridge::new();
        b.observe(&Event::TurnStart { turn: 1 }).await;
        b.observe(&started("read")).await;
        b.observe(&started("grep")).await;
        assert_eq!(b.tool_buf, ["⚙ grep"]);
        b.observe(&Event::TurnStart { turn: 1 }).await;
        assert!(b.tool_buf.is_empty(), "{:?}", b.tool_buf);
    }

    #[tokio::test]
    async fn an_error_line_drains_the_held_calls_first() {
        let mut b = Bridge::new();
        b.observe(&Event::TurnStart { turn: 1 }).await;
        b.observe(&started("read")).await;
        b.observe(&started("grep")).await;
        assert_eq!(b.tool_buf, ["⚙ grep"]);
        b.observe(&Event::Warning("careful".into())).await;
        assert!(b.tool_buf.is_empty(), "{:?}", b.tool_buf);
    }
}
