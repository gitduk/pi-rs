//! The QR login state machine, mirroring the reference client's
//! `waitForWeixinLogin`: long-poll the QR status, refresh on expiry (bounded),
//! follow IDC redirects, and hand back credentials on confirm.
//!
//! Interaction with a person goes through the two callbacks so the same flow
//! serves a plain terminal and pi's TUI. The `need_verifycode` state — the
//! phone shows a code that has to be typed back — needs a reader callback;
//! without one the login ends honestly instead of hanging.

use std::time::{Duration, Instant};

use crate::client::Client;
use crate::types::{Credentials, QrStatus};

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("wechat login: {0}")]
    Failed(String),
    #[error("wechat login timed out after {0:?} — try /wechat on again")]
    Timeout(Duration),
    #[error(
        "wechat login: the phone asked for a verification code, which this build cannot enter \
         (the account has extra verification enabled)"
    )]
    VerifyCodeUnsupported,
    #[error("wechat login: this bot is already bound to another client")]
    AlreadyBound,
}

pub type Result<T> = std::result::Result<T, LoginError>;

/// How a person sees the login. `show_qr` receives the rendered QR (text);
/// `notice` receives one-line progress messages including the fallback URL.
/// `read_verify_code` returns the code typed by the user, or `None` to abort.
/// Owned boxes so the flow can run inside a spawned task.
pub struct LoginView {
    pub show_qr: Box<dyn FnMut(&str) + Send>,
    pub notice: Box<dyn FnMut(&str) + Send>,
    pub read_verify_code: Option<Box<dyn FnMut() -> Option<String> + Send>>,
}

/// How long the whole login may take before `Timeout`.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(8 * 60);
/// A QR may be refreshed this many times after expiring.
pub const MAX_QR_REFRESH: u32 = 3;
/// The pause between polls, as in the reference (each poll itself holds up to
/// 35s server-side).
pub const POLL_PAUSE: Duration = Duration::from_secs(1);

/// Run the login until confirmed. `client` may end up on a different host
/// after an IDC redirect; the returned credentials carry the final base URL.
pub async fn login(client: &mut Client, view: &mut LoginView) -> Result<Credentials> {
    let mut qrcode = fetch_qrcode(client, view).await?;
    let mut refresh = 0u32;
    let mut verify_code: Option<String> = None;
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut said_scanned = false;

    loop {
        if Instant::now() >= deadline {
            return Err(LoginError::Timeout(LOGIN_TIMEOUT));
        }
        let status = client
            .poll_qrcode(&qrcode.qrcode, verify_code.as_deref())
            .await
            .map_err(|e| LoginError::Failed(e.to_string()))?;
        match status {
            QrStatus::Wait => {}
            QrStatus::Scanned => {
                if !said_scanned {
                    (view.notice)("scanned — confirm it on the phone");
                    said_scanned = true;
                }
            }
            QrStatus::NeedVerifyCode => {
                let Some(read) = view.read_verify_code.as_mut() else {
                    return Err(LoginError::VerifyCodeUnsupported);
                };
                (view.notice)("the phone shows a verification code — type it here");
                let Some(code) = read() else {
                    return Err(LoginError::Failed("verification code not given".into()));
                };
                verify_code = Some(code);
                said_scanned = false;
                continue;
            }
            QrStatus::VerifyCodeBlocked => {
                (view.notice)("the verification code was refused; refreshing the QR");
                qrcode = refresh_qrcode(client, view, &mut refresh).await?;
                verify_code = None;
                said_scanned = false;
                continue;
            }
            QrStatus::Expired => {
                (view.notice)("the QR expired; refreshing");
                qrcode = refresh_qrcode(client, view, &mut refresh).await?;
                verify_code = None;
                said_scanned = false;
                continue;
            }
            QrStatus::BindedRedirect => {
                return Err(LoginError::AlreadyBound);
            }
            QrStatus::Redirect { host } if !host.is_empty() => {
                let base = format!("https://{host}");
                (view.notice)(&format!("redirected to {base}"));
                *client = Client::new(base);
            }
            QrStatus::Redirect { .. } => {
                (view.notice)("the server asked to redirect but named no host; continuing");
            }
            QrStatus::Confirmed(credentials) => return Ok(credentials),
        }
        tokio::time::sleep(POLL_PAUSE).await;
    }
}

/// Render a URL as a text QR the terminal can show, or `None` if the URL is
/// too large to encode. Same job as the reference's `qrcode-terminal`.
pub fn render_qr(url: &str) -> Option<String> {
    use qrcode::render::unicode;
    let code = qrcode::QrCode::new(url.as_bytes()).ok()?;
    Some(code.render::<unicode::Dense1x2>().build())
}

async fn fetch_qrcode(client: &Client, view: &mut LoginView) -> Result<crate::types::QrCode> {
    let qr = client
        .fetch_qrcode(&[])
        .await
        .map_err(|e| LoginError::Failed(e.to_string()))?;
    (view.notice)("scan the QR with WeChat to connect this session");
    if let Some(rendered) = render_qr(&qr.qrcode_img_content) {
        (view.show_qr)(&rendered);
    }
    (view.notice)(&format!(
        "if the QR does not render, open: {}",
        qr.qrcode_img_content
    ));
    Ok(qr)
}

async fn refresh_qrcode(
    client: &Client,
    view: &mut LoginView,
    refresh: &mut u32,
) -> Result<crate::types::QrCode> {
    if *refresh >= MAX_QR_REFRESH {
        return Err(LoginError::Failed(format!(
            "the QR expired {MAX_QR_REFRESH} times — give up and try /wechat on again"
        )));
    }
    *refresh += 1;
    let qr = client
        .fetch_qrcode(&[])
        .await
        .map_err(|e| LoginError::Failed(e.to_string()))?;
    (view.notice)("a fresh QR follows — scan it");
    if let Some(rendered) = render_qr(&qr.qrcode_img_content) {
        (view.show_qr)(&rendered);
    }
    Ok(qr)
}
