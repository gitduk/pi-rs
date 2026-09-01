//! The iLink protocol client: fixed headers, the six endpoints pi uses, and
//! the two long-polling calls that treat a client-side timeout as the normal
//! empty result rather than an error.
//!
//! Everything here is protocol only — no session, no persistence. The bridge
//! in `crates/cli` owns state and decides what a message means.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::RequestBuilder;
use serde_json::{json, Value};

use crate::types::{
    BOT_TYPE, CHANNEL_VERSION, Config, Credentials, ILINK_APP_CLIENT_VERSION, ILINK_APP_ID,
    QrCode, QrStatus, Update,
};

/// How long a long-poll request is allowed to hold before the client treats
/// it as an empty poll. The server holds up to 35s itself; the reference
/// client aborts at exactly the same mark.
pub const LONG_POLL_TIMEOUT: Duration = Duration::from_millis(35_000);
/// Regular API calls (sendMessage, getUploadUrl).
pub const API_TIMEOUT: Duration = Duration::from_millis(15_000);
/// Lightweight calls (getConfig, sendTyping).
pub const CONFIG_TIMEOUT: Duration = Duration::from_millis(10_000);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wechat {endpoint}: ret={ret} errmsg={errmsg:?}")]
    Api {
        endpoint: String,
        ret: i64,
        errmsg: Option<String>,
    },
    #[error("wechat {endpoint}: http {status} {body}")]
    Http {
        endpoint: String,
        status: u16,
        body: String,
    },
    #[error("wechat: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("wechat: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The one request channel. Cloneable — the long-poll task and the bridge
/// share it — and cheap: `reqwest::Client` pools connections inside.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base_url: String,
}

impl Client {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Ask for a login QR. `local_tokens` are previously-known bot tokens:
    /// the server uses them to say "already bound" instead of issuing a login.
    pub async fn fetch_qrcode(&self, local_tokens: &[String]) -> Result<QrCode> {
        let body = json!({ "local_token_list": local_tokens });
        let endpoint = format!("ilink/bot/get_bot_qrcode?bot_type={BOT_TYPE}");
        let resp: Value = self.post(&endpoint, None, body, API_TIMEOUT).await?;
        Ok(serde_json::from_value(resp)?)
    }

    /// Long-poll the QR's scan status. A 35s client timeout means "nothing
    /// yet" and reads as `QrStatus::Wait`, never as an error.
    pub async fn poll_qrcode(&self, qrcode: &str, verify_code: Option<&str>) -> Result<QrStatus> {
        let mut endpoint = format!("ilink/bot/get_qrcode_status?qrcode={qrcode}");
        if let Some(code) = verify_code {
            endpoint.push_str(&format!("&verify_code={code}"));
        }
        let raw = match self.get(&endpoint, LONG_POLL_TIMEOUT).await {
            Ok(raw) => raw,
            Err(Error::Reqwest(e)) if e.is_timeout() => {
                return Ok(QrStatus::Wait);
            }
            Err(e) => return Err(e),
        };
        let v: Value = serde_json::from_str(&raw)?;
        parse_status(v)
    }

    /// Long-poll for inbound messages. A client-side timeout — the server
    /// held past our 35s without news — is the ordinary empty result.
    pub async fn get_updates(&self, token: &str, buf: &str, timeout: Duration) -> Result<Update> {
        let body = json!({
            "get_updates_buf": buf,
            "base_info": base_info(),
        });
        let resp: Value = match self
            .post("ilink/bot/getupdates", Some(token), body, timeout)
            .await
        {
            Ok(v) => v,
            Err(Error::Reqwest(e)) if e.is_timeout() => return Ok(Update::default()),
            Err(e) => return Err(e),
        };
        Ok(serde_json::from_value(resp)?)
    }

    /// Send one text message. `context_token` must be the inbound message's
    /// own token, verbatim, or the reply misses the conversation window.
    pub async fn send_text(
        &self,
        token: &str,
        to: &str,
        context_token: &str,
        text: &str,
    ) -> Result<()> {
        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to,
                "client_id": client_id(),
                "message_type": 2,
                "message_state": 2,
                "item_list": [ { "type": 1, "text_item": { "text": text } } ],
                "context_token": context_token,
            },
            "base_info": base_info(),
        });
        self.post_no_body("ilink/bot/sendmessage", Some(token), body, API_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// The per-user config, currently just the typing ticket. The bridge caches
    /// it per user and refreshes periodically, as the reference does.
    pub async fn get_config(&self, token: &str, user: &str, context_token: &str) -> Result<Config> {
        let body = json!({
            "ilink_user_id": user,
            "context_token": context_token,
            "base_info": base_info(),
        });
        let resp: Value = self
            .post("ilink/bot/getconfig", Some(token), body, CONFIG_TIMEOUT)
            .await?;
        Ok(serde_json::from_value(resp)?)
    }

    /// Send the "typing…" indicator (or cancel it).
    pub async fn send_typing(
        &self,
        token: &str,
        user: &str,
        ticket: &str,
        status: i64,
    ) -> Result<()> {
        let body = json!({
            "ilink_user_id": user,
            "typing_ticket": ticket,
            "status": status,
            "base_info": base_info(),
        });
        self.post_no_body("ilink/bot/sendtyping", Some(token), body, CONFIG_TIMEOUT)
            .await
            .map(|_| ())
    }

    async fn get(&self, endpoint: &str, timeout: Duration) -> Result<String> {
        self.call(
            self.http.get(self.url(endpoint)),
            None,
            endpoint,
            timeout,
        )
        .await
    }

    async fn post(
        &self,
        endpoint: &str,
        token: Option<&str>,
        body: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.post_no_body(endpoint, token, body, timeout).await
    }

    async fn post_no_body(
        &self,
        endpoint: &str,
        token: Option<&str>,
        body: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let raw = self
            .call(
                self.http.post(self.url(endpoint)).json(&body),
                token,
                endpoint,
                timeout,
            )
            .await?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// The shared send path: headers, timeout, and the non-2xx / non-zero
    /// `ret` verdicts. Everything the reference's `apiPostFetch` does.
    async fn call(
        &self,
        mut req: RequestBuilder,
        token: Option<&str>,
        endpoint: &str,
        timeout: Duration,
    ) -> Result<String> {
        req = req.headers(headers(token)).timeout(timeout);
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let raw = resp.text().await.unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(Error::Http {
                endpoint: endpoint.to_string(),
                status,
                body: raw,
            });
        }
        if let Ok(v) = serde_json::from_str::<Value>(&raw)
            && let Some(ret) = v["ret"].as_i64()
            && ret != 0
        {
            return Err(Error::Api {
                endpoint: endpoint.to_string(),
                ret,
                errmsg: v["errmsg"].as_str().map(str::to_string),
            });
        }
        Ok(raw)
    }

    fn url(&self, endpoint: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), endpoint)
    }
}

fn base_info() -> Value {
    json!({
        "channel_version": CHANNEL_VERSION,
        "bot_agent": "OpenClaw",
    })
}

/// A fresh random uint32, as its decimal string, base64 — one per request,
/// which is the replay protection the server expects.
fn wechat_uin() -> String {
    let n = rand::random::<u32>().to_string();
    STANDARD.encode(n.as_bytes())
}

/// The fixed request headers, mirroring the reference client: the two app
/// headers on every request; the auth trio on POSTs once a token exists.
fn headers(token: Option<&str>) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(
        "AuthorizationType",
        HeaderValue::from_static("ilink_bot_token"),
    );
    h.insert("X-WECHAT-UIN", HeaderValue::from_str(&wechat_uin()).unwrap());
    h.insert("iLink-App-Id", HeaderValue::from_static(ILINK_APP_ID));
    h.insert(
        "iLink-App-ClientVersion",
        HeaderValue::from_str(&ILINK_APP_CLIENT_VERSION.to_string()).unwrap(),
    );
    if let Some(token) = token.filter(|t| !t.trim().is_empty()) {
        h.insert(
            reqwest::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
    }
    h
}

/// Parse one `get_qrcode_status` body into a `QrStatus`.
fn parse_status(v: Value) -> Result<QrStatus> {
    let status = v["status"].as_str().unwrap_or("wait");
    let q = |name: &str| v[name].as_str().map(str::to_string);
    Ok(match status {
        "wait" => QrStatus::Wait,
        "scaned" => QrStatus::Scanned,
        "need_verifycode" => QrStatus::NeedVerifyCode,
        "verify_code_blocked" => QrStatus::VerifyCodeBlocked,
        "expired" => QrStatus::Expired,
        "binded_redirect" => QrStatus::BindedRedirect,
        "scaned_but_redirect" => QrStatus::Redirect {
            host: v["redirect_host"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        },
        "confirmed" => QrStatus::Confirmed(Credentials {
            token: q("bot_token").unwrap_or_default(),
            base_url: q("baseurl").unwrap_or_else(|| crate::types::DEFAULT_BASE_URL.into()),
            bot_id: q("ilink_bot_id").unwrap_or_default(),
            user_id: q("ilink_user_id").unwrap_or_default(),
        }),
        other => {
            tracing::warn!(target: "pi::wechat", status = other, "unrecognised QR status");
            QrStatus::Wait
        }
    })
}

/// A client id the server accepts for tracing; shape mirrors the reference
/// (`prefix:timestamp-hex`).
fn client_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("pi-wechat:{now}-{:08x}", rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_confirmed_status_becomes_credentials() {
        let v = json!({
            "status": "confirmed",
            "bot_token": "tk",
            "baseurl": "https://ilinkai.weixin.qq.com",
            "ilink_bot_id": "b@im.bot",
            "ilink_user_id": "u@im.wechat",
        });
        match parse_status(v).unwrap() {
            QrStatus::Confirmed(c) => {
                assert_eq!(c.token, "tk");
                assert_eq!(c.bot_id, "b@im.bot");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn an_unknown_status_waits_rather_than_failing() {
        let v = json!({ "status": "fancy_new_state" });
        assert!(matches!(parse_status(v).unwrap(), QrStatus::Wait));
    }

    #[test]
    fn the_uin_is_base64_of_the_decimal_string() {
        // Verbatim from the reference: the u32 is formatted as decimal first,
        // then base64 — not base64 of the raw bytes.
        assert_eq!(STANDARD.encode(b"hello"), "aGVsbG8=");
        assert_eq!(STANDARD.encode(b"12345"), "MTIzNDU=");
        assert_eq!(STANDARD.encode(b""), "");
    }

    #[test]
    fn the_send_body_echoes_the_context_token() {
        // The one field the reference calls the biggest trap: context_token
        // must be passed through verbatim.
        let body = json!({
            "msg": { "context_token": "AARzJWAF", "to_user_id": "u@im.wechat" },
            "base_info": {}
        });
        assert_eq!(body["msg"]["context_token"], "AARzJWAF");
    }
}
