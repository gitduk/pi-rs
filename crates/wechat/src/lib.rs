//! WeChat for pi: a protocol client for the Weixin iLink bot API (HTTP/JSON
//! long-poll), and `pi-wechat`, the tool that bridges a session to it.
//!
//! `client`, `login` and `types` know only the wire; `bridge` owns the saved
//! state and what a message means. Verified against
//! `@tencent-weixin/openclaw-weixin` 2.4.8.

pub mod bridge;
mod client;
mod login;
mod markdown;
mod split;
mod types;

use client::Client;
use login::{LoginView, login as login_flow};
use types::{DEFAULT_BASE_URL, Update, text_of};
