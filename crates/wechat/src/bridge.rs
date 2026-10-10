//! pi-wechat as a pi job: what the person types on the phone goes up fd 3 as
//! `input` (or `interrupt` for `/stop`), and the turn it opens comes back as
//! `started`, then one `reply`, cut into messages and sent on to the phone.

use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::sync::mpsc;

use crate::markdown::format_markdown;
use crate::split::split;
use crate::{Client, Update};

// No documented limit — a floor we chose, not a published ceiling. Bytes, not
// chars, since server counting is unspecified and bytes are the smaller CJK budget.
const MESSAGE_LIMIT: usize = 2000;
// Between pieces of one reply: a burst is what a rate limiter watches for.
const PACE: Duration = Duration::from_millis(500);

// What persists between runs. One peer per login, so the reply address and
// the context token are single slots.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct State {
    token: Option<String>,
    base_url: String,
    peer: Option<String>,
    get_updates_buf: String,
    context_token: Option<String>,
}

// Who a reply goes to, read once under the lock.
struct Target {
    token: String,
    peer: String,
    context_token: Option<String>,
}

// The state and where it lives, locked as one so a save sees what the
// holder of the lock just wrote. Never held across an await.
struct Kept {
    path: PathBuf,
    state: Mutex<State>,
}

impl Kept {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn target(&self) -> Option<Target> {
        let s = self.lock();
        Some(Target {
            token: s.token.clone()?,
            peer: s.peer.clone()?,
            context_token: s.context_token.clone(),
        })
    }

    // It carries the session token: written private, then renamed in whole.
    fn save(&self, state: &State) {
        let Ok(body) = serde_json::to_vec_pretty(state) else {
            return;
        };
        let tmp = self.path.with_extension("json.pi-tmp");
        let written = std::fs::write(&tmp, body)
            .and_then(|()| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            })
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        if let Err(e) = written {
            eprintln!("wechat: the login could not be saved: {e}");
        }
    }
}

// pi's end of fd 3, written by one task so a plain callback can say things.
#[derive(Clone)]
struct Pi(mpsc::UnboundedSender<Value>);

impl Pi {
    fn say(&self, line: Value) {
        let _ = self.0.send(line);
    }

    fn notice(&self, text: &str) {
        self.say(json!({ "notice": text }));
    }
}

/// Run as pi's job until the login fails or expires, or pi goes. The error
/// is what pi reports back to the model when the job ends.
pub async fn run() -> Result<(), String> {
    if std::env::var("PI_EVENTS_FD").as_deref() != Ok("3") {
        return Err("pi-wechat runs as a pi tool: there is no fd 3 here".into());
    }
    let mut args = String::new();
    let _ = tokio::io::stdin().read_to_string(&mut args).await;
    let path = state_path().ok_or("wechat: neither PI_HOME nor HOME is set")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("wechat: {}: {e}", dir.display()))?;
    }
    let state: State = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    // SAFETY: PI_EVENTS_FD says pi opened fd 3 for this process, and
    // nothing else here takes it.
    let fd3 = unsafe { std::os::unix::net::UnixStream::from_raw_fd(3) };
    fd3.set_nonblocking(true).map_err(|e| e.to_string())?;
    let (read, mut write) = tokio::net::UnixStream::from_std(fd3)
        .map_err(|e| e.to_string())?
        .into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
    });
    let pi = Pi(tx);

    let logged_in = state.token.is_some();
    pi.say(json!({ "detach": if logged_in {
        "connecting to WeChat with the saved login; messages from the phone will arrive here as turns"
    } else {
        "logging in to WeChat: a QR is on the screen for the user to scan; once they do, messages from the phone arrive here as turns"
    }}));
    let mut client = Client::new(base_of(&state));
    let kept = Kept {
        path,
        state: Mutex::new(state),
    };
    if !logged_in {
        login(&mut client, &kept, &pi).await?;
    }
    pi.say(json!({ "status": "connected" }));
    tokio::select! {
        r = poll(&client, &kept, &pi) => r,
        () = answer(&client, &kept, &pi, read) => Ok(()),
    }
}

// Where pi keeps its own files, so a restart resumes without a new scan.
fn state_path() -> Option<PathBuf> {
    std::env::var_os("PI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
        .map(|root| root.join("wechat.json"))
}

fn base_of(state: &State) -> String {
    if state.base_url.is_empty() {
        crate::DEFAULT_BASE_URL.to_string()
    } else {
        state.base_url.clone()
    }
}

// QR → confirm → credentials. The QR and progress go to the screen, where
// they stay long enough to be scanned; the model never reads them.
async fn login(client: &mut Client, kept: &Kept, pi: &Pi) -> Result<(), String> {
    pi.say(json!({ "status": "waiting for the QR to be scanned" }));
    let (qr, note) = (pi.clone(), pi.clone());
    let mut view = crate::LoginView {
        show_qr: Box::new(move |rendered: &str| qr.notice(rendered)),
        notice: Box::new(move |text: &str| note.notice(text)),
    };
    let credentials = crate::login_flow(client, &mut view)
        .await
        .map_err(|e| e.to_string())?;
    let mut s = kept.lock();
    *s = State {
        token: Some(credentials.token),
        base_url: credentials.base_url,
        ..State::default()
    };
    kept.save(&s);
    Ok(())
}

// Client-side timeouts are the normal empty result; real errors back off
// (the reference's 2s/30s rhythm); a stale token ends the job.
async fn poll(client: &Client, kept: &Kept, pi: &Pi) -> Result<(), String> {
    let mut failures = 0u32;
    let mut timeout = crate::client::LONG_POLL_TIMEOUT;
    loop {
        let (token, buf) = {
            let s = kept.lock();
            (s.token.clone(), s.get_updates_buf.clone())
        };
        let token = token.ok_or("wechat: no login")?;
        let failed = match client.get_updates(&token, &buf, timeout).await {
            Ok(update) if update.is_stale_token() => {
                let mut s = kept.lock();
                s.token = None;
                kept.save(&s);
                return Err("the WeChat login expired; call wechat again for a new QR".into());
            }
            Ok(update) if update.is_error() => format!(
                "ret={} errcode={:?} errmsg={:?}",
                update.ret, update.errcode, update.errmsg
            ),
            Ok(update) => {
                failures = 0;
                if let Some(t) = update.longpolling_timeout_ms.filter(|t| *t > 0) {
                    timeout = Duration::from_millis(t);
                }
                heard(kept, pi, update);
                continue;
            }
            Err(e) => format!("{e:#}"),
        };
        failures += 1;
        if failures == 3 {
            pi.notice(&format!(
                "wechat getupdates failing — backing off: {failed}"
            ));
        }
        tokio::time::sleep(backoff(&mut failures)).await;
    }
}

// Lines go up as input, `/stop` as an interrupt; the reply address moves to
// whoever spoke last. Saved only when something changed.
fn heard(kept: &Kept, pi: &Pi, update: Update) {
    let mut s = kept.lock();
    let before = s.clone();
    if !update.get_updates_buf.is_empty() {
        s.get_updates_buf = update.get_updates_buf;
    }
    for msg in update.msgs {
        if msg.message_type != 1 {
            continue;
        }
        let text = crate::text_of(&msg);
        if text.is_empty() {
            continue;
        }
        if matches!(text.trim(), "/stop" | "/esc") {
            pi.say(json!({ "interrupt": true }));
            continue;
        }
        s.peer = Some(msg.from_user_id.clone());
        // Keep the last valid token: a tokenless message must not erase it
        // (the reference stores only when one is present).
        if let Some(token) = &msg.context_token {
            s.context_token = Some(token.clone());
        }
        pi.say(json!({ "input": text }));
    }
    if *s != before {
        kept.save(&s);
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

// Typing while a turn the phone opened runs, then its answer, in order.
// Ends when pi does: there is nobody left to bridge to.
async fn answer(client: &Client, kept: &Kept, pi: &Pi, read: OwnedReadHalf) {
    let mut lines = BufReader::new(read).lines();
    let mut tickets = Tickets::default();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(told) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(to) = kept.target() else {
            continue;
        };
        if told.get("started").and_then(Value::as_bool) == Some(true) {
            tickets.typing(client, &to, true).await;
        } else if let Some(reply) = told.get("reply").and_then(Value::as_str) {
            tickets.typing(client, &to, false).await;
            send(client, &to, pi, reply).await;
        }
    }
}

async fn send(client: &Client, to: &Target, pi: &Pi, reply: &str) {
    let text = format_markdown(reply);
    if text.trim().is_empty() {
        return;
    }
    let context_token = to.context_token.as_deref().unwrap_or_default();
    let pieces = split(&text, MESSAGE_LIMIT);
    let total = pieces.len();
    for (i, piece) in pieces.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(PACE).await;
        }
        let sent = client
            .send_text(&to.token, &to.peer, context_token, piece)
            .await;
        // A later piece without the ones before it reads as garbage, so a
        // failed send ends the message rather than skipping a hole.
        if let Err(e) = sent {
            let part = if total > 1 {
                format!(" (piece {}/{total})", i + 1)
            } else {
                String::new()
            };
            // Only a reply with no conversation window to ride is fixed by the phone.
            let hint = if to.context_token.is_none() {
                " — try sending a message from the phone first"
            } else {
                ""
            };
            pi.notice(&format!("wechat send failed{part}: {e:#}{hint}"));
            return;
        }
    }
}

// Typing tickets, one peer's at a time: fetched once, kept until a send
// with it fails. A peer the server gave none to is remembered as such.
#[derive(Default)]
struct Tickets {
    held: Option<(String, Option<String>)>,
}

impl Tickets {
    // Best effort: no ticket, no indicator.
    async fn typing(&mut self, client: &Client, to: &Target, on: bool) {
        let known = self.held.as_ref().filter(|(peer, _)| *peer == to.peer);
        let ticket = match known {
            Some((_, ticket)) => ticket.clone(),
            None => {
                let fetched = fetch_ticket(client, to).await;
                self.held = Some((to.peer.clone(), fetched.clone()));
                fetched
            }
        };
        let Some(ticket) = ticket else {
            return;
        };
        let status = if on { 1 } else { 2 };
        if client
            .send_typing(&to.token, &to.peer, &ticket, status)
            .await
            .is_err()
        {
            self.held = None;
        }
    }
}

async fn fetch_ticket(client: &Client, to: &Target) -> Option<String> {
    let context_token = to.context_token.as_deref().unwrap_or_default();
    let config = client
        .get_config(&to.token, &to.peer, context_token)
        .await
        .ok()?;
    config.typing_ticket.filter(|t| !t.is_empty())
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

    fn update(msgs: Value, buf: &str) -> Update {
        serde_json::from_value(json!({ "ret": 0, "msgs": msgs, "get_updates_buf": buf })).unwrap()
    }

    fn kept() -> (tempfile::TempDir, Kept) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wechat.json");
        (
            dir,
            Kept {
                path,
                state: Mutex::new(State::default()),
            },
        )
    }

    fn msg(text: &str, ctx: Option<&str>) -> Value {
        json!({
            "from_user_id": "u1",
            "message_type": 1,
            "context_token": ctx,
            "item_list": [{ "type": 1, "text_item": { "text": text } }],
        })
    }

    // A person's line goes up as input and moves the reply address; `/stop`
    // goes up as an interrupt; the cursor and the address are saved.
    #[test]
    fn what_the_phone_says_goes_up_and_who_said_it_is_kept() {
        let (_dir, kept) = kept();
        let (tx, mut rx) = mpsc::unbounded_channel();
        heard(
            &kept,
            &Pi(tx),
            update(
                json!([msg("why did it fail", Some("c1")), msg("/stop", None)]),
                "b2",
            ),
        );
        assert_eq!(
            rx.try_recv().unwrap(),
            json!({ "input": "why did it fail" })
        );
        assert_eq!(rx.try_recv().unwrap(), json!({ "interrupt": true }));
        let saved: State = serde_json::from_slice(&std::fs::read(&kept.path).unwrap()).unwrap();
        assert_eq!(
            (
                saved.peer.as_deref(),
                saved.context_token.as_deref(),
                saved.get_updates_buf.as_str()
            ),
            (Some("u1"), Some("c1"), "b2")
        );
    }

    // A poll that changes nothing leaves the file alone.
    #[test]
    fn an_unchanged_poll_writes_nothing() {
        let (_dir, kept) = kept();
        let (tx, _rx) = mpsc::unbounded_channel();
        heard(&kept, &Pi(tx), update(json!([]), ""));
        assert!(!kept.path.exists());
    }

    #[test]
    fn the_saved_login_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, kept) = kept();
        kept.save(&State::default());
        let mode = std::fs::metadata(&kept.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
