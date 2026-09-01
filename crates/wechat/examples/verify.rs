//! Standalone verification binary (WECHAT.md §5.1): login with a QR code,
//! long-poll for inbound messages, echo every text back verbatim, and keep
//! enough state under the pi root that a restart resumes without re-scanning.
//!
//! Run with `cargo run -p wechat --example verify`. State lives at
//! `$PI_HOME/wechat.json` (default `~/.pi/wechat.json`), the same file the pi
//! bridge uses, so the two can hand the session over to each other.
//!
//! This is the tool the design doc wants kept: it is the protocol layer's own
//! end-to-end check, independent of the agent.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use wechat::{Client, LoginView, Update};

/// How long the long-poll may hold between polls (the server holds 35s).
const LONG_POLL: Duration = Duration::from_millis(35_000);

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let state_path = state_path();
    let mut client = Client::new(wechat::DEFAULT_BASE_URL);

    let mut state = load_state(&state_path).unwrap_or_default();
    if state.token.is_none() {
        println!("no saved login — scanning a QR now");
        let mut view = LoginView {
            show_qr: Box::new(|rendered: &str| println!("{rendered}")),
            notice: Box::new(|text: &str| println!("{text}")),
            read_verify_code: None,
        };
        let credentials = wechat::login_flow(&mut client, &mut view).await?;
        println!("connected as {} (host {})", credentials.bot_id, credentials.base_url);
        state.token = Some(credentials.token);
        state.base_url = credentials.base_url;
        state.bot_id = Some(credentials.bot_id);
        state.user_id = Some(credentials.user_id);
        save_state(&state_path, &state)?;
    }
    client = Client::new(state.base_url.clone());

    let state = Arc::new(Mutex::new(state));
    println!("polling for messages — send one from the phone (Ctrl-C to stop)");
    loop {
        let (token, buf) = {
            let s = state.lock().await;
            (
                s.token.clone().context("token vanished")?,
                s.get_updates_buf.clone(),
            )
        };
        match client.get_updates(&token, &buf, LONG_POLL).await {
            Ok(update) => handle(&client, &state, &state_path, update).await?,
            Err(e) => {
                eprintln!("getupdates failed: {e:#}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn handle(
    client: &Client,
    state: &Arc<Mutex<State>>,
    path: &PathBuf,
    update: Update,
) -> Result<()> {
    if update.is_error() {
        if update.is_stale_token() {
            eprintln!("token stale — delete {} and re-run to scan again", path.display());
            std::process::exit(1);
        }
        eprintln!(
            "getupdates error: ret={} errcode={:?} errmsg={:?}",
            update.ret, update.errcode, update.errmsg
        );
        return Ok(());
    }
    if !update.get_updates_buf.is_empty() {
        let mut s = state.lock().await;
        s.get_updates_buf = update.get_updates_buf.clone();
        save_state(path, &s)?;
    }
    for msg in update.msgs {
        if msg.message_type != 1 {
            continue;
        }
        let text = wechat::text_of(&msg);
        if text.is_empty() {
            continue;
        }
        println!("inbound from {}: {text:?}", msg.from_user_id);
        // The verification contract: echo the message back verbatim, with the
        // message's own context_token.
        let Some(context_token) = msg.context_token else {
            eprintln!("message carried no context_token — cannot reply");
            continue;
        };
        let (token, _) = {
            let s = state.lock().await;
            (s.token.clone().context("token vanished")?, ())
        };
        client
            .send_text(&token, &msg.from_user_id, &context_token, &text)
            .await
            .map_err(|e| anyhow::anyhow!("send failed: {e:#}"))?;
        println!("echoed {text:?}");
    }
    Ok(())
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct State {
    token: Option<String>,
    base_url: String,
    bot_id: Option<String>,
    user_id: Option<String>,
    get_updates_buf: String,
}

fn state_path() -> PathBuf {
    let root = std::env::var_os("PI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
        .unwrap_or_else(|| PathBuf::from(".pi"));
    root.join("wechat.json")
}

fn load_state(path: &PathBuf) -> Result<State> {
    let body = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&body)?)
}

fn save_state(path: &PathBuf, state: &State) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.pi-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
