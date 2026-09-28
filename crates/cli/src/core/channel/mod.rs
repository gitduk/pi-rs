//! Input channels: a chat platform the session can be driven from.
//!
//! A platform implements `Channel` — how to connect, how to send one message,
//! what its messages may hold — and nothing else. What the phone gets back is
//! the `Relay`'s, written once for every platform: the answer to the turn it
//! asked for, whole, cut into ordered pieces when it is long, and nothing else.

mod wechat;

use std::sync::Arc;
use std::time::Duration;

use agent::Event;
use async_trait::async_trait;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::input::ChannelCmd;

pub use wechat::WeChat;

/// What a channel hands the surface.
pub enum Inbound {
    // A text message from the peer.
    Text { text: String },
    // Something worth saying on the local terminal and worth finding again: a
    // status, an error, the QR. The QR has to stay long enough to be scanned,
    // and a way out of a broken token has to stay readable, so the line lands
    // in the scrollback rather than on the bar.
    Notice(String),
    // The same, on the bar for a moment and no longer: news about the channel
    // itself that nobody reads twice.
    Flash(String),
    // The peer typed `/stop` or `/esc`: interrupt the running turn now.
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

// Room held back for the `(n/m)` marker so a piece plus its marker still fits
// the budget. Ten bytes at three digits a side, rounded up.
const MARKER_RESERVE: usize = 12;

// The pause between pieces of one split message. A platform may rate-limit,
// and a burst is what a rate limiter watches for; at this length the reader
// cannot tell.
const PIECE_INTERVAL: Duration = Duration::from_millis(500);

/// One channel and the turn it owes an answer to.
pub struct Relay {
    channel: Arc<dyn Channel>,
    inbox: Inbox,
    abort: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    // Lanes whose turn this channel asked for, each with its answer so far.
    // Nothing else is relayed: a platform may cap replies per inbound message.
    asked: Vec<(u64, String)>,
    // Whether the indicator is currently marked on. Optimistic: a failed
    // send only leaves the mark stale, never freezes the surface.
    typing_on: bool,
    // The outbound task last spawned. The next one awaits it, so a split
    // answer's tail cannot be overtaken by the next answer.
    last_send: Option<JoinHandle<()>>,
    // Stops the chain above. A single send was over before `off` could
    // matter; a split one runs for seconds, and until this the phone kept
    // receiving pieces after the channel had reported itself stopped.
    outbound: CancellationToken,
}

impl Relay {
    fn new(channel: Arc<dyn Channel>, tx: UnboundedSender<(&'static str, Inbound)>) -> Self {
        let inbox = Inbox {
            from: channel.name(),
            tx,
        };
        Self {
            channel,
            inbox,
            abort: None,
            task: None,
            asked: Vec::new(),
            typing_on: false,
            last_send: None,
            outbound: CancellationToken::new(),
        }
    }

    fn name(&self) -> &'static str {
        self.channel.name()
    }

    // Whether the channel's task is still running. The stored handle
    // outlives its task, so a finished one must read as off.
    fn alive(&self) -> bool {
        self.task.as_ref().is_some_and(|h| !h.is_finished())
    }

    fn status(&self) -> Vec<String> {
        let name = self.name();
        if self.alive() {
            vec![format!("{name}: connected — /{name} off to disconnect")]
        } else {
            vec![format!("{name}: off — /{name} on to connect")]
        }
    }

    fn on(&mut self) -> Vec<String> {
        let name = self.name();
        if self.alive() {
            return vec![format!("{name} is already connected")];
        }
        let abort = CancellationToken::new();
        let channel = self.channel.clone();
        let inbox = self.inbox.clone();
        let task_abort = abort.clone();
        self.task = Some(tokio::spawn(
            async move { channel.run(inbox, task_abort).await },
        ));
        self.abort = Some(abort);
        vec![format!("{name} starting — progress follows in the session")]
    }

    fn off(&mut self) -> Vec<String> {
        if let Some(abort) = self.abort.take() {
            abort.cancel();
        }
        self.task = None;
        self.outbound.cancel();
        self.outbound = CancellationToken::new();
        self.last_send = None;
        self.typing_on = false;
        self.asked.clear();
        let name = self.name();
        vec![format!("{name} stopped — /{name} on to reconnect")]
    }

    // The channel's message became, or joined, the turn running on `lane`.
    // Joining keeps what the turn already said for it.
    fn ask(&mut self, lane: u64) {
        if self.asked.iter().any(|(l, _)| *l == lane) {
            return;
        }
        self.asked.push((lane, String::new()));
        self.typing(true);
    }

    // Tool calls, retries and warnings stay on the terminal: each would be
    // one more message out of what the platform allows per question.
    fn observe(&mut self, lane: u64, event: &Event) {
        let Some(at) = self.asked.iter().position(|(l, _)| *l == lane) else {
            return;
        };
        match event {
            Event::TextDelta(text) => self.asked[at].1.push_str(text),
            Event::Done { .. } => self.flush(at, false),
            _ => {}
        }
    }

    // A successful run already flushed on `Done`; a cancelled or failed
    // one sends what it has.
    fn finish_turn(&mut self, lane: u64, cancelled: bool) {
        if let Some(at) = self.asked.iter().position(|(l, _)| *l == lane) {
            self.flush(at, cancelled);
        }
    }

    fn flush(&mut self, at: usize, cancelled: bool) {
        let (_, mut text) = self.asked.swap_remove(at);
        if cancelled && !text.trim().is_empty() {
            text.push_str("\n\n(stopped)");
        }
        if !text.trim().is_empty() {
            let formatted = self.channel.format(&text);
            if !formatted.is_empty() {
                self.send_line(&formatted);
            }
        }
        if self.asked.is_empty() {
            self.typing(false);
        }
    }

    // One outbound message, sent from its own task so a slow or failing
    // send cannot stall the surface that called it. Failures land on the
    // local terminal rather than vanishing: a platform may rate-limit or
    // block, and that has to be visible here.
    fn send_line(&mut self, text: &str) {
        // `off` keeps the credentials, so an ended channel would keep sending.
        if !self.alive() {
            return;
        }
        let channel = self.channel.clone();
        let inbox = self.inbox.clone();
        let pieces = split(text, channel.limit());
        let previous = self.last_send.take();
        let stop = self.outbound.clone();
        let handle = tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let total = pieces.len();
            for (i, piece) in pieces.into_iter().enumerate() {
                if stop.is_cancelled() {
                    break;
                }
                if i > 0 {
                    tokio::select! {
                        () = tokio::time::sleep(PIECE_INTERVAL) => {}
                        () = stop.cancelled() => break,
                    }
                }
                // A later piece without the ones before it reads as garbage,
                // so a failed send ends the message rather than skipping a hole.
                if let Err(e) = channel.send(&piece).await {
                    let part = if total > 1 {
                        format!(" (piece {}/{total})", i + 1)
                    } else {
                        String::new()
                    };
                    let _ = inbox.send(Inbound::Notice(format!(
                        "{} send failed{part}: {e:#}",
                        channel.name()
                    )));
                    break;
                }
            }
        });
        self.last_send = Some(handle);
    }

    // The typing indicator, sent from a background task so the send can
    // never stall the surface.
    fn typing(&mut self, on: bool) {
        if !self.alive() || self.typing_on == on {
            return;
        }
        self.typing_on = on;
        let channel = self.channel.clone();
        tokio::spawn(async move { channel.typing(on).await });
    }
}

/// Every channel this build knows, feeding one inbound queue stamped with
/// the channel each message came from.
pub struct Channels {
    relays: Vec<Relay>,
    pub(crate) rx: UnboundedReceiver<(&'static str, Inbound)>,
}

impl Channels {
    pub fn new(channels: Vec<Arc<dyn Channel>>) -> Self {
        let (tx, rx) = unbounded_channel();
        let relays = channels
            .into_iter()
            .map(|c| Relay::new(c, tx.clone()))
            .collect();
        Self { relays, rx }
    }

    /// What `/<name>` with `cmd` answers.
    pub fn command(&mut self, name: &str, cmd: ChannelCmd) -> Vec<String> {
        let Some(relay) = self.relay(name) else {
            return vec![format!("{name}: not in this build")];
        };
        match cmd {
            ChannelCmd::Status => relay.status(),
            ChannelCmd::On => relay.on(),
            ChannelCmd::Off => relay.off(),
        }
    }

    /// `name`'s message started, or was steered into, the turn on `lane`:
    /// that turn's answer is owed to it.
    pub fn ask(&mut self, name: &str, lane: u64) {
        if let Some(relay) = self.relay(name) {
            relay.ask(lane);
        }
    }

    /// One event from any lane's run; each relay keeps only its own turn's.
    pub fn observe(&mut self, lane: u64, event: &Event) {
        for relay in &mut self.relays {
            relay.observe(lane, event);
        }
    }

    pub fn finish_turn(&mut self, lane: u64, cancelled: bool) {
        for relay in &mut self.relays {
            relay.finish_turn(lane, cancelled);
        }
    }

    fn relay(&mut self, name: &str) -> Option<&mut Relay> {
        self.relays.iter_mut().find(|r| r.name() == name)
    }
}

// Cut an outbound message into pieces that each fit `limit` bytes. One that
// already fits comes back whole and unmarked; anything longer is marked
// `(n/m)`, so a reader on the phone can tell a message still arriving from one
// that ended — a send the server blocks reports on the local terminal only,
// and the phone would otherwise see a truncated answer as the whole answer.
fn split(text: &str, limit: usize) -> Vec<String> {
    // Against the real limit, not the loop's smaller budget: text that fits
    // unmarked should go out unmarked rather than become two marked pieces.
    if text.len() <= limit {
        return vec![text.to_string()];
    }
    let budget = limit.saturating_sub(MARKER_RESERVE).max(1);
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= budget {
            pieces.push(rest.to_string());
            break;
        }
        let (cut, skip) = boundary(rest, budget);
        pieces.push(rest[..cut].to_string());
        rest = &rest[cut + skip..];
    }
    let total = pieces.len();
    if total > 1 {
        for (i, piece) in pieces.iter_mut().enumerate() {
            piece.insert_str(0, &format!("({}/{total}) ", i + 1));
        }
    }
    pieces
}

// Where to end a piece that overflows `budget`, and how many separator bytes
// to drop after it: the last paragraph break within reach, else the last line
// break, else the last space, else the last character boundary. A hard cut
// drops nothing — indentation inside a code block is content, and the halves
// have to rejoin exactly. A boundary in the first half of the budget is worse
// than no boundary at all: taking it doubles the number of messages.
//
// The cut is never zero: a budget shorter than the first character would
// otherwise leave the caller's loop exactly where it started, forever.
fn boundary(rest: &str, budget: usize) -> (usize, usize) {
    let first = rest.chars().next().map_or(1, char::len_utf8);
    let mut end = budget.max(first);
    while end > first && !rest.is_char_boundary(end) {
        end -= 1;
    }
    let head = &rest[..end];
    for sep in ["\n\n", "\n", " "] {
        let Some(i) = head.rfind(sep).filter(|&i| i > end / 2) else {
            continue;
        };
        let skip = if sep == " " {
            1
        } else {
            rest[i..]
                .bytes()
                .take_while(|b| matches!(b, b'\n' | b'\r'))
                .count()
        };
        return (i, skip);
    }
    (end, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // A platform that is up until told otherwise and keeps what it was sent.
    #[derive(Default)]
    struct Fake {
        sent: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Channel for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn limit(&self) -> usize {
            40
        }
        fn format(&self, markdown: &str) -> String {
            markdown.to_string()
        }
        async fn run(&self, _inbox: Inbox, abort: CancellationToken) {
            abort.cancelled().await;
        }
        async fn send(&self, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    const LANE: u64 = 7;

    fn connected(fake: &Arc<Fake>) -> Relay {
        let (tx, _rx) = unbounded_channel();
        let mut relay = Relay::new(fake.clone(), tx);
        relay.on();
        relay
    }

    // Every send chains on the one before, so awaiting the last awaits all.
    async fn sent(relay: &mut Relay, fake: &Fake) -> Vec<String> {
        if let Some(last) = relay.last_send.take() {
            last.await.unwrap();
        }
        fake.sent.lock().unwrap().clone()
    }

    fn done() -> Event {
        Event::Done {
            turns: 1,
            usage: Default::default(),
            ctx: (0, 0),
            compactions: 0,
        }
    }

    #[test]
    fn a_run_with_no_boundary_is_cut_on_a_character_and_rejoins_exactly() {
        let text = "中".repeat(200);
        let pieces = split(&text, 100);
        assert!(pieces.len() > 1);
        let rejoined: String = pieces
            .iter()
            .map(|p| p.split_once(") ").expect("marker").1)
            .collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn a_budget_shorter_than_one_character_still_advances() {
        let pieces = split("中文中文", 1);
        assert_eq!(pieces.len(), 4);
    }

    #[tokio::test]
    async fn a_turn_typed_at_the_terminal_reaches_nobody() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.observe(LANE, &Event::TextDelta("local".into()));
        b.observe(LANE, &done());
        b.finish_turn(LANE, false);
        assert!(sent(&mut b, &fake).await.is_empty());
    }

    #[tokio::test]
    async fn an_asked_turn_sends_its_answer_and_nothing_about_tools() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(
            LANE,
            &Event::ToolStart {
                id: "1".into(),
                name: "read".into(),
                args: serde_json::json!({}),
            },
        );
        b.observe(LANE, &Event::Warning("careful".into()));
        b.observe(LANE, &Event::TextDelta("the answer".into()));
        b.observe(LANE, &done());
        b.finish_turn(LANE, false);
        assert_eq!(sent(&mut b, &fake).await, ["the answer"]);
    }

    #[tokio::test]
    async fn another_lanes_text_stays_out_of_the_answer() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(LANE + 1, &Event::TextDelta("elsewhere".into()));
        b.observe(LANE, &Event::TextDelta("here".into()));
        b.observe(LANE + 1, &done());
        b.observe(LANE, &done());
        assert_eq!(sent(&mut b, &fake).await, ["here"]);
    }

    #[tokio::test]
    async fn once_answered_the_next_turn_on_that_lane_is_the_terminals() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("asked".into()));
        b.observe(LANE, &done());
        b.finish_turn(LANE, false);
        b.observe(LANE, &Event::TextDelta("typed".into()));
        b.observe(LANE, &done());
        b.finish_turn(LANE, false);
        assert_eq!(sent(&mut b, &fake).await, ["asked"]);
    }

    #[tokio::test]
    async fn a_second_message_into_the_same_turn_keeps_what_was_said() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("one ".into()));
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("two".into()));
        b.observe(LANE, &done());
        assert_eq!(sent(&mut b, &fake).await, ["one two"]);
    }

    #[tokio::test]
    async fn asking_on_a_second_lane_leaves_the_first_answer_owed() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("first".into()));
        b.ask(LANE + 1);
        b.observe(LANE + 1, &Event::TextDelta("second".into()));
        b.observe(LANE + 1, &done());
        b.observe(LANE, &done());
        assert_eq!(sent(&mut b, &fake).await, ["second", "first"]);
    }

    #[tokio::test]
    async fn a_stopped_turn_sends_what_it_had_and_says_so() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("half".into()));
        b.finish_turn(LANE, true);
        assert_eq!(sent(&mut b, &fake).await, ["half\n\n(stopped)"]);
    }

    #[tokio::test]
    async fn a_long_answer_arrives_in_marked_pieces_that_each_fit() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("word ".repeat(12)));
        b.observe(LANE, &done());
        let sent = sent(&mut b, &fake).await;
        assert!(sent.len() > 1, "{sent:?}");
        assert!(sent[0].starts_with("(1/"), "{sent:?}");
        assert!(sent.iter().all(|m| m.len() <= 40), "{sent:?}");
    }

    #[tokio::test]
    async fn a_channel_that_is_off_sends_nothing() {
        let fake = Arc::new(Fake::default());
        let mut b = connected(&fake);
        b.off();
        b.ask(LANE);
        b.observe(LANE, &Event::TextDelta("hello".into()));
        b.finish_turn(LANE, false);
        assert!(b.last_send.is_none());
        assert!(fake.sent.lock().unwrap().is_empty());
    }
}
