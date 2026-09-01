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
#[derive(Debug, Clone)]
pub struct Credentials {
    pub token: String,
    pub base_url: String,
    pub bot_id: String,
    pub user_id: String,
}

/// The result of one `get_qrcode_status` long-poll.
#[derive(Debug, Clone)]
pub enum QrStatus {
    /// Nothing happened (including a client-side 35s timeout): keep polling.
    Wait,
    /// Scanned; WeChat is still waiting for the user to confirm.
    Scanned,
    /// The phone shows a verification code that has to be typed in.
    NeedVerifyCode,
    /// The code was wrong too many times; a fresh QR is needed.
    VerifyCodeBlocked,
    /// The QR has expired; a fresh one is needed.
    Expired,
    /// This bot is already bound to another client; no new login possible.
    BindedRedirect,
    /// The session is being redirected to another host; resume polling there.
    Redirect { host: String },
    /// Login confirmed.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_yields_its_first_text_item() {
        let msg: WireMessage = serde_json::from_value(serde_json::json!({
            "from_user_id": "o9cq800kum_xxx@im.wechat",
            "message_type": 1,
            "context_token": "AARzJWAFAAABAAAAAAAp",
            "item_list": [
                { "type": 2, "image_item": { "media": {} } },
                { "type": 1, "text_item": { "text": "你好" } }
            ]
        }))
        .unwrap();
        assert_eq!(text_of(&msg), "你好");
        assert_eq!(msg.context_token.as_deref(), Some("AARzJWAFAAABAAAAAAAp"));
    }

    #[test]
    fn a_voice_transcription_counts_as_text() {
        let msg: WireMessage = serde_json::from_value(serde_json::json!({
            "from_user_id": "u@im.wechat",
            "message_type": 1,
            "item_list": [ { "type": 3, "voice_item": { "text": "转文字" } } ]
        }))
        .unwrap();
        assert_eq!(text_of(&msg), "转文字");
    }

    #[test]
    fn unknown_fields_do_not_break_decoding() {
        // A group_id (section 4 of the reference doc) and bookkeeping fields
        // the server has added must not kill the long-poll loop.
        let msg: WireMessage = serde_json::from_value(serde_json::json!({
            "from_user_id": "u@im.wechat",
            "group_id": "g@chatroom",
            "seq": 42,
            "message_id": "x",
            "create_time_ms": 1700000000000_i64,
            "item_list": []
        }))
        .unwrap();
        assert_eq!(text_of(&msg), "");
        assert_eq!(msg.from_user_id, "u@im.wechat");
    }

    #[test]
    fn an_empty_poll_is_not_an_error() {
        let u: Update = serde_json::from_str("{\"ret\":0,\"msgs\":[],\"get_updates_buf\":\"b\",\"longpolling_timeout_ms\":35000}").unwrap();
        assert!(!u.is_error());
        assert_eq!(u.get_updates_buf, "b");
    }

    #[test]
    fn a_stale_token_is_named() {
        for body in [
            r#"{"ret":-14,"errmsg":"token expired"}"#,
            r#"{"errcode":-14}"#,
        ] {
            let u: Update = serde_json::from_str(body).unwrap();
            assert!(u.is_error());
            assert!(u.is_stale_token(), "{body}");
        }
        let u: Update = serde_json::from_str(r#"{"ret":10002,"errmsg":"rate limited"}"#).unwrap();
        assert!(u.is_error());
        assert!(!u.is_stale_token());
    }
}
