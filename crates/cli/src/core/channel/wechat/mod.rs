//! WeChat as a channel: the login, the long-poll, and the
//! `~/.pi/wechat.json` state file. The protocol client stays in the `wechat`
//! crate; this module is where pi's session meets it.

mod markdown;

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use wechat::Update;

use super::{Channel, Inbound, Inbox};
use markdown::format_markdown;

// What persists between runs, under the pi root. One peer per session in
// this build, so the reply address and the context token are single slots.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct State {
    token: Option<String>,
    base_url: String,
    peer: Option<String>,
    get_updates_buf: String,
    context_token: Option<String>,
}

// The byte budget for one outbound message. The protocol documents no limit
// and the reference implementation never splits, so this is a floor we chose,
// not a ceiling anyone published. Bytes rather than characters: nothing says
// whether the server counts UTF-8 bytes or UTF-16 units, and for the CJK an
// answer is likely to contain, bytes are the smaller of the two budgets.
const MESSAGE_LIMIT: usize = 2000;

// The typing indicator's shared state: the ticket cache, serialized with
// the on/off sends by the same lock.
#[derive(Default)]
struct Typing {
    ticket: Option<String>,
}

pub struct WeChat {
    state: Arc<Mutex<State>>,
    client: std::sync::Mutex<wechat::Client>,
    typing: Mutex<Typing>,
}

impl WeChat {
    /// The word `/wechat` answers to; the command table reads it from here.
    pub const NAME: &'static str = "wechat";

    pub fn new() -> Self {
        let state = load().unwrap_or_default();
        let client = wechat::Client::new(base_of(&state));
        Self {
            state: Arc::new(Mutex::new(state)),
            client: std::sync::Mutex::new(client),
            typing: Mutex::default(),
        }
    }

    // The client for the base the session currently talks to. A redirected
    // login saves its host to state; the client is rebuilt only when that
    // host changed, so a session keeps one connection pool.
    async fn client(&self) -> wechat::Client {
        let base = base_of(&*self.state.lock().await);
        let mut client = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        if client.base_url() != base {
            *client = wechat::Client::new(base);
        }
        client.clone()
    }
}

impl Default for WeChat {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for WeChat {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn limit(&self) -> usize {
        MESSAGE_LIMIT
    }

    fn format(&self, markdown: &str) -> String {
        format_markdown(markdown)
    }

    // With a saved token the long-poll starts at once; without one a login
    // runs first, in the same task so `/wechat off` can stop either half.
    async fn run(&self, tx: Inbox, abort: CancellationToken) {
        let client = self.client().await;
        let logged_in = self.state.lock().await.token.is_some();
        if logged_in {
            let _ = tx.send(Inbound::Flash(
                "wechat connected — long-polling for messages".into(),
            ));
            poll(client, self.state.clone(), tx, abort).await;
        } else {
            login(client, self.state.clone(), tx, abort).await;
        }
    }

    async fn send(&self, text: &str) -> anyhow::Result<()> {
        let (token, peer, context_token) = {
            let s = self.state.lock().await;
            (s.token.clone(), s.peer.clone(), s.context_token.clone())
        };
        let (Some(token), Some(peer)) = (token, peer) else {
            return Ok(());
        };
        let context_token = context_token.unwrap_or_default();
        self.client()
            .await
            .send_text(&token, &peer, &context_token, text)
            .await
            .map_err(|e| anyhow::anyhow!("{e:#} — try sending a message from the phone first"))
    }

    // No ticket, no effect; a failed send is logged and otherwise ignored.
    async fn typing(&self, on: bool) {
        let client = self.client().await;
        let mut t = self.typing.lock().await;
        let Some(ticket) = typing_ticket(&mut t, &self.state, &client).await else {
            return;
        };
        let (token, peer) = {
            let s = self.state.lock().await;
            (s.token.clone(), s.peer.clone())
        };
        let (Some(token), Some(peer)) = (token, peer) else {
            return;
        };
        let status = if on { 1 } else { 2 };
        if let Err(e) = client.send_typing(&token, &peer, &ticket, status).await {
            tracing::warn!(target: "pi::wechat", error = %e, "sendtyping");
        }
    }
}

fn base_of(state: &State) -> String {
    if state.base_url.is_empty() {
        wechat::DEFAULT_BASE_URL.to_string()
    } else {
        state.base_url.clone()
    }
}

// QR → confirm → credentials, then the long-poll. The QR and progress go out
// as notices, where they stay long enough to be scanned.
async fn login(
    mut client: wechat::Client,
    state: Arc<Mutex<State>>,
    tx: Inbox,
    abort: CancellationToken,
) {
    let _ = tx.send(Inbound::Notice(
        "wechat login started — scan the QR below with WeChat".into(),
    ));
    let tx_qr = tx.clone();
    let tx_note = tx.clone();
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
            _ = abort.cancelled() => {
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
            let _ = tx.send(Inbound::Flash(
                "wechat connected — polling for messages".into(),
            ));
            poll(client, state, tx, abort).await;
        }
        Err(e) => {
            let _ = tx.send(Inbound::Notice(format!("wechat login failed: {e}")));
        }
    }
}

// The long-poll loop. Client-side timeouts are the normal empty result, real
// errors back off (2s, 30s after three in a row — the reference's rhythm);
// a stale token is reported and stops the channel until a fresh login.
async fn poll(
    client: wechat::Client,
    state: Arc<Mutex<State>>,
    tx: Inbox,
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
            Ok(update) => handle_update(&state, &tx, update, &mut failures, &mut timeout).await,
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

// The per-peer typing ticket, fetched once and cached under the typing
// lock so every on/off task reuses it.
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
    tx: &Inbox,
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

// 2s between ordinary retries, 30s once three have failed in a row (the
// reference monitor's numbers); the counter resets on the 30s step.
fn backoff(failures: &mut u32) -> Duration {
    if *failures >= 3 {
        *failures = 0;
        Duration::from_secs(30)
    } else {
        Duration::from_secs(2)
    }
}

fn state_path() -> Option<PathBuf> {
    tool::state::dir().map(|d| d.join("wechat.json"))
}

fn load() -> Option<State> {
    let path = state_path()?;
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

// The credentials here are the phone's, not the provider's, but they are a
// login either way: through the same private write, so the file is 0600 and a
// crash mid-save leaves the last good copy rather than half of this one.
fn save(state: &State) {
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_vec_pretty(state) {
        let _ = tool::state::write_private(&path, &body);
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
}
