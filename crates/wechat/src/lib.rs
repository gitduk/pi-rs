//! A pure protocol client for the Weixin iLink bot API (HTTP/JSON long-poll).
//!
//! This crate knows the wire and nothing else: no session, no persistence, no
//! notion of pi. The bridge in `crates/cli` owns state and decides what a
//! message means, so this crate stays reusable.
//!
//! Verified statically against `@tencent-weixin/openclaw-weixin` 2.4.8 (the
//! package the WECHAT.md protocol notes were reverse-engineered from, four
//! minor versions later). See `WECHAT.md` §3 for what was checked where.

pub mod client;
pub mod login;
pub mod types;

pub use client::{Client, Error as ClientError};
pub use login::{login as login_flow, LoginError, LoginView, render_qr};
pub use types::{
    text_of, Credentials, QrCode, QrStatus, Update, WireMessage, CHANNEL_VERSION, DEFAULT_BASE_URL,
};
