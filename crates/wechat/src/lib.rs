//! WeChat for pi: a protocol client for the Weixin iLink bot API (HTTP/JSON
//! long-poll), and `WeChat`, the `channel::Channel` built on it.
//!
//! `client`, `login` and `types` know only the wire; `adapter` owns the saved
//! state and what a message means. Verified against
//! `@tencent-weixin/openclaw-weixin` 2.4.8 — see `WECHAT.md` §3.

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
