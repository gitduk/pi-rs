//! Wire types for the Weixin iLink bot API (HTTP/JSON long-poll).
//!
//! Mirrors `@tencent-weixin/openclaw-weixin` 2.4.8, the package this client was
//! verified against. Unknown fields are ignored by construction (no
//! `deny_unknown_fields`): the server has added fields between versions — a
//! `group_id` on messages, bookkeeping fields on responses — and a decoder that
//! dies on them breaks the whole long-poll loop.

use serde::Deserialize;

/// The fixed API host for QR requests and the default for everything else. A
/// login may be redirected to another host (`scaned_but_redirect`); the
/// confirmed response's `baseurl` is what a session then talks to.
pub const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

/// The channel version the server negotiates against, sent verbatim in every
/// `base_info`. This is a protocol negotiation, not a cosmetic string: it is
/// the package version of the reference client, and changing it is a claim
/// about the wire format.
pub const CHANNEL_VERSION: &str = "2.4.8";

/// The `iLink-App-Id` every request carries, from the reference package.json.
pub const ILINK_APP_ID: &str = "bot";

/// The `iLink-App-ClientVersion` header: the reference package version 2.4.8
/// encoded as `major<<16 | minor<<8 | patch` (0x00020008 = 132104).
pub const ILINK_APP_CLIENT_VERSION: u32 = 132_104;

/// What `get_bot_qrcode` asks for: the bot flavour, 3 per the reference.
pub const BOT_TYPE: &str = "3";

/// One login QR code, as issued.
#[derive(Debug, Clone, Deserialize)]
pub struct QrCode {
    pub qrcode: String,
    /// The URL the QR encodes; also what a phone can open directly.
    pub qrcode_img_content: String,
}

/// What a successful login hands back. `base_url` is the confirmed response's
/// `baseurl` — the reference uses it for every subsequent request.
#[derive(Clone)]
pub struct Credentials {
    pub token: String,
    pub base_url: String,
    pub bot_id: String,
    pub user_id: String,
}

// Manual rather than derived: a `{:?}` that prints the session token leaks it
// through any log line that happens to carry the credentials.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("bot_id", &self.bot_id)
            .field("user_id", &self.user_id)
            .finish()
    }
}

/// The result of one `get_qrcode_status` long-poll.
#[derive(Debug, Clone)]
pub enum QrStatus {
    // Nothing happened (including a client-side 35s timeout): keep polling.
    Wait,
    // Scanned; WeChat is still waiting for the user to confirm.
    Scanned,
    // The phone shows a verification code that has to be typed in.
    NeedVerifyCode,
    // The code was wrong too many times; a fresh QR is needed.
    VerifyCodeBlocked,
    // The QR has expired; a fresh one is needed.
    Expired,
    // This bot is already bound to another client; no new login possible.
    BindedRedirect,
    // The session is being redirected to another host; resume polling there.
    Redirect { host: String },
    // Login confirmed.
    Confirmed(Credentials),
}

/// One message from `getupdates`. Only the fields pi reads are typed; the rest
/// ride along as ignored serde unknowns.
#[derive(Debug, Clone, Deserialize)]
pub struct WireMessage {
    #[serde(default)]
    pub from_user_id: String,
    #[serde(default)]
    pub to_user_id: String,
    /// 1 = user sent it, 2 = bot sent it. The loop handles only 1.
    #[serde(default)]
    pub message_type: i64,
    #[serde(default)]
    pub message_state: i64,
    /// Required verbatim on any reply, or the reply misses the conversation.
    #[serde(default)]
    pub context_token: Option<String>,
    #[serde(default)]
    pub item_list: Vec<Item>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    /// 1 text, 2 image, 3 voice, 4 file, 5 video.
    #[serde(default)]
    pub r#type: i64,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    /// A voice item may carry a transcription (`text`), which is as close to
    /// text as this client goes.
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextItem {
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceItem {
    #[serde(default)]
    pub text: Option<String>,
}

/// The text a message carries, by priority: the first text item, else a voice
/// transcription. Media-only messages yield "".
pub fn text_of(msg: &WireMessage) -> String {
    for item in &msg.item_list {
        match item.r#type {
            1 => {
                if let Some(t) = &item.text_item {
                    return t.text.clone();
                }
            }
            3 => {
                if let Some(v) = &item.voice_item
                    && let Some(text) = &v.text
                {
                    return text.clone();
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// One `getupdates` response. A client-side long-poll timeout returns an empty
/// `Update` (ret 0, no messages) rather than an error — that is the normal
/// no-news case, exactly like the reference client.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Update {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Vec<WireMessage>,
    /// The cursor to send on the next poll; persisted so nothing is re-sent.
    #[serde(default)]
    pub get_updates_buf: String,
    /// The server's suggested hold time for the next poll, when it says one.
    #[serde(default)]
    pub longpolling_timeout_ms: Option<u64>,
}

impl Update {
    /// The error code the server uses for a stale/expired bot token.
    pub const STALE_TOKEN: i64 = -14;

    /// Whether the response is an API error (as opposed to a normal empty
    /// long-poll). `ret` and `errcode` are checked independently, as in the
    /// reference monitor.
    pub fn is_error(&self) -> bool {
        self.ret != 0 || self.errcode.is_some_and(|e| e != 0)
    }

    /// Whether the error means the bot token no longer works and a fresh
    /// QR login is required.
    pub fn is_stale_token(&self) -> bool {
        self.ret == Self::STALE_TOKEN || self.errcode == Some(Self::STALE_TOKEN)
    }
}

/// What `getconfig` returns that pi uses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub typing_ticket: Option<String>,
}
