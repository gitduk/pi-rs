//! What must not leave the machine, and what comes back in place of it.
//!
//! A value is sent as a token and the token is turned back into the value on
//! the way in, so a run that reads a key out of a file and pastes it into a
//! command still works while the vendor only ever sees the token.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Result;
use crate::message::{
    AssistantContent, Image, Message, ReasoningContent, ToolResultContent, UserContent,
};
use crate::model::ModelSpec;
use crate::request::Request;
use crate::stream::{BlockKind, StreamEvent};
use crate::transport::Transport;

/// Where every token begins: the scan looks for this before it looks for a
/// token, so text full of `{{` costs one pass.
const OPEN: &str = "{{PI_";

/// 40 bits of a keyed digest: enough to tell a handful of values apart, and
/// keyed so a token seen on the wire cannot be checked against a guess.
const ID_BYTES: usize = 5;

#[derive(Debug)]
pub struct Mask {
    key: [u8; 32],
    // In the order they were added; a bucket holds indices into this.
    entries: Vec<Entry>,
    by_value: HashMap<String, usize>,
    by_token: HashMap<String, String>,
    // Which first bytes a value can start with, so a megabyte body is one pass.
    starts: [bool; 256],
    buckets: [Vec<usize>; 256],
    longest_token: usize,
}

#[derive(Debug)]
struct Entry {
    token: String,
    kind: String,
    tier: Tier,
    value: String,
}

// How many times each token has left, across every set this process built: a
// wire may hold an older set than the one armed now, and the count is about
// what the vendor saw, not about which instance saw it.
static HITS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn counted(token: &str) -> u64 {
    HITS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(token)
        .copied()
        .unwrap_or(0)
}

fn count(token: &str) {
    *HITS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(token.to_string())
        .or_default() += 1;
}

/// How a value came to be masked: the owner named it, or a shape rule only
/// guessed at it. A guess is the one worth being able to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Named,
    Guessed,
}

/// One value as a listing shows it: the token the vendor sees, the kind it
/// names, and how much of the value a person may be told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    pub token: String,
    /// The kind the token names: a listing groups by it.
    pub kind: String,
    pub tier: Tier,
    /// The first characters, which for a key are its scheme rather than its
    /// secret: enough to recognise the value, not enough to use it.
    pub head: String,
    /// The last characters, by the same rule: together the two ends identify
    /// a value without spelling it.
    pub tail: String,
    /// Characters, not bytes.
    pub len: usize,
    pub hits: u64,
}

impl Footprint {
    /// The token without the braces that mark it on the wire: what a listing
    /// shows, since a reader is looking at names rather than at something to
    /// paste back.
    pub fn bare(&self) -> &str {
        self.token
            .strip_prefix(OPEN)
            .and_then(|rest| rest.strip_suffix("}}"))
            .unwrap_or(&self.token)
    }
}

/// How much of a value a listing shows: its scheme for a long one, one
/// character for a value short enough that four would be half of it.
const HEAD: usize = 4;

fn head_chars(value: &str) -> usize {
    match value.chars().count() {
        0..12 => 1,
        _ => HEAD,
    }
}

impl Mask {
    /// A mask built on the machine's own secret: the same value gets the
    /// same token every run, and no other machine's does.
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key,
            entries: Vec::new(),
            by_value: HashMap::new(),
            by_token: HashMap::new(),
            starts: [false; 256],
            buckets: std::array::from_fn(|_| Vec::new()),
            longest_token: 0,
        }
    }

    /// Register a value the owner named, under the kind it is — which the
    /// token then names. A value already listed keeps its first token and its
    /// first kind.
    pub fn add(&mut self, kind: &str, value: &str) {
        self.insert(kind, Tier::Named, value);
    }

    /// Register a value a shape rule guessed at: the kind is what the rule
    /// said it was, and the listing says the mask rests on a guess.
    pub fn add_guessed(&mut self, kind: &str, value: &str) {
        self.insert(kind, Tier::Guessed, value);
    }

    fn insert(&mut self, label: &str, tier: Tier, value: &str) {
        if value.is_empty() || self.by_value.contains_key(value) {
            return;
        }
        let token = self.allocate(label, value);
        let first = value.as_bytes()[0] as usize;
        self.starts[first] = true;
        self.buckets[first].push(self.entries.len());
        self.longest_token = self.longest_token.max(token.len());
        self.by_value.insert(value.to_string(), self.entries.len());
        self.by_token.insert(token.clone(), value.to_string());
        self.entries.push(Entry {
            token,
            kind: label.to_string(),
            tier,
            value: value.to_string(),
        });
    }

    /// Every value in `text` as its token. Nothing else is touched.
    pub fn cover(&self, text: &str) -> String {
        if !self.might_match(text) {
            return text.to_string();
        }
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        while at < bytes.len() {
            let Some(off) = bytes[at..].iter().position(|b| self.starts[*b as usize]) else {
                out.push_str(&text[at..]);
                break;
            };
            let pos = at + off;
            out.push_str(&text[at..pos]);
            match self.longest_at(text, pos) {
                Some(entry) => {
                    count(&entry.token);
                    out.push_str(&entry.token);
                    at = pos + entry.value.len();
                }
                None => {
                    let c = text[pos..].chars().next().expect("a char boundary");
                    out.push(c);
                    at = pos + c.len_utf8();
                }
            }
        }
        out
    }

    // A field with no value in it is most of a request: it stays where it is
    // rather than being copied and put back.
    fn cover_field(&self, text: &mut String) {
        if !self.might_match(text) {
            return;
        }
        *text = self.cover(text);
    }

    fn might_match(&self, text: &str) -> bool {
        text.as_bytes().iter().any(|b| self.starts[*b as usize])
    }

    /// The same request with every text it carries covered: this is what a
    /// wire is handed. An opaque payload is left exactly as it came — a
    /// signature is verified byte for byte, and image data is not text.
    pub fn cover_request(&self, req: &Request) -> Request {
        let mut out = req.clone();
        if self.is_empty() {
            return out;
        }
        if let Some(system) = out.system.as_mut() {
            self.cover_field(system);
        }
        for message in &mut out.messages {
            self.cover_message(message);
        }
        for tool in &mut out.tools {
            self.cover_field(&mut tool.description);
            self.map_json(&mut tool.input_schema, &|text| self.cover(text));
        }
        out
    }

    fn cover_message(&self, message: &mut Message) {
        match message {
            Message::System { content } => *content = self.cover(content),
            Message::User { content } => {
                for part in content {
                    match part {
                        UserContent::Text(text) => self.cover_field(&mut text.text),
                        UserContent::ToolResult(result) => {
                            for part in &mut result.content {
                                match part {
                                    ToolResultContent::Text(text) => {
                                        self.cover_field(&mut text.text);
                                    }
                                    ToolResultContent::Json { value } => {
                                        self.map_json(value, &|text| self.cover(text));
                                    }
                                    ToolResultContent::Image(_) => {}
                                }
                            }
                        }
                        // A URL is text; the encoded bytes beside it are not.
                        UserContent::Image(Image::Url { url }) => *url = self.cover(url),
                        UserContent::Image(Image::Base64 { .. }) => {}
                    }
                }
            }
            Message::Assistant { content } => {
                for part in content {
                    match part {
                        AssistantContent::Text(text) => self.cover_field(&mut text.text),
                        AssistantContent::ToolCall(call) => {
                            self.map_json(&mut call.args, &|text| self.cover(text));
                        }
                        AssistantContent::Reasoning(reasoning) => {
                            for part in &mut reasoning.content {
                                // The prose is covered; a signature is not, so
                                // replaying it lands on the bytes it was made for.
                                if let ReasoningContent::Text { text, .. } = part {
                                    *text = self.cover(text);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Every string a JSON tree holds. Keys are structure and stay as they are.
    fn map_json(&self, value: &mut Value, f: &dyn Fn(&str) -> String) {
        match value {
            Value::String(text) => *text = f(text),
            Value::Array(items) => {
                for item in items {
                    self.map_json(item, f);
                }
            }
            Value::Object(map) => {
                // Structure is normally left alone, but a key that holds a
                // value is a leak in the one place the model never looks.
                let rewritten: Vec<(String, String)> = (map.keys())
                    .filter(|key| self.might_match(key))
                    .map(|key| (key.clone(), f(key)))
                    .collect();
                for (was, now) in rewritten {
                    if was != now
                        && let Some(item) = map.remove(&was)
                    {
                        map.insert(now, item);
                    }
                }
                for item in map.values_mut() {
                    self.map_json(item, f);
                }
            }
            _ => {}
        }
    }

    /// Every token in `text` as its value. A token this mask never issued is
    /// prose and stays as it was written.
    pub fn restore(&self, text: &str) -> String {
        if self.by_token.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let held = self.rewrite(text, false, &mut out);
        out.push_str(&text[text.len() - held..]);
        out
    }

    /// How many values are masked. What a diagnostic may say without saying
    /// any of them.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every value as the listing shows it, in the order they were found.
    pub fn footprints(&self) -> Vec<Footprint> {
        self.entries
            .iter()
            .map(|entry| {
                let chars: Vec<char> = entry.value.chars().collect();
                let shown = head_chars(&entry.value);
                Footprint {
                    token: entry.token.clone(),
                    kind: entry.kind.clone(),
                    tier: entry.tier,
                    head: chars.iter().take(shown).collect(),
                    tail: chars
                        .iter()
                        .skip(chars.len().saturating_sub(shown))
                        .collect(),
                    len: chars.len(),
                    hits: counted(&entry.token),
                }
            })
            .collect()
    }

    /// Write `text` into `out`, restoring what is whole, and answer how many
    /// trailing bytes were held back as a token the caller may still complete.
    fn rewrite(&self, text: &str, json: bool, out: &mut String) -> usize {
        let mut at = 0;
        loop {
            let Some(found) = text[at..].find(OPEN) else {
                let held = self.hold(&text[at..]);
                out.push_str(&text[at..text.len() - held]);
                return held;
            };
            let start = at + found;
            let rest = &text[start..];
            match self.token_at(rest) {
                Some((token, value)) => {
                    out.push_str(&text[at..start]);
                    match json {
                        true => out.push_str(&escaped(value)),
                        false => out.push_str(value),
                    }
                    at = start + token.len();
                }
                // The buffer ends inside a token: the caller's next delta may
                // finish it, so the tail waits rather than being spent.
                None => {
                    let held = self.hold(rest);
                    if held > 0 {
                        out.push_str(&text[at..text.len() - held]);
                        return held;
                    }
                    out.push_str(&text[at..start + OPEN.len()]);
                    at = start + OPEN.len();
                }
            }
        }
    }

    fn token_at(&self, rest: &str) -> Option<(&str, &str)> {
        self.by_token
            .iter()
            .filter(|(token, _)| rest.starts_with(token.as_str()))
            .max_by_key(|(token, _)| token.len())
            .map(|(token, value)| (token.as_str(), value.as_str()))
    }

    // The longest tail of `tail` that could still become a token. Every token
    // opens on `{`, so only those byte offsets can start one.
    fn hold(&self, tail: &str) -> usize {
        if self.by_token.is_empty() || tail.len() >= self.longest_token {
            return 0;
        }
        let bytes = tail.as_bytes();
        let begin = bytes.len().saturating_sub(self.longest_token - 1);
        for pos in begin..bytes.len() {
            if bytes[pos] != b'{' {
                continue;
            }
            let piece = &tail[pos..];
            if self.by_token.keys().any(|t| t.starts_with(piece)) {
                return bytes.len() - pos;
            }
        }
        0
    }

    fn longest_at(&self, text: &str, pos: usize) -> Option<&Entry> {
        self.buckets[text.as_bytes()[pos] as usize]
            .iter()
            .filter_map(|i| self.entries.get(*i))
            .filter(|e| text[pos..].starts_with(&e.value))
            .max_by_key(|e| e.value.len())
    }

    // The digest covers the value alone: one value is one secret, whichever
    // kind it was first listed under, and a second kind must not split it.
    fn allocate(&self, label: &str, value: &str) -> String {
        let mut salt = 0u64;
        loop {
            let token = self.token(label, value, salt);
            match self.by_token.get(&token) {
                Some(taken) if taken != value => salt += 1,
                _ => return token,
            }
        }
    }

    fn token(&self, label: &str, value: &str, salt: u64) -> String {
        let mut digest = Sha256::new();
        digest.update(self.key);
        digest.update(salt.to_le_bytes());
        digest.update(value.as_bytes());
        let sum = digest.finalize();
        let id: String = sum[..ID_BYTES].iter().map(|b| format!("{b:02x}")).collect();
        format!("{{{{PI_{label}_{id}}}}}")
    }
}

// A value landing inside a JSON string has to carry that string's escapes.
fn escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).expect("a string serialises");
    quoted[1..quoted.len() - 1].to_string()
}

/// What turns a stream of deltas back into prose: text that ends inside a
/// token waits for the delta that completes it, kept apart by block index.
#[derive(Debug)]
pub struct Restorer {
    mask: Arc<Mask>,
    held: BTreeMap<usize, String>,
}

impl Restorer {
    pub fn new(mask: Arc<Mask>) -> Self {
        Self {
            mask,
            held: BTreeMap::new(),
        }
    }

    /// The part of `delta` that can be read now, continuing a token the last
    /// delta left open on `index`.
    pub fn push(&mut self, index: usize, delta: &str) -> String {
        self.write(index, delta, false)
    }

    /// `push` for the raw JSON text of a tool call's arguments.
    pub fn push_json(&mut self, index: usize, delta: &str) -> String {
        self.write(index, delta, true)
    }

    /// Whatever `index` still held: the model's own text, cut in half.
    pub fn flush(&mut self, index: usize) -> String {
        self.held.remove(&index).unwrap_or_default()
    }

    /// Every held tail, in block order.
    pub fn flush_all(&mut self) -> Vec<(usize, String)> {
        std::mem::take(&mut self.held).into_iter().collect()
    }

    fn write(&mut self, index: usize, delta: &str, json: bool) -> String {
        let mut out = String::with_capacity(delta.len() + 4);
        match self.held.remove(&index) {
            // Nothing was left open, which is every delta but a few: written
            // where it lies rather than joined first.
            None => {
                let held = self.mask.rewrite(delta, json, &mut out);
                if held > 0 {
                    self.held
                        .insert(index, delta[delta.len() - held..].to_string());
                }
            }
            Some(carry) => {
                let text = format!("{carry}{delta}");
                let held = self.mask.rewrite(&text, json, &mut out);
                if held > 0 {
                    self.held
                        .insert(index, text[text.len() - held..].to_string());
                }
            }
        }
        out
    }
}

/// The set a process sends through: armed by whoever resolved the config,
/// read by a wire at each request. One cell rather than one set per wire, so
/// a value found later is covered from the next request on, and a run with two
/// checkouts open masks what either of them named.
#[derive(Debug, Default)]
pub struct Armed(RwLock<Option<Arc<Mask>>>);

impl Armed {
    pub fn arm(&self, mask: Option<Arc<Mask>>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = mask;
    }

    pub fn current(&self) -> Option<Arc<Mask>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// A wire whose traffic is covered: what leaves carries tokens, and what
/// comes back carries the values again.
pub struct Masked {
    inner: Arc<dyn Transport>,
    armed: Arc<Armed>,
}

impl Masked {
    pub fn new(inner: Arc<dyn Transport>, armed: Arc<Armed>) -> Self {
        Self { inner, armed }
    }
}

#[async_trait]
impl Transport for Masked {
    fn gaps(&self) -> Vec<String> {
        self.inner.gaps()
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let Some(mask) = self.armed.current() else {
            return self.inner.stream(spec, req).await;
        };
        let covered = mask.cover_request(req);
        let events = self.inner.stream(spec, &covered).await?;
        Ok(Restored::new(events, mask).boxed())
    }
}

// The wire's events, restored one by one. A tail held back at a block's end is
// handed over first, so nothing the model wrote is dropped between events.
struct Restored {
    events: BoxStream<'static, Result<StreamEvent>>,
    mask: Arc<Mask>,
    restorer: Restorer,
    // What each open block is, so a held tail comes back as the kind of delta
    // the block began with.
    kinds: BTreeMap<usize, BlockKind>,
    queued: VecDeque<StreamEvent>,
    ended: bool,
}

impl Restored {
    fn new(events: BoxStream<'static, Result<StreamEvent>>, mask: Arc<Mask>) -> Self {
        Self {
            restorer: Restorer::new(Arc::clone(&mask)),
            events,
            mask,
            kinds: BTreeMap::new(),
            queued: VecDeque::new(),
            ended: false,
        }
    }

    fn absorb(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::BlockStart { index, kind } => {
                self.kinds.insert(index, kind.clone());
                self.queued
                    .push_back(StreamEvent::BlockStart { index, kind });
            }
            StreamEvent::TextDelta { index, delta } => {
                let delta = self.restorer.push(index, &delta);
                self.queued
                    .push_back(StreamEvent::TextDelta { index, delta });
            }
            StreamEvent::ReasoningDelta { index, delta } => {
                let delta = self.restorer.push(index, &delta);
                self.queued
                    .push_back(StreamEvent::ReasoningDelta { index, delta });
            }
            StreamEvent::ToolArgsDelta { index, delta } => {
                let delta = self.restorer.push_json(index, &delta);
                self.queued
                    .push_back(StreamEvent::ToolArgsDelta { index, delta });
            }
            StreamEvent::BlockEnd { index } => {
                self.release(index);
                self.kinds.remove(&index);
                self.queued.push_back(StreamEvent::BlockEnd { index });
            }
            StreamEvent::Done { stop, usage } => {
                self.release_all();
                self.queued.push_back(StreamEvent::Done { stop, usage });
            }
            StreamEvent::Complete { content, invalid } => {
                self.release_all();
                let content = content
                    .into_iter()
                    .map(|part| self.restore_content(part))
                    .collect();
                // `invalid` is raw argument text, which only ever goes back
                // out; its tokens are covered again on the way.
                self.queued
                    .push_back(StreamEvent::Complete { content, invalid });
            }
            other => self.queued.push_back(other),
        }
    }

    fn release(&mut self, index: usize) {
        let tail = self.restorer.flush(index);
        if !tail.is_empty() {
            self.queued.push_back(self.tail_event(index, tail));
        }
    }

    fn release_all(&mut self) {
        for (index, tail) in self.restorer.flush_all() {
            if !tail.is_empty() {
                self.queued.push_back(self.tail_event(index, tail));
            }
        }
    }

    fn tail_event(&self, index: usize, tail: String) -> StreamEvent {
        match self.kinds.get(&index) {
            Some(BlockKind::ToolCall { .. }) => StreamEvent::ToolArgsDelta { index, delta: tail },
            Some(BlockKind::Reasoning) | Some(BlockKind::RedactedReasoning { .. }) => {
                StreamEvent::ReasoningDelta { index, delta: tail }
            }
            _ => StreamEvent::TextDelta { index, delta: tail },
        }
    }

    fn restore_content(&self, content: AssistantContent) -> AssistantContent {
        match content {
            AssistantContent::Text(mut text) => {
                text.text = self.mask.restore(&text.text);
                AssistantContent::Text(text)
            }
            AssistantContent::ToolCall(mut call) => {
                self.mask
                    .map_json(&mut call.args, &|text| self.mask.restore(text));
                AssistantContent::ToolCall(call)
            }
            AssistantContent::Reasoning(mut reasoning) => {
                for part in &mut reasoning.content {
                    if let ReasoningContent::Text { text, .. } = part {
                        *text = self.mask.restore(text);
                    }
                }
                AssistantContent::Reasoning(reasoning)
            }
        }
    }
}

impl Stream for Restored {
    type Item = Result<StreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if let Some(event) = this.queued.pop_front() {
                return Poll::Ready(Some(Ok(event)));
            }
            match this.events.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(Some(Ok(event))) => this.absorb(event),
                Poll::Ready(None) => {
                    if this.ended {
                        return Poll::Ready(None);
                    }
                    this.ended = true;
                    this.release_all();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::executor::block_on;
    use futures::stream::{BoxStream, StreamExt};
    use serde_json::json;

    use super::*;
    use crate::message::{
        AssistantContent, Image, Message, Reasoning, ReasoningContent, Text, ToolCall, ToolResult,
        ToolResultContent, UserContent,
    };
    use crate::model::ModelSpec;
    use crate::request::{Request, ToolDef};
    use crate::stream::{BlockKind, StopReason, StreamEvent, Usage};
    use crate::transport::Transport;

    const KEY: [u8; 32] = [7; 32];

    fn mask(key: [u8; 32], values: &[(&str, &str)]) -> Arc<Mask> {
        let mut mask = Mask::new(key);
        for (label, value) in values {
            mask.add(label, value);
        }
        Arc::new(mask)
    }

    // The whole contract: what leaves is not the value, and what comes back is.
    #[test]
    fn a_value_comes_back_from_its_token() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let covered = s.cover("export API_KEY=sk-live-9f3a1c2b7d5e");
        assert!(!covered.contains("sk-live"), "{covered}");
        assert_eq!(s.restore(&covered), "export API_KEY=sk-live-9f3a1c2b7d5e");
    }

    // A prefix of an earlier value must not eat the longer one's head.
    #[test]
    fn the_longer_of_two_overlapping_values_wins() {
        let s = mask(KEY, &[("TOKEN", "sk-abc"), ("API_KEY", "sk-abcdef")]);
        let covered = s.cover("sk-abcdef");
        assert_eq!(s.restore(&covered), "sk-abcdef");
        assert_ne!(covered, format!("{}def", s.cover("sk-abc")));
    }

    // Same value, same token: a conversation's prefix has to read the same
    // turn after turn, or the vendor's cache and a thinking block's signature
    // stop matching. A different key is what keeps a guessed token from being
    // checkable off the machine.
    #[test]
    fn one_value_gets_one_token_and_the_key_decides_which() {
        let value = "ghp_0123456789abcdefghij";
        let here = mask(KEY, &[("API_KEY", value)]);
        let again = mask(KEY, &[("API_KEY", value)]);
        let elsewhere = mask([9; 32], &[("API_KEY", value)]);
        assert_eq!(here.cover(value), again.cover(value));
        assert_ne!(here.cover(value), elsewhere.cover(value));
    }

    // SSE cuts deltas anywhere, so a token arrives in pieces; the tail cannot
    // be forwarded before it is known not to be one.
    #[test]
    fn a_token_split_across_two_deltas_is_restored_by_the_second() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let token = s.cover("sk-live-9f3a1c2b7d5e");
        let (head, tail) = token.split_at(token.len() / 2);
        let mut r = Restorer::new(Arc::clone(&s));
        assert_eq!(r.push(0, &format!("key {head}")), "key ");
        assert_eq!(
            r.push(0, &format!("{tail} done")),
            "sk-live-9f3a1c2b7d5e done"
        );
    }

    // What the model cut in half is still what it wrote: never swallowed.
    #[test]
    fn a_partial_that_never_completes_is_emitted_when_the_block_ends() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let token = s.cover("sk-live-9f3a1c2b7d5e");
        let mut r = Restorer::new(Arc::clone(&s));
        assert_eq!(r.push(0, &format!("wait {}", &token[..4])), "wait ");
        assert_eq!(r.flush(0), &token[..4]);
        assert_eq!(r.flush(0), "");
    }

    // Two blocks can each end with a partial; the order they are handed back
    // in is the order they were read, or a reader sees block 3's tail first.
    #[test]
    fn flushing_every_remainder_keeps_the_blocks_in_order() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let token = s.cover("sk-live-9f3a1c2b7d5e");
        let head = &token[..5];
        let mut r = Restorer::new(Arc::clone(&s));
        r.push(7, head);
        r.push(3, head);
        assert_eq!(
            r.flush_all(),
            vec![(3, head.to_string()), (7, head.to_string())]
        );
    }

    // A token we never issued is prose, not a crash and not a guess.
    #[test]
    fn a_token_that_maps_to_nothing_is_left_as_written() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        assert_eq!(
            s.restore("{{PI_API_KEY_0000000000}}"),
            "{{PI_API_KEY_0000000000}}"
        );
    }

    // Tool arguments are JSON text: the value has to land inside the string
    // escaped, or the block that parses them fails on a quote or a newline.
    #[test]
    fn a_value_with_quotes_comes_back_into_arguments_escaped() {
        let value = "pw\"with\\slashes\nand a newline";
        let s = mask(KEY, &[("PASSWD", value)]);
        let token = s.cover(value);
        let mut r = Restorer::new(Arc::clone(&s));
        let out = r.push_json(0, &format!("{{\"password\":\"{token}\"}}"));
        let parsed: serde_json::Value = serde_json::from_str(&out).expect(&out);
        assert_eq!(parsed["password"], value);
    }

    // Restoring is not a second masking pass: the turn's text may pass here
    // more than once and must not change on the second time.
    #[test]
    fn restoring_twice_changes_nothing() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let once = s.restore(&s.cover("a sk-live-9f3a1c2b7d5e b"));
        assert_eq!(s.restore(&once), once);
    }

    // One value listed under two kinds is still one value: two tokens would
    // split one secret's history in half, and the first listing is the answer.
    #[test]
    fn a_value_listed_twice_keeps_the_first_label_and_one_token() {
        let s = mask(
            KEY,
            &[
                ("EMAIL", "hunter2@corp.example"),
                ("PASSWD", "hunter2@corp.example"),
            ],
        );
        let covered = s.cover("hunter2@corp.example");
        assert_eq!(covered.matches("{{PI_").count(), 1, "{covered}");
        assert!(covered.contains("EMAIL"), "{covered}");
        assert_eq!(s.restore(&covered), "hunter2@corp.example");
    }

    // Text with nothing to cover is the common case by far: it must come
    // through byte for byte, including a `{{` that opens no token.
    #[test]
    fn text_without_a_token_comes_through_unchanged() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let text = "a {{PI_ b {{PI_NOT_A_TOKEN}} c";
        assert_eq!(s.cover(text), text);
        assert_eq!(s.restore(text), text);
    }

    // The listing is where a person can see what was masked, how much of it
    // is shown, and whether the mask rests on a guess: a head of four
    // characters is enough to recognise a value and not enough to use it.
    #[test]
    fn a_listing_names_each_value_by_its_kind_and_both_ends() {
        let mut s = Mask::new(KEY);
        s.add("API_KEY", "sk-listing-0123456789");
        s.add_guessed("TOKEN", "999888777:AAbbCCddEEffGG");
        s.add("PASSWD", "hunter2");
        let text = "sk-listing-0123456789 twice: sk-listing-0123456789";
        let _ = s.cover(text);

        let prints = s.footprints();
        assert_eq!(prints.len(), 3);
        assert!(prints[0].token.contains("API_KEY"), "{:?}", prints[0]);
        assert_eq!(prints[0].tier, Tier::Named);
        assert_eq!(prints[0].kind, "API_KEY");
        assert_eq!(prints[0].head, "sk-l");
        assert_eq!(prints[0].tail, "6789");
        assert_eq!(prints[0].len, 21);
        assert_eq!(prints[0].hits, 2);
        assert_eq!(prints[1].tier, Tier::Guessed);
        assert_eq!(prints[1].hits, 0);
        // Four of a seven-character value would be more than half of it.
        assert_eq!(prints[2].head, "h");
        assert_eq!(prints[2].tail, "2");
        assert_eq!(prints[2].len, 7);
        assert!(prints.iter().all(|p| p.head.chars().count() <= 4));
    }

    // A request carries text in a dozen places and opaque bytes in three. The
    // shape has to survive either way: a masked request is still valid JSON
    // with the same keys and the same numbers.
    #[test]
    fn every_text_a_request_carries_is_covered() {
        let value = "sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[("API_KEY", value)]);
        let covered = s.cover_request(&request_carrying(value, false));
        assert_eq!(covered.messages.len(), 3, "the messages stay put");
        assert_eq!(
            covered.tools[0].input_schema["properties"]["command"]["type"],
            "string"
        );
        let wire = wire_json(&covered);
        assert!(!wire.contains(value), "{wire}");
    }

    // A signature is verified by the vendor byte for byte, an encrypted block
    // is theirs to read, and image data is not text — covering any of them
    // either earns a 400 or spends a scan on a megabyte that cannot match.
    #[test]
    fn a_signature_an_encrypted_payload_and_image_data_go_out_as_they_came() {
        let value = "sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[("API_KEY", value)]);
        let wire = wire_json(&s.cover_request(&request_carrying(value, true)));
        assert!(wire.contains(&format!("sig:{value}")), "{wire}");
        assert!(wire.contains(&format!("enc:{value}")), "{wire}");
        assert!(wire.contains(&format!("AAA{value}")), "{wire}");
        assert!(!wire.contains(&format!("thinking of {value}")), "{wire}");
    }

    // SSE cuts deltas anywhere, and the wire the agent reads must never see
    // the cut: what arrives is prose, not two halves of a token.
    #[test]
    fn a_token_split_across_streamed_deltas_is_restored() {
        let value = "sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[("API_KEY", value)]);
        let token = s.cover(value);
        let (head, tail) = token.split_at(6);
        let (events, _) = drain(
            vec![
                StreamEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    delta: format!("key {head}"),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    delta: format!("{tail} done"),
                },
                StreamEvent::BlockEnd { index: 0 },
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                },
            ],
            &s,
        );
        assert_eq!(text_of(&events), format!("key {value} done"));
    }

    // Tool arguments travel as JSON text: a value with a quote in it has to
    // land escaped, or the block that parses them fails on its own content.
    #[test]
    fn tool_argument_deltas_come_back_escaped() {
        let value = "pw\"with\\slashes";
        let s = mask(KEY, &[("PASSWD", value)]);
        let args = format!("{{\"password\":\"{}\"}}", s.cover(value));
        let (head, tail) = args.split_at(args.len() / 2);
        let (events, _) = drain(
            vec![
                StreamEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::ToolCall {
                        id: Some("c".into()),
                        name: "bash".into(),
                    },
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: head.to_string(),
                },
                StreamEvent::ToolArgsDelta {
                    index: 0,
                    delta: tail.to_string(),
                },
                StreamEvent::BlockEnd { index: 0 },
                StreamEvent::Done {
                    stop: StopReason::ToolUse,
                    usage: Usage::default(),
                },
            ],
            &s,
        );
        let args: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ToolArgsDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        let parsed: serde_json::Value = serde_json::from_str(&args).expect(&args);
        assert_eq!(parsed["password"], value);
    }

    // The covering happens on the far side of the seam: whatever the wire gets
    // to serialize must be the masked request, not the one the loop built.
    #[test]
    fn the_wire_behind_the_mask_is_never_handed_a_value() {
        let value = "sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[("API_KEY", value)]);
        let (_, seen) = drain_with(request_carrying(value, false), Vec::new(), &s);
        let wire = wire_json(&seen.expect("a call"));
        assert!(!wire.contains(value), "{wire}");
    }

    // A wire that ends on `Done` without closing its last block would swallow
    // the model's parting bytes: the held tail is read out before the end.
    #[test]
    fn a_tail_the_stream_never_completes_is_still_read() {
        let s = mask(KEY, &[("API_KEY", "sk-live-9f3a1c2b7d5e")]);
        let token = s.cover("sk-live-9f3a1c2b7d5e");
        let head = &token[..4];
        let (events, _) = drain(
            vec![
                StreamEvent::BlockStart {
                    index: 0,
                    kind: BlockKind::Text,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    delta: format!("cut {head}"),
                },
                StreamEvent::Done {
                    stop: StopReason::EndTurn,
                    usage: Usage::default(),
                },
            ],
            &s,
        );
        assert_eq!(text_of(&events), format!("cut {head}"));
    }

    // The Responses wire states the whole turn at the end instead of folding
    // it from deltas, so this path needs the restore a second time.
    #[test]
    fn a_turn_stated_whole_comes_back_restored() {
        let value = "sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[("API_KEY", value)]);
        let token = s.cover(value);
        let (events, _) = drain(
            vec![StreamEvent::Complete {
                content: vec![
                    AssistantContent::Text(Text {
                        text: format!("the key is {token}"),
                    }),
                    AssistantContent::ToolCall(ToolCall {
                        id: "c".into(),
                        name: "bash".into(),
                        args: json!({ "command": format!("echo {token}") }),
                    }),
                ],
                invalid: Vec::new(),
            }],
            &s,
        );
        let StreamEvent::Complete { content, .. } = &events[0] else {
            panic!("{events:?}")
        };
        let AssistantContent::Text(text) = &content[0] else {
            panic!("{content:?}")
        };
        assert_eq!(text.text, format!("the key is {value}"));
        let AssistantContent::ToolCall(call) = &content[1] else {
            panic!("{content:?}")
        };
        assert_eq!(call.args["command"], format!("echo {value}"));
    }

    // A value found after the wire was made — a script's `.env` edited
    // mid-run — is covered from the next request on, because the wire reads
    // the set as it stands rather than the one it was made with.
    #[test]
    fn a_wire_covers_with_the_set_armed_after_it_was_made() {
        let seen = Arc::new(Mutex::new(None));
        let wire = Canned {
            events: Vec::new(),
            seen: Arc::clone(&seen),
        };
        let armed = Arc::new(Armed::default());
        let masked = Masked::new(Arc::new(wire), Arc::clone(&armed));
        let value = "sk-late-0123456789";
        let spec = ModelSpec::test();
        let request = request_carrying(value, false);
        let call = |seen: &Arc<Mutex<Option<Request>>>| {
            // The stream is never polled: what this test reads is the request
            // the wire was handed, which happens before the first event.
            let _ = block_on(masked.stream(&spec, &request)).expect("a stream");
            wire_json(&seen.lock().expect("unpoisoned").clone().expect("a call"))
        };

        // Nothing armed yet: the request goes as it is.
        assert!(call(&seen).contains(value));

        armed.arm(Some(mask(KEY, &[("API_KEY", value)])));
        assert!(!call(&seen).contains(value));
    }

    // The body a wire builds from a request, as the one string its parts
    // serialize to. `Request` itself carries no serialized form.
    fn wire_json(req: &Request) -> String {
        serde_json::json!({
            "system": &req.system,
            "messages": serde_json::to_value(&req.messages).expect("messages"),
            "tools": &req.tools,
        })
        .to_string()
    }

    // Every place a request can carry `value` as text; with `payloads` the
    // three places a wire must not rewrite carry it too.
    fn request_carrying(value: &str, payloads: bool) -> Request {
        let opaque = |body: String| match payloads {
            true => body,
            false => "opaque".to_string(),
        };
        Request {
            system: Some(format!("the key is {value}")),
            messages: vec![
                Message::User {
                    content: vec![
                        UserContent::Text(Text {
                            text: format!("use {value}"),
                        }),
                        UserContent::Image(Image::Base64 {
                            media_type: "image/png".into(),
                            data: opaque(format!("AAA{value}")),
                        }),
                    ],
                },
                Message::User {
                    content: vec![UserContent::ToolResult(ToolResult {
                        call: "c1".into(),
                        name: "bash".into(),
                        content: vec![
                            ToolResultContent::Text(Text {
                                text: format!("export K={value}"),
                            }),
                            ToolResultContent::Json {
                                value: json!({ "env": format!("K={value}") }),
                            },
                        ],
                        is_error: false,
                    })],
                },
                Message::Assistant {
                    content: vec![
                        AssistantContent::ToolCall(ToolCall {
                            id: "c2".into(),
                            name: "bash".into(),
                            args: json!({ "command": format!("echo {value}"), "n": 3 }),
                        }),
                        AssistantContent::Reasoning(Reasoning {
                            id: None,
                            by: None,
                            content: vec![
                                ReasoningContent::Text {
                                    text: format!("thinking of {value}"),
                                    signature: Some(opaque(format!("sig:{value}"))),
                                },
                                ReasoningContent::Encrypted(opaque(format!("enc:{value}"))),
                            ],
                        }),
                    ],
                },
            ],
            tools: vec![ToolDef {
                name: "bash".into(),
                description: format!("runs {value}"),
                input_schema: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string", "description": format!("uses {value}") } }
                }),
            }],
            ..Request::default()
        }
    }

    #[derive(Clone)]
    struct Canned {
        events: Vec<StreamEvent>,
        seen: Arc<Mutex<Option<Request>>>,
    }

    #[async_trait::async_trait]
    impl Transport for Canned {
        async fn stream(
            &self,
            _: &ModelSpec,
            req: &Request,
        ) -> crate::Result<BoxStream<'static, crate::Result<StreamEvent>>> {
            *self.seen.lock().expect("unpoisoned") = Some(req.clone());
            Ok(futures::stream::iter(self.events.clone().into_iter().map(Ok)).boxed())
        }
    }

    fn drain(events: Vec<StreamEvent>, mask: &Arc<Mask>) -> (Vec<StreamEvent>, Option<Request>) {
        drain_with(Request::default(), events, mask)
    }

    fn drain_with(
        request: Request,
        events: Vec<StreamEvent>,
        mask: &Arc<Mask>,
    ) -> (Vec<StreamEvent>, Option<Request>) {
        let seen = Arc::new(Mutex::new(None));
        let wire = Canned {
            events,
            seen: Arc::clone(&seen),
        };
        let armed = Arc::new(Armed::default());
        armed.arm(Some(Arc::clone(mask)));
        let masked = Masked::new(Arc::new(wire), armed);
        let spec = ModelSpec::test();
        let out = block_on(async {
            let mut stream = masked.stream(&spec, &request).await.expect("a stream");
            let mut out = Vec::new();
            while let Some(ev) = stream.next().await {
                out.push(ev.expect("an event"));
            }
            out
        });
        let request = seen.lock().expect("unpoisoned").clone();
        (out, request)
    }

    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }
}
