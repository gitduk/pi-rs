//! WeChat for pi: a protocol client for the Weixin iLink bot API (HTTP/JSON
//! long-poll), and `WeChat`, the `channel::Channel` built on it.
//!
//! `client`, `login` and `types` know the wire and nothing else: no session,
//! no persistence. `adapter` owns the saved state and what a message means.
//!
//! Verified statically against `@tencent-weixin/openclaw-weixin` 2.4.8 (the
//! package the WECHAT.md protocol notes were reverse-engineered from, four
//! minor versions later). See `WECHAT.md` §3 for what was checked where.

mod adapter;
pub mod client;
pub mod login;
mod markdown;
pub mod types;

pub use adapter::{Keep, WeChat};
pub use client::{Client, Error as ClientError};
pub use login::{LoginError, LoginView, login as login_flow, render_qr};
pub use types::{
    CHANNEL_VERSION, Credentials, DEFAULT_BASE_URL, QrCode, QrStatus, Update, WireMessage, text_of,
};
