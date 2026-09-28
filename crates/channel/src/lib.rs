//! What an input channel is: a chat platform the session can be driven from.
//!
//! A platform implements `Channel` — how to connect, how to send one message,
//! what its messages may hold — and nothing else. What gets said back, and
//! when, is the surface's to decide, written once for every platform.

use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::error::SendError;
use tokio_util::sync::CancellationToken;

/// What a channel hands the surface.
pub enum Inbound {
    /// A text message from the peer.
    Text { text: String },
    /// A line for the local scrollback, where it lasts: a status, an error,
    /// the QR. Anything that must stay long enough to be read or scanned.
    Notice(String),
    /// A line for the bar, for a moment: news nobody reads twice.
    Flash(String),
    /// The peer typed `/stop` or `/esc`: interrupt the running turn now.
    Stop,
}

/// Where a channel reports. Stamps each message with the channel's name, so
/// the answer goes back to the channel that asked and no other.
#[derive(Clone)]
pub struct Inbox {
    from: &'static str,
    tx: UnboundedSender<(&'static str, Inbound)>,
}

impl Inbox {
    /// The inbox of the channel called `from`, feeding the surface's `tx`.
    pub fn new(from: &'static str, tx: UnboundedSender<(&'static str, Inbound)>) -> Self {
        Self { from, tx }
    }

    pub fn send(&self, inbound: Inbound) -> Result<(), SendError<(&'static str, Inbound)>> {
        self.tx.send((self.from, inbound))
    }
}

/// One chat platform. Implementations own their credentials, their wire and
/// their one peer; the relay decides what is said and when.
#[async_trait]
pub trait Channel: Send + Sync {
    /// The word its command answers to: `wechat` is `/wechat`.
    fn name(&self) -> &'static str;
    /// Bytes one outbound message may hold. Bytes because they never
    /// undercount: a platform that limits characters is safe under it too.
    fn limit(&self) -> usize;
    /// The pause between pieces of one split message. A burst is what a rate
    /// limiter watches for; the default is short enough that nobody notices.
    fn pace(&self) -> Duration {
        Duration::from_millis(500)
    }
    /// The model's markdown, as this platform can show it.
    fn format(&self, markdown: &str) -> String;
    /// Log in if needed, then receive until `abort` fires. Progress, the
    /// peer's messages and failures all go out on `inbox`.
    async fn run(&self, inbox: Inbox, abort: CancellationToken);
    /// Send one message to the peer. No peer yet is not an error: there is
    /// nobody to tell.
    async fn send(&self, text: &str) -> anyhow::Result<()>;
    /// Best effort; a platform without an indicator keeps the default.
    async fn typing(&self, _on: bool) {}
}
