---
[package]
edition = "2024"
description = "Bridge this session to Telegram: what the user sends to their bot arrives here as turns, and each answer goes back to the chat. Use when the user asks to connect Telegram. It needs TELEGRAM_BOT_TOKEN, a bot token from @BotFather, in the tools' .env. It runs in the background until `jobs` stops it; the first time, a link and a QR show in its job for the user to pair their account, and the pairing is kept for next time."

[dependencies]
pulldown-cmark = { version = "0.13", default-features = false }
qrcode = { version = "0.14", default-features = false }
rand = "0.8"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["macros", "time", "sync", "net", "io-util", "io-std", "rt"] }

[dev-dependencies]
tempfile = "3"
---
//! Telegram for pi: a Bot API client (HTTPS long-poll), and the tool that
//! bridges a session to one private chat with the bot.
//!
//! `api` knows only the wire; `render` turns markdown into text plus
//! entities; `bridge` owns the saved pairing and what a message means.
//!
//! A cargo script pi runs as a tool: copy it into `~/.pi/tools/`. Its own
//! tests run with `cargo +nightly test -Zscript --manifest-path telegram.rs`.

use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match bridge::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    }
}

mod bridge {
    //! telegram as a pi job: what the paired account sends goes up fd 3 as
    //! `input` (or `interrupt` for `/stop`). It follows, so every turn on the
    //! checkout comes back as `started`, then one `reply`, sent to the chat;
    //! each question put there comes too, its options as buttons: a press
    //! answers with that one, a reply of numbers only with several.
    //!
    //! Anyone can message a bot, and a turn here can run commands, so only
    //! the account that sent the pairing code shown in the job is heard.

    use std::collections::BTreeSet;
    use std::os::fd::FromRawFd;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::unix::OwnedReadHalf;
    use tokio::sync::mpsc;

    use crate::api::{Button, Click, Client, Entity, Update};
    use crate::render::{pieces, render};

    // Bytes, so a piece is within Telegram's 4096 UTF-16 units whatever the script.
    const MESSAGE_LIMIT: usize = 4096;
    // Telegram asks for no more than one message a second in one chat.
    const PACE: Duration = Duration::from_secs(1);
    // The indicator lapses after 5 s unless renewed.
    const TYPING_EVERY: Duration = Duration::from_secs(4);

    #[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    struct State {
        // The bot the pairing belongs to: a new token starts over.
        bot: Option<i64>,
        offset: i64,
        user: Option<i64>,
        chat: Option<i64>,
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

        fn chat(&self) -> Option<i64> {
            self.lock().chat
        }

        // Whoever can write it decides who drives the session: kept private.
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
                eprintln!("telegram: the pairing could not be saved: {e}");
            }
        }
    }

    // The questions up on the checkout, as this chat shows them. Never held
    // across an await.
    #[derive(Default)]
    struct Asking(Mutex<Vec<Up>>);

    impl Asking {
        fn lock(&self) -> MutexGuard<'_, Vec<Up>> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    // Where a question is with this chat.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Stage {
        // Not sent yet: one asked while a turn runs waits for that turn's
        // reply, which it follows on from.
        Held,
        // On its way to the chat or there.
        Shown,
        // Its answer went up; pi closes it soon.
        Answered,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Up {
        id: u64,
        text: String,
        options: Vec<String>,
        stage: Stage,
        // The message showing it, once sent.
        message: Option<i64>,
    }

    impl Up {
        // `{"question": {...}}` as pi tells it.
        fn of(told: &Value) -> Option<Self> {
            let q = told.get("question")?;
            let options = (q.get("options")?.as_array()?.iter())
                .map(|o| o.as_str().map(str::to_string))
                .collect::<Option<_>>()?;
            Some(Self {
                id: q.get("id")?.as_u64()?,
                text: q.get("text")?.as_str()?.to_string(),
                options,
                stage: Stage::Held,
                message: None,
            })
        }

        // In full, since a button cuts a long option short.
        fn shown(&self) -> String {
            let options = (self.options.iter().enumerate())
                .map(|(i, o)| format!("{}. {o}", i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            let how = "tap one, or reply with numbers such as 1 3 for several";
            format!("? {}\n\n{options}\n\n({how})", self.text)
        }

        // A row per option, then the one that closes it unanswered.
        fn buttons(&self) -> Vec<Vec<Button>> {
            let button = |text: String, what: &str| Button {
                text,
                callback_data: format!("{}:{what}", self.id),
            };
            (self.options.iter().enumerate())
                .map(|(i, o)| vec![button(format!("{}. {o}", i + 1), &i.to_string())])
                .chain([vec![button("close".into(), "close")]])
                .collect()
        }
    }

    const CLOSED: &str = "this question is closed";

    // A press goes up as the answer: its option, or none to close it. One on
    // what is no longer up, or already answered, says why not.
    fn pressed(asking: &Asking, pi: &Pi, data: &str) -> Result<(), &'static str> {
        let (id, what) = data.split_once(':').ok_or(CLOSED)?;
        let id: u64 = id.parse().map_err(|_| CLOSED)?;
        let mut up = asking.lock();
        let q = up.iter_mut().find(|q| q.id == id).ok_or(CLOSED)?;
        if q.stage == Stage::Answered {
            return Err("already answered");
        }
        let picks: Vec<usize> = match what {
            "close" => Vec::new(),
            at => {
                let at = (at.parse().ok())
                    .filter(|at| *at < q.options.len())
                    .ok_or(CLOSED)?;
                vec![at]
            }
        };
        q.stage = Stage::Answered;
        pi.say(json!({ "answer": id, "picks": picks }));
        Ok(())
    }

    async fn press(client: &Client, asking: &Asking, pi: &Pi, click: Click) {
        let said = pressed(asking, pi, click.data.as_deref().unwrap_or_default());
        let _ = client.settle(&click.id, said.err()).await;
    }

    // A line of option numbers only answers the latest question shown and not
    // yet answered, by place; anything else is the person's own words.
    fn picks_of(asking: &Asking, line: &str) -> Option<(u64, Vec<usize>)> {
        let mut up = asking.lock();
        let q = up.iter_mut().rev().find(|q| q.stage == Stage::Shown)?;
        let picks = (line.split(|c: char| c.is_whitespace() || ",，、".contains(c)))
            .filter(|w| !w.is_empty())
            .map(|w| {
                (w.parse::<usize>().ok())
                    .filter(|n| (1..=q.options.len()).contains(n))
                    .map(|n| n - 1)
            })
            .collect::<Option<BTreeSet<_>>>()?;
        if picks.is_empty() {
            return None;
        }
        q.stage = Stage::Answered;
        Some((q.id, picks.into_iter().collect()))
    }

    // Questions not yet shown go out, marked first so the other task does
    // not send them too; one closed while it went has its buttons taken
    // away again.
    async fn show_held(client: &Client, chat: i64, asking: &Asking, pi: &Pi) {
        let held: Vec<Up> = (asking.lock().iter_mut())
            .filter(|q| q.stage == Stage::Held)
            .map(|q| {
                q.stage = Stage::Shown;
                q.clone()
            })
            .collect();
        for q in held {
            match client.send_buttons(chat, &q.shown(), &q.buttons()).await {
                Ok(message) => {
                    let kept = (asking.lock().iter_mut())
                        .find(|u| u.id == q.id)
                        .map(|u| u.message = Some(message))
                        .is_some();
                    if !kept {
                        let _ = client.set_buttons(chat, message, &[]).await;
                    }
                }
                Err(e) => {
                    // Answered while it went stays answered, not sent again.
                    if let Some(u) = asking.lock().iter_mut().find(|u| u.id == q.id)
                        && u.stage == Stage::Shown
                    {
                        u.stage = Stage::Held;
                    }
                    pi.status(&format!("telegram could not show a question: {e}"));
                }
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

        fn status(&self, text: &str) {
            self.say(json!({ "status": text }));
        }
    }

    /// Run as pi's job until the token is refused, another client takes the
    /// updates, or pi goes. The error is what pi reports back to the model.
    pub async fn run() -> Result<(), String> {
        if std::env::var("PI_EVENTS_FD").as_deref() != Ok("3") {
            return Err("telegram runs as a pi tool: there is no fd 3 here".into());
        }
        let mut args = String::new();
        let _ = tokio::io::stdin().read_to_string(&mut args).await;
        let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
        let token = token.trim();
        if token.is_empty() {
            return Err(
                "telegram: TELEGRAM_BOT_TOKEN is not set — create a bot with @BotFather \
                        and put its token in ~/.pi/tools/.env"
                    .into(),
            );
        }
        let client = Client::new(token);
        let me = client.get_me().await.map_err(|e| e.to_string())?;
        let bot = me.username.unwrap_or_default();

        let path = state_path().ok_or("telegram: neither PI_HOME nor HOME is set")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("telegram: {}: {e}", dir.display()))?;
        }
        let mut state: State = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if state.bot != Some(me.id) {
            state = State {
                bot: Some(me.id),
                ..State::default()
            };
        }

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

        let mut code = None;
        if state.user.is_some() {
            pi.say(json!({ "detach": format!(
                "connected to Telegram as @{bot}; messages from the paired account will arrive here as turns"
            )}));
            pi.say(json!({ "status": "connected" }));
        } else {
            pi.say(json!({ "detach": format!(
                "waiting for the user to pair @{bot}: a link and a QR are in this job; once they open it, messages from their account arrive here as turns"
            )}));
            let fresh = format!("{:08}", rand::random::<u32>() % 100_000_000);
            show_pairing(&pi, &bot, &fresh);
            pi.say(json!({ "status": "waiting for pairing" }));
            code = Some(fresh);
        }
        pi.say(json!({ "follow": true }));
        let kept = Kept {
            path,
            state: Mutex::new(state),
        };
        let asking = Asking::default();
        tokio::select! {
            r = poll(&client, &kept, &asking, &pi, code) => r,
            () = answer(&client, &kept, &asking, &pi, read) => Ok(()),
        }
    }

    // Where pi keeps its own files, so a restart resumes without pairing again.
    fn state_path() -> Option<PathBuf> {
        std::env::var_os("PI_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
            .map(|root| root.join("telegram.json"))
    }

    // The deep link sends `/start <code>` by itself; the QR lets a phone camera
    // open it. Both on stderr, read by opening the job; the model reads neither.
    fn show_pairing(pi: &Pi, bot: &str, code: &str) {
        let link = format!("https://t.me/{bot}?start={code}");
        pi.status("pair Telegram with this session: the link and QR are in this job");
        eprintln!("open {link} and press Start, or send `/start {code}` to @{bot}");
        if let Some(qr) = render_qr(&link) {
            eprintln!("{qr}");
        }
    }

    fn render_qr(url: &str) -> Option<String> {
        use qrcode::render::unicode;
        let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
        Some(code.render::<unicode::Dense1x2>().build())
    }

    // A refused token or a second reader ends the job; anything else backs off.
    async fn poll(
        client: &Client,
        kept: &Kept,
        asking: &Asking,
        pi: &Pi,
        mut code: Option<String>,
    ) -> Result<(), String> {
        let mut failures = 0u32;
        loop {
            let offset = kept.lock().offset;
            let failed = match client.get_updates(offset).await {
                Ok(updates) => {
                    failures = 0;
                    let (paired, clicks) = heard(kept, asking, pi, &mut code, updates);
                    if let Some(chat) = paired {
                        let hello = "connected to pi: what you send here arrives as turns, and /stop stops one";
                        let _ = client.send_message(chat, hello, &[]).await;
                        show_held(client, chat, asking, pi).await;
                    }
                    for click in clicks {
                        press(client, asking, pi, click).await;
                    }
                    continue;
                }
                Err(e) if e.code() == Some(401) => {
                    return Err("telegram refused the bot token; check TELEGRAM_BOT_TOKEN".into());
                }
                Err(e) if e.code() == Some(409) => {
                    return Err(format!(
                        "{e} — another client reads this bot's updates (another pi, or a webhook); stop it, then call telegram again"
                    ));
                }
                Err(e) => e,
            };
            failures += 1;
            if failures == 3 {
                pi.status(&format!(
                    "telegram getUpdates failing — backing off: {failed}"
                ));
            }
            let rest = backoff(&mut failures);
            let wait = failed.retry_after().unwrap_or(rest);
            tokio::time::sleep(wait).await;
        }
    }

    // Moves the offset past every update; only the paired account is heard.
    // Returns the chat that just paired, to be greeted, and its presses on
    // buttons in that chat. Saved only on change.
    fn heard(
        kept: &Kept,
        asking: &Asking,
        pi: &Pi,
        code: &mut Option<String>,
        updates: Vec<Update>,
    ) -> (Option<i64>, Vec<Click>) {
        let mut s = kept.lock();
        let before = s.clone();
        let mut paired = None;
        let mut clicks = Vec::new();
        for update in updates {
            s.offset = s.offset.max(update.update_id + 1);
            if let Some(click) = update.callback_query {
                let chat = click.message.as_ref().map(|m| m.chat.id);
                if s.user == Some(click.from.id) && chat.is_some() && chat == s.chat {
                    clicks.push(click);
                }
                continue;
            }
            let Some(msg) = update.message else {
                continue;
            };
            let Some(from) = msg.from else {
                continue;
            };
            let text = msg.text.unwrap_or_default();
            let line = text.trim();
            match s.user {
                None => {
                    let Some(want) = code.as_deref() else {
                        continue;
                    };
                    let private = msg.chat.kind == "private";
                    if private && (line == want || line == format!("/start {want}")) {
                        s.user = Some(from.id);
                        s.chat = Some(msg.chat.id);
                        *code = None;
                        paired = Some(msg.chat.id);
                        let who = from
                            .username
                            .map_or(from.id.to_string(), |u| format!("@{u}"));
                        pi.status(&format!("connected, paired with {who}"));
                    }
                }
                Some(user) if user == from.id && s.chat == Some(msg.chat.id) => {
                    if line.is_empty() {
                        continue;
                    }
                    match command(line) {
                        Some("stop" | "esc") => pi.say(json!({ "interrupt": true })),
                        Some("start") => {}
                        _ => match picks_of(asking, line) {
                            Some((id, picks)) => pi.say(json!({ "answer": id, "picks": picks })),
                            None => pi.say(json!({ "input": text })),
                        },
                    }
                }
                Some(_) => {}
            }
        }
        if *s != before {
            kept.save(&s);
        }
        (paired, clicks)
    }

    // `/stop@some_bot extra` → `stop`; a line that is not a command → None.
    fn command(line: &str) -> Option<&str> {
        let word = line.strip_prefix('/')?.split_whitespace().next()?;
        Some(word.split('@').next().unwrap_or(word))
    }

    // 2s between ordinary retries, 30s once three have failed in a row; the
    // counter resets on the 30s step.
    fn backoff(failures: &mut u32) -> Duration {
        if *failures >= 3 {
            *failures = 0;
            Duration::from_secs(30)
        } else {
            Duration::from_secs(2)
        }
    }

    // Typing while a turn runs, then its answer, then the questions it left
    // up. Ends when pi does.
    async fn answer(client: &Client, kept: &Kept, asking: &Asking, pi: &Pi, read: OwnedReadHalf) {
        let mut lines = BufReader::new(read).lines();
        let mut typing: Option<i64> = None;
        let mut tick = tokio::time::interval(TYPING_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else {
                        return;
                    };
                    let Ok(told) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    // Kept even before pairing: what is up then shows once paired.
                    let mut shows = false;
                    let mut gone = None;
                    if let Some(up) = Up::of(&told) {
                        asking.lock().push(up);
                        shows = typing.is_none();
                    } else if let Some(id) = told.get("closed").and_then(Value::as_u64) {
                        let mut up = asking.lock();
                        let at = up.iter().position(|q| q.id == id);
                        gone = at.and_then(|at| up.remove(at).message);
                    }
                    // Not taken: it may be answered again, and the person hears why.
                    let refused = told.get("refused").and_then(Value::as_u64).map(|id| {
                        if let Some(q) = asking.lock().iter_mut().find(|q| q.id == id)
                            && q.stage == Stage::Answered
                        {
                            q.stage = Stage::Shown;
                        }
                        told.get("why").and_then(Value::as_str).unwrap_or_default().to_string()
                    });
                    let Some(chat) = kept.chat() else {
                        continue;
                    };
                    if let Some(why) = refused {
                        send(client, chat, pi, &format!("not taken: {why}")).await;
                    }
                    if told.get("started").and_then(Value::as_bool) == Some(true) {
                        typing = Some(chat);
                        tick.reset_immediately();
                    } else if let Some(reply) = told.get("reply").and_then(Value::as_str) {
                        typing = None;
                        send(client, chat, pi, reply).await;
                        shows = true;
                    }
                    if let Some(message) = gone {
                        let _ = client.set_buttons(chat, message, &[]).await;
                    }
                    if shows {
                        show_held(client, chat, asking, pi).await;
                    }
                }
                _ = tick.tick(), if typing.is_some() => {
                    if let Some(chat) = typing {
                        let _ = client.send_typing(chat).await;
                    }
                }
            }
        }
    }

    async fn send(client: &Client, chat: i64, pi: &Pi, reply: &str) {
        let (text, spans) = render(reply);
        let pieces = pieces(&text, &spans, MESSAGE_LIMIT);
        let total = pieces.len();
        for (i, (piece, entities)) in pieces.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(PACE).await;
            }
            // A later piece without the ones before it reads as garbage, so a
            // failed send ends the message rather than skipping a hole.
            if let Err(e) = deliver(client, chat, piece, entities).await {
                let part = if total > 1 {
                    format!(" (piece {}/{total})", i + 1)
                } else {
                    String::new()
                };
                pi.status(&format!("telegram send failed{part}: {e}"));
                return;
            }
        }
    }

    async fn deliver(
        client: &Client,
        chat: i64,
        text: &str,
        entities: &[Entity],
    ) -> Result<(), crate::api::Error> {
        let mut waited = 0;
        loop {
            let e = match client.send_message(chat, text, entities).await {
                Ok(()) => return Ok(()),
                Err(e) => e,
            };
            if let Some(wait) = e.retry_after().filter(|_| waited < 3) {
                waited += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            // Formatting the server will not take must not lose the words.
            if e.code() == Some(400) && !entities.is_empty() {
                return client.send_message(chat, text, &[]).await;
            }
            return Err(e);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn kept() -> (tempfile::TempDir, Kept) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("telegram.json");
            (
                dir,
                Kept {
                    path,
                    state: Mutex::new(State::default()),
                },
            )
        }

        fn update(id: i64, from: i64, chat: i64, kind: &str, text: &str) -> Update {
            serde_json::from_value(json!({
                "update_id": id,
                "message": {
                    "from": { "id": from },
                    "chat": { "id": chat, "type": kind },
                    "text": text,
                },
            }))
            .unwrap()
        }

        // A stranger is never heard, before or after pairing; only the
        // private chat that sent the code pairs, and then drives the session.
        #[test]
        fn only_the_account_that_paired_is_heard() {
            let (_dir, kept) = kept();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let pi = Pi(tx);
            let mut code = Some("12345678".to_string());
            let asking = Asking::default();
            let (paired, _) = heard(
                &kept,
                &asking,
                &pi,
                &mut code,
                vec![
                    update(1, 9, 9, "private", "rm -rf ~"),
                    update(2, 7, -100, "group", "/start 12345678"),
                    update(3, 7, 7, "private", "/start 12345678"),
                ],
            );
            assert_eq!(paired, Some(7));
            assert_eq!(code, None);
            assert!(rx.try_recv().unwrap().get("status").is_some());
            assert!(rx.try_recv().is_err());

            heard(
                &kept,
                &asking,
                &pi,
                &mut code,
                vec![
                    update(4, 9, 9, "private", "rm -rf ~"),
                    update(5, 7, 7, "private", "why did it fail"),
                    update(6, 7, 7, "private", "/stop@pi_bot"),
                ],
            );
            assert_eq!(
                rx.try_recv().unwrap(),
                json!({ "input": "why did it fail" })
            );
            assert_eq!(rx.try_recv().unwrap(), json!({ "interrupt": true }));
            assert!(rx.try_recv().is_err());
            let saved: State = serde_json::from_slice(&std::fs::read(&kept.path).unwrap()).unwrap();
            assert_eq!(
                (saved.user, saved.chat, saved.offset),
                (Some(7), Some(7), 7)
            );
        }

        // Only the paired account's presses, in its chat, count; a press
        // answers with its option, or closes, once; one on what is no longer
        // up says so. A reply of numbers only answers with several.
        #[test]
        fn a_press_answers_only_for_the_paired_account() {
            let (_dir, kept) = kept();
            *kept.lock() = State {
                user: Some(7),
                chat: Some(7),
                ..State::default()
            };
            let (tx, mut rx) = mpsc::unbounded_channel();
            let pi = Pi(tx);
            let asking = Asking::default();
            let click = |id: i64, from: i64, chat: i64, data: &str| -> Update {
                serde_json::from_value(json!({
                    "update_id": id,
                    "callback_query": {
                        "id": "c",
                        "from": { "id": from },
                        "message": { "message_id": 1, "chat": { "id": chat, "type": "private" } },
                        "data": data,
                    },
                }))
                .unwrap()
            };
            let (_, clicks) = heard(
                &kept,
                &asking,
                &pi,
                &mut None,
                vec![
                    click(1, 9, 7, "3:0"),
                    click(2, 7, -100, "3:0"),
                    click(3, 7, 7, "3:0"),
                ],
            );
            assert_eq!(clicks.len(), 1);
            assert!(rx.try_recv().is_err(), "a press says nothing by itself");

            let told = |id: u64| {
                json!({ "question": {
                    "id": id, "text": "which?", "options": ["a", "b", "c"], "back": false,
                }})
            };
            asking.lock().push(Up::of(&told(3)).unwrap());
            assert_eq!(pressed(&asking, &pi, "3:3"), Err(CLOSED));
            assert_eq!(pressed(&asking, &pi, "3:1"), Ok(()));
            assert_eq!(rx.try_recv().unwrap(), json!({ "answer": 3, "picks": [1] }));
            assert!(pressed(&asking, &pi, "3:close").is_err(), "answered once");
            asking.lock().push(Up::of(&told(5)).unwrap());
            assert_eq!(pressed(&asking, &pi, "5:close"), Ok(()));
            assert_eq!(rx.try_recv().unwrap(), json!({ "answer": 5, "picks": [] }));
            assert_eq!(pressed(&asking, &pi, "9:0"), Err(CLOSED));

            let mut shown = Up::of(&told(6)).unwrap();
            shown.stage = Stage::Shown;
            asking.lock().push(shown);
            let says = |text: &str| {
                heard(
                    &kept,
                    &asking,
                    &pi,
                    &mut None,
                    vec![update(9, 7, 7, "private", text)],
                );
            };
            says("3，1 3");
            assert_eq!(
                rx.try_recv().unwrap(),
                json!({ "answer": 6, "picks": [0, 2] })
            );
            says("1 more");
            assert_eq!(rx.try_recv().unwrap(), json!({ "input": "1 more" }));
        }

        #[test]
        fn the_saved_pairing_is_private() {
            use std::os::unix::fs::PermissionsExt;
            let (_dir, kept) = kept();
            kept.save(&State::default());
            let mode = std::fs::metadata(&kept.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

mod api {
    //! The Bot API calls the bridge uses. Protocol only — no pairing, no
    //! persistence. The token rides in every URL, so no error carries one.

    use std::time::Duration;

    use serde::de::{DeserializeOwned, IgnoredAny};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    // How long the server holds an empty getUpdates; the client waits a margin more.
    const POLL_HOLD_SECS: u64 = 30;
    const POLL_TIMEOUT: Duration = Duration::from_secs(POLL_HOLD_SECS + 10);
    const API_TIMEOUT: Duration = Duration::from_secs(15);

    #[derive(Debug, thiserror::Error)]
    pub enum Error {
        #[error("telegram {method}: {code} {description}")]
        Api {
            method: &'static str,
            code: i64,
            description: String,
            retry_after: Option<u64>,
        },
        #[error("telegram {method}: {source}")]
        Http {
            method: &'static str,
            source: reqwest::Error,
        },
        #[error("telegram {method}: {source}")]
        Json {
            method: &'static str,
            source: serde_json::Error,
        },
    }

    impl Error {
        pub fn code(&self) -> Option<i64> {
            match self {
                Error::Api { code, .. } => Some(*code),
                _ => None,
            }
        }

        pub fn retry_after(&self) -> Option<Duration> {
            match self {
                Error::Api {
                    retry_after: Some(s),
                    ..
                } => Some(Duration::from_secs(*s)),
                _ => None,
            }
        }
    }

    pub type Result<T> = std::result::Result<T, Error>;

    #[derive(Deserialize)]
    struct Reply<T> {
        ok: bool,
        #[serde(default = "Option::default")]
        result: Option<T>,
        #[serde(default)]
        error_code: i64,
        #[serde(default)]
        description: String,
        #[serde(default)]
        parameters: Option<Parameters>,
    }

    #[derive(Deserialize)]
    struct Parameters {
        retry_after: Option<u64>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Update {
        pub update_id: i64,
        #[serde(default)]
        pub message: Option<Message>,
        #[serde(default)]
        pub callback_query: Option<Click>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Message {
        #[serde(default)]
        pub message_id: i64,
        #[serde(default)]
        pub from: Option<User>,
        pub chat: Chat,
        #[serde(default)]
        pub text: Option<String>,
    }

    /// A press on a button under a message the bot sent.
    #[derive(Debug, Deserialize)]
    pub struct Click {
        pub id: String,
        pub from: User,
        #[serde(default)]
        pub message: Option<Message>,
        #[serde(default)]
        pub data: Option<String>,
    }

    /// One button: what it says, and what a press on it sends back.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct Button {
        pub text: String,
        pub callback_data: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct Chat {
        pub id: i64,
        #[serde(rename = "type", default)]
        pub kind: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct User {
        pub id: i64,
        #[serde(default)]
        pub username: Option<String>,
    }

    /// One formatting run; `offset` and `length` count UTF-16 code units.
    #[derive(Debug, Clone, PartialEq, Serialize)]
    pub struct Entity {
        #[serde(rename = "type")]
        pub kind: &'static str,
        pub offset: usize,
        pub length: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub language: Option<String>,
    }

    pub struct Client {
        http: reqwest::Client,
        base: String,
    }

    impl Client {
        pub fn new(token: &str) -> Self {
            Self {
                http: reqwest::Client::new(),
                base: format!("https://api.telegram.org/bot{token}"),
            }
        }

        pub async fn get_me(&self) -> Result<User> {
            self.call("getMe", json!({}), API_TIMEOUT).await
        }

        /// Long-poll for messages from `offset` on. A client timeout is the
        /// ordinary empty result, not an error.
        pub async fn get_updates(&self, offset: i64) -> Result<Vec<Update>> {
            let body = json!({
                "offset": offset,
                "timeout": POLL_HOLD_SECS,
                "allowed_updates": ["message", "callback_query"],
            });
            match self.call("getUpdates", body, POLL_TIMEOUT).await {
                Err(Error::Http { source, .. }) if source.is_timeout() => Ok(Vec::new()),
                r => r,
            }
        }

        pub async fn send_message(&self, chat: i64, text: &str, entities: &[Entity]) -> Result<()> {
            let body = json!({
                "chat_id": chat,
                "text": text,
                "entities": entities,
                "link_preview_options": { "is_disabled": true },
            });
            self.call::<IgnoredAny>("sendMessage", body, API_TIMEOUT)
                .await
                .map(|_| ())
        }

        /// Send `text` with rows of buttons under it: the message's id.
        pub async fn send_buttons(
            &self,
            chat: i64,
            text: &str,
            rows: &[Vec<Button>],
        ) -> Result<i64> {
            let body = json!({
                "chat_id": chat,
                "text": text,
                "reply_markup": { "inline_keyboard": rows },
            });
            let sent: Message = self.call("sendMessage", body, API_TIMEOUT).await?;
            Ok(sent.message_id)
        }

        /// Put `rows` of buttons under message `id` in place of its own; none
        /// takes them away.
        pub async fn set_buttons(&self, chat: i64, id: i64, rows: &[Vec<Button>]) -> Result<()> {
            let body = json!({
                "chat_id": chat,
                "message_id": id,
                "reply_markup": { "inline_keyboard": rows },
            });
            self.call::<IgnoredAny>("editMessageReplyMarkup", body, API_TIMEOUT)
                .await
                .map(|_| ())
        }

        /// Settle a press, so the button stops spinning; `text` shows briefly.
        pub async fn settle(&self, click: &str, text: Option<&str>) -> Result<()> {
            let body = json!({ "callback_query_id": click, "text": text });
            self.call::<IgnoredAny>("answerCallbackQuery", body, API_TIMEOUT)
                .await
                .map(|_| ())
        }

        pub async fn send_typing(&self, chat: i64) -> Result<()> {
            let body = json!({ "chat_id": chat, "action": "typing" });
            self.call::<IgnoredAny>("sendChatAction", body, API_TIMEOUT)
                .await
                .map(|_| ())
        }

        // The Bot API answers errors with the same JSON envelope as successes,
        // whatever the HTTP status, so the envelope decides.
        async fn call<T: DeserializeOwned>(
            &self,
            method: &'static str,
            body: Value,
            timeout: Duration,
        ) -> Result<T> {
            let http = |e: reqwest::Error| Error::Http {
                method,
                source: e.without_url(),
            };
            let raw = self
                .http
                .post(format!("{}/{method}", self.base))
                .json(&body)
                .timeout(timeout)
                .send()
                .await
                .map_err(http)?
                .bytes()
                .await
                .map_err(http)?;
            let reply: Reply<T> =
                serde_json::from_slice(&raw).map_err(|source| Error::Json { method, source })?;
            match reply.result {
                Some(result) if reply.ok => Ok(result),
                _ => Err(Error::Api {
                    method,
                    code: reply.error_code,
                    description: reply.description,
                    retry_after: reply.parameters.and_then(|p| p.retry_after),
                }),
            }
        }
    }
}

mod render {
    //! Markdown as Telegram shows it: plain text plus entities, so nothing
    //! the model writes needs escaping and a long reply can be cut anywhere.

    use std::ops::Range;

    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    use crate::api::Entity;

    /// An entity over byte offsets of the whole rendered text.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Span {
        pub kind: &'static str,
        pub range: Range<usize>,
        pub url: Option<String>,
        pub language: Option<String>,
    }

    struct Open {
        kind: &'static str,
        start: usize,
        url: Option<String>,
        language: Option<String>,
    }

    #[derive(Default)]
    struct Table {
        rows: Vec<Vec<String>>,
        row: Vec<String>,
        cell: String,
    }

    #[derive(Default)]
    struct Writer {
        out: String,
        spans: Vec<Span>,
        open: Vec<Open>,
        // Per open link: `None` when it became an entity, else the URL to print after it.
        links: Vec<Option<String>>,
        lists: Vec<Option<u64>>,
        item_started: bool,
        quote_depth: usize,
        table: Option<Table>,
    }

    impl Writer {
        fn push(&mut self, s: &str) {
            match &mut self.table {
                Some(t) => t.cell.push_str(s),
                None => self.out.push_str(s),
            }
        }

        fn block(&mut self) {
            if self.out.is_empty() {
                return;
            }
            while !self.out.ends_with("\n\n") {
                self.out.push('\n');
            }
        }

        fn newline(&mut self) {
            if !self.out.is_empty() && !self.out.ends_with('\n') {
                self.out.push('\n');
            }
        }

        // Inline runs inside a table cell are dropped: the table is one `pre`.
        fn open(&mut self, kind: &'static str, url: Option<String>, language: Option<String>) {
            if self.table.is_none() {
                self.open.push(Open {
                    kind,
                    start: self.out.len(),
                    url,
                    language,
                });
            }
        }

        fn close(&mut self) {
            if self.table.is_some() {
                return;
            }
            if let Some(o) = self.open.pop()
                && o.start < self.out.len()
            {
                self.spans.push(Span {
                    kind: o.kind,
                    range: o.start..self.out.len(),
                    url: o.url,
                    language: o.language,
                });
            }
        }

        fn trim_newlines(&mut self, floor: usize) {
            while self.out.len() > floor && self.out.ends_with('\n') {
                self.out.pop();
            }
        }

        fn render_table(&mut self, table: Table) {
            let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);
            let mut widths = vec![0; cols];
            for row in &table.rows {
                for (w, cell) in widths.iter_mut().zip(row) {
                    *w = (*w).max(cell.chars().count());
                }
            }
            self.block();
            self.open("pre", None, None);
            for (i, row) in table.rows.iter().enumerate() {
                let cells: Vec<String> = (0..cols)
                    .map(|c| {
                        let cell = row.get(c).map_or("", String::as_str);
                        format!("{cell}{}", " ".repeat(widths[c] - cell.chars().count()))
                    })
                    .collect();
                self.out.push_str(cells.join(" | ").trim_end());
                self.out.push('\n');
                if i == 0 {
                    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
                    self.out.push_str(&rule.join("-+-"));
                    self.out.push('\n');
                }
            }
            let start = self.open.last().map_or(0, |o| o.start);
            self.trim_newlines(start);
            self.close();
        }
    }

    // Telegram refuses a whole message over a link it cannot parse.
    fn linkable(url: &str) -> bool {
        reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "tg"))
    }

    pub fn render(input: &str) -> (String, Vec<Span>) {
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        opts.insert(Options::ENABLE_STRIKETHROUGH);
        opts.insert(Options::ENABLE_TASKLISTS);
        let mut w = Writer::default();
        for event in Parser::new_ext(input, opts) {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Paragraph => {
                        if w.item_started {
                            w.item_started = false;
                        } else if w.lists.is_empty() {
                            w.block();
                        } else {
                            w.newline();
                        }
                    }
                    Tag::Heading { .. } => {
                        w.block();
                        w.open("bold", None, None);
                    }
                    Tag::BlockQuote(_) => {
                        w.block();
                        if w.quote_depth == 0 {
                            w.open("blockquote", None, None);
                        }
                        w.quote_depth += 1;
                    }
                    Tag::CodeBlock(kind) => {
                        w.block();
                        let lang = match kind {
                            CodeBlockKind::Fenced(l) if !l.trim().is_empty() => {
                                Some(l.trim().to_string())
                            }
                            _ => None,
                        };
                        w.open("pre", None, lang);
                    }
                    Tag::List(start) => {
                        if w.lists.is_empty() {
                            w.block();
                        } else {
                            w.newline();
                        }
                        w.lists.push(start);
                    }
                    Tag::Item => {
                        w.newline();
                        let indent = "  ".repeat(w.lists.len().saturating_sub(1));
                        let marker = match w.lists.last_mut() {
                            Some(Some(n)) => {
                                *n += 1;
                                format!("{}. ", *n - 1)
                            }
                            _ => "• ".to_string(),
                        };
                        w.push(&format!("{indent}{marker}"));
                        w.item_started = true;
                    }
                    Tag::Table(_) => {
                        w.block();
                        w.table = Some(Table::default());
                    }
                    Tag::TableHead | Tag::TableRow => {
                        if let Some(t) = &mut w.table {
                            t.row.clear();
                        }
                    }
                    Tag::TableCell => {
                        if let Some(t) = &mut w.table {
                            t.cell.clear();
                        }
                    }
                    Tag::Emphasis => w.open("italic", None, None),
                    Tag::Strong => w.open("bold", None, None),
                    Tag::Strikethrough => w.open("strikethrough", None, None),
                    Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                        if w.table.is_none() && linkable(&dest_url) {
                            w.links.push(None);
                            w.open("text_link", Some(dest_url.to_string()), None);
                        } else {
                            w.links.push(Some(dest_url.to_string()));
                        }
                    }
                    _ => {}
                },
                Event::End(tag) => match tag {
                    TagEnd::Heading(_)
                    | TagEnd::Emphasis
                    | TagEnd::Strong
                    | TagEnd::Strikethrough => w.close(),
                    TagEnd::BlockQuote(_) => {
                        w.quote_depth = w.quote_depth.saturating_sub(1);
                        if w.quote_depth == 0 {
                            let start = w.open.last().map_or(0, |o| o.start);
                            w.trim_newlines(start);
                            w.close();
                        }
                    }
                    TagEnd::CodeBlock => {
                        let start = w.open.last().map_or(0, |o| o.start);
                        w.trim_newlines(start);
                        w.close();
                    }
                    TagEnd::List(_) => {
                        w.lists.pop();
                    }
                    TagEnd::Item => w.item_started = false,
                    TagEnd::TableCell => {
                        if let Some(t) = &mut w.table {
                            let cell = std::mem::take(&mut t.cell).trim().to_string();
                            t.row.push(cell);
                        }
                    }
                    TagEnd::TableHead | TagEnd::TableRow => {
                        if let Some(t) = &mut w.table {
                            let row = std::mem::take(&mut t.row);
                            t.rows.push(row);
                        }
                    }
                    TagEnd::Table => {
                        if let Some(t) = w.table.take() {
                            w.render_table(t);
                        }
                    }
                    TagEnd::Link | TagEnd::Image => match w.links.pop() {
                        Some(None) => w.close(),
                        Some(Some(url)) if !url.is_empty() => w.push(&format!(" ({url})")),
                        _ => {}
                    },
                    _ => {}
                },
                Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                    w.item_started = false;
                    w.push(&t);
                }
                Event::Code(c) => {
                    w.item_started = false;
                    w.open("code", None, None);
                    w.push(&c);
                    w.close();
                }
                Event::SoftBreak | Event::HardBreak => {
                    if w.table.is_some() {
                        w.push(" ");
                    } else {
                        w.push("\n");
                    }
                }
                Event::Rule => {
                    w.block();
                    w.push("──────────");
                }
                Event::TaskListMarker(done) => w.push(if done { "[x] " } else { "[ ] " }),
                _ => {}
            }
        }
        let len = w.out.trim_end().len();
        w.out.truncate(len);
        for span in &mut w.spans {
            span.range.end = span.range.end.min(len);
        }
        w.spans.retain(|s| s.range.start < s.range.end);
        (w.out, w.spans)
    }

    fn utf16(s: &str) -> usize {
        s.encode_utf16().count()
    }

    /// The rendered reply as messages within `limit` bytes, each with the
    /// entities that fall in it, re-counted from its own start.
    pub fn pieces(text: &str, spans: &[Span], limit: usize) -> Vec<(String, Vec<Entity>)> {
        crate::split::split(text, limit)
            .into_iter()
            .filter(|r| !text[r.clone()].trim().is_empty())
            .map(|r| {
                let mut entities: Vec<Entity> = spans
                    .iter()
                    .filter_map(|s| {
                        let start = s.range.start.max(r.start);
                        let end = s.range.end.min(r.end);
                        (start < end).then(|| Entity {
                            kind: s.kind,
                            offset: utf16(&text[r.start..start]),
                            length: utf16(&text[start..end]),
                            url: s.url.clone(),
                            language: s.language.clone(),
                        })
                    })
                    .collect();
                entities.sort_by_key(|e| (e.offset, std::cmp::Reverse(e.length)));
                (text[r].to_string(), entities)
            })
            .collect()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Offsets are UTF-16 units from each piece's own start: an emoji
        // counts two, and a span cut by the split continues in the next piece.
        #[test]
        fn entities_are_counted_in_utf16_from_their_own_piece() {
            let (text, spans) = render(&format!("{}\n\n😀 **粗体**", "a".repeat(30)));
            let pieces = pieces(&text, &spans, 32);
            assert_eq!(pieces.len(), 2);
            let (piece, entities) = &pieces[1];
            assert_eq!(piece, "😀 粗体");
            assert_eq!(
                entities.as_slice(),
                [Entity {
                    kind: "bold",
                    offset: 3,
                    length: 2,
                    url: None,
                    language: None,
                }]
            );
        }

        // A link Telegram would refuse keeps its address as text instead.
        #[test]
        fn a_link_that_is_not_a_web_address_is_spelled_out() {
            let (text, spans) = render("see [the file](src/main.rs) or [docs](https://x.dev)");
            assert_eq!(text, "see the file (src/main.rs) or docs");
            assert_eq!(spans.len(), 1);
            assert_eq!(&text[spans[0].range.clone()], "docs");
        }

        #[test]
        fn a_code_block_is_pre_without_its_trailing_newline() {
            let (text, spans) = render("run:\n\n```sh\ncargo test\n```\n");
            assert_eq!(text, "run:\n\ncargo test");
            assert_eq!(spans[0].kind, "pre");
            assert_eq!(spans[0].language.as_deref(), Some("sh"));
            assert_eq!(&text[spans[0].range.clone()], "cargo test");
        }
    }
}

mod split {
    //! One reply cut into messages a platform will take: each within a byte
    //! budget, at the most natural boundary in reach. Mirrors `wechat.rs`'s,
    //! returning ranges so entities can follow the cut.

    use std::ops::Range;

    pub(crate) fn split(text: &str, limit: usize) -> Vec<Range<usize>> {
        let mut pieces = Vec::new();
        let mut at = 0;
        while at < text.len() {
            let rest = &text[at..];
            if rest.len() <= limit {
                pieces.push(at..text.len());
                break;
            }
            let (cut, skip) = boundary(rest, limit);
            pieces.push(at..at + cut);
            at += cut + skip;
        }
        pieces
    }

    // Falls back paragraph → line → space → char boundary; the cut is never
    // zero, or the loop would spin forever.
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
                rest[i..]
                    .bytes()
                    .take_while(|b| matches!(b, b'\n' | b'\r'))
                    .count()
            };
            return (i, skip);
        }
        (end, 0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_run_with_no_boundary_is_cut_on_a_character() {
            let text = "中".repeat(200);
            let pieces = split(&text, 100);
            assert!(pieces.len() > 1);
            let rejoined: String = pieces.into_iter().map(|r| &text[r]).collect();
            assert_eq!(rejoined, text);
        }
    }
}
