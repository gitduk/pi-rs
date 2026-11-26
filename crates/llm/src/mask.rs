//! What must not leave the machine, and what comes back in place of it.
//!
//! A value is sent as a token and the token is turned back into the value on
//! the way in, so a run that reads a key out of a file and pastes it into a
//! command still works while the vendor only ever sees the token.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::message::{
    AssistantContent, Image, Message, ReasoningContent, ToolResultContent, UserContent,
};
use crate::model::ModelSpec;
use crate::request::Request;
use crate::stream::{BlockKind, StreamEvent};
use crate::transport::Transport;
use crate::{LlmError, Result};

type Buckets = [Vec<usize>; 256];

// Discovery borrows the request. Rewriting visits the same fields mutably.
// Both leave signatures and encoded image data untouched.
macro_rules! visit_request_text {
    ($req:ident, $text:expr, $json:expr $(, $mutable:tt)?) => {{
        if let Some(system) = &$($mutable)? $req.system {
            ($text)(system);
        }
        for message in &$($mutable)? $req.messages {
            match message {
                Message::System { content } => ($text)(content),
                Message::User { content } => {
                    for part in content {
                        match part {
                            UserContent::Text(part) => ($text)(&$($mutable)? part.text),
                            UserContent::ToolResult(result) => {
                                for part in &$($mutable)? result.content {
                                    match part {
                                        ToolResultContent::Text(part) => {
                                            ($text)(&$($mutable)? part.text);
                                        }
                                        ToolResultContent::Json { value } => ($json)(value),
                                        ToolResultContent::Image(_) => {}
                                    }
                                }
                            }
                            UserContent::Image(Image::Url { url }) => ($text)(url),
                            UserContent::Image(Image::Base64 { .. }) => {}
                        }
                    }
                }
                Message::Assistant { content } => {
                    for part in content {
                        match part {
                            AssistantContent::Text(part) => ($text)(&$($mutable)? part.text),
                            AssistantContent::ToolCall(call) => ($json)(&$($mutable)? call.args),
                            AssistantContent::Reasoning(reasoning) => {
                                for part in &$($mutable)? reasoning.content {
                                    if let ReasoningContent::Text { text, .. } = part {
                                        ($text)(text);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        for tool in &$($mutable)? $req.tools {
            ($text)(&$($mutable)? tool.description);
            ($json)(&$($mutable)? tool.input_schema);
        }
    }};
}

#[derive(Clone, Debug)]
pub struct Mask {
    key: [u8; 32],
    // In the order they were added; a bucket holds indices into this.
    entries: Vec<Entry>,
    token_buckets: Buckets,
    value_buckets: Buckets,
}

#[derive(Clone, Debug)]
struct Entry {
    token: String,
    name: Option<String>,
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
    let mut hits = HITS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(count) = hits.get_mut(token) {
        *count += 1;
    } else {
        hits.insert(token.to_owned(), 1);
    }
}

/// A masked value's source, abbreviated display, and replacement count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footprint {
    /// What introduced the value; `None` when nothing did.
    pub name: Option<String>,
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

/// How much of a value a listing shows: its scheme for a long one, one
/// character for a value short enough that four would be half of it.
const HEAD: usize = 4;

fn head_chars(len: usize) -> usize {
    match len {
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
            token_buckets: std::array::from_fn(|_| Vec::new()),
            value_buckets: std::array::from_fn(|_| Vec::new()),
        }
    }

    /// Register a value found in text about to leave. A value already listed
    /// keeps its first token and its first name.
    ///
    /// # Errors
    ///
    /// Returns an error if this value has no free token of the same shape.
    pub fn add(&mut self, name: Option<&str>, value: &str) -> Result<()> {
        self.insert(name, value, &HashSet::new())
    }

    fn insert(&mut self, name: Option<&str>, value: &str, pending: &HashSet<&str>) -> Result<()> {
        let Some(&first) = value.as_bytes().first() else {
            return Ok(());
        };
        if self.contains_value(value, first) {
            return Ok(());
        }
        let token = self.allocate(value, pending)?;
        let index = self.entries.len();
        self.token_buckets[token.as_bytes()[0] as usize].push(index);
        self.value_buckets[first as usize].push(index);
        self.entries.push(Entry {
            token,
            name: name.map(str::to_string),
            value: value.to_string(),
        });
        Ok(())
    }

    /// Every value in `text` as its token. Nothing else is touched.
    pub fn cover(&self, text: &str) -> String {
        self.covered(text).into_owned()
    }

    fn covered<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = None;
        let mut copied = 0;
        let mut from = 0;
        while let Some(start) = next_candidate(text, from, &self.value_buckets) {
            let rest = &text[start..];
            let entry = self
                .entries_in(&self.value_buckets[text.as_bytes()[start] as usize])
                .filter(|entry| rest.starts_with(&entry.value))
                .max_by_key(|entry| entry.value.len());
            let Some(entry) = entry else {
                from = start + 1;
                continue;
            };
            count(&entry.token);
            let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
            out.push_str(&text[copied..start]);
            out.push_str(&entry.token);
            copied = start + entry.value.len();
            from = copied;
        }
        finish_rewrite(text, out, copied)
    }

    fn cover_field(&self, text: &mut String) {
        rewrite_field(text, |text| self.covered(text));
    }

    /// The same request with every text it carries covered: this is what a
    /// wire is handed. An opaque payload is left exactly as it came — a
    /// signature is verified byte for byte, and image data is not text.
    pub fn cover_request(&self, req: &Request) -> Request {
        let mut out = req.clone();
        if !self.is_empty() {
            visit_request_text!(
                out,
                |text| self.cover_field(text),
                |value| map_json(value, &|text| self.covered(text)),
                mut
            );
        }
        out
    }

    /// Every token in `text` as its value. A token this mask never issued is
    /// prose and stays as it was written.
    pub fn restore(&self, text: &str) -> String {
        self.restored(text).into_owned()
    }

    fn restored<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let (out, tail) = self.restore_chunk(text, false);
        match out {
            Cow::Borrowed(_) => Cow::Borrowed(text),
            Cow::Owned(mut out) => {
                out.push_str(tail);
                Cow::Owned(out)
            }
        }
    }

    /// How many values are masked. What a diagnostic may say without saying
    /// any of them.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether this set already holds `value`.
    pub fn contains(&self, value: &str) -> bool {
        match value.as_bytes().first() {
            Some(&first) => self.contains_value(value, first),
            None => false,
        }
    }

    /// Whether `value` is a token this set issued: a token seen again in the
    /// text is a mask, not a secret to mask.
    pub fn is_token(&self, value: &str) -> bool {
        match value.as_bytes().first() {
            Some(&first) => self
                .entries_in(&self.token_buckets[first as usize])
                .any(|entry| entry.token == value),
            None => false,
        }
    }

    /// Every value as the listing shows it, in the order they were found.
    pub fn footprints(&self) -> Vec<Footprint> {
        self.entries
            .iter()
            .map(|entry| {
                let len = entry.value.chars().count();
                let shown = head_chars(len);
                Footprint {
                    name: entry.name.clone(),
                    head: crate::slice::head_chars(&entry.value, shown).to_owned(),
                    tail: crate::slice::tail_chars(&entry.value, shown).to_owned(),
                    len,
                    hits: counted(&entry.token),
                }
            })
            .collect()
    }

    /// Restore complete tokens and hold a trailing prefix for the next delta.
    fn restore_chunk<'a>(&self, text: &'a str, json: bool) -> (Cow<'a, str>, &'a str) {
        if self.is_empty() {
            return (Cow::Borrowed(text), "");
        }
        let mut out = None;
        let mut copied = 0;
        let mut from = 0;
        while let Some(start) = next_candidate(text, from, &self.token_buckets) {
            let rest = &text[start..];
            let mut partial = false;
            let entry = self
                .entries_in(&self.token_buckets[text.as_bytes()[start] as usize])
                .find(|entry| {
                    if rest.len() < entry.token.len() {
                        partial |= entry.token.starts_with(rest);
                        false
                    } else {
                        rest.starts_with(&entry.token)
                    }
                });
            if let Some(entry) = entry {
                let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
                out.push_str(&text[copied..start]);
                if json {
                    let quoted = serde_json::to_string(&entry.value).expect("a string serialises");
                    out.push_str(&quoted[1..quoted.len() - 1]);
                } else {
                    out.push_str(&entry.value);
                }
                copied = start + entry.token.len();
                from = copied;
            } else if partial {
                return (finish_rewrite(&text[..start], out, copied), rest);
            } else {
                from = start + 1;
            }
        }
        (finish_rewrite(text, out, copied), "")
    }

    fn entries_in<'a>(&'a self, bucket: &'a [usize]) -> impl Iterator<Item = &'a Entry> {
        bucket.iter().map(|&index| &self.entries[index])
    }

    fn contains_value(&self, value: &str, first: u8) -> bool {
        self.entries_in(&self.value_buckets[first as usize])
            .any(|entry| entry.value == value)
    }

    fn allocate(&self, value: &str, pending: &HashSet<&str>) -> Result<String> {
        let body = value.strip_prefix("Bearer ").unwrap_or(value);
        let body = body.strip_prefix("sk-").unwrap_or(body);
        let body = if body.is_empty() { value } else { body };
        let prefix = &value[..value.len() - body.len()];
        let kind = Kind::of(body);
        let length = body.chars().count();
        let seed = self.expand(value, kind.bytes(length));
        let mut token = String::with_capacity(prefix.len() + length);
        token.push_str(prefix);

        // Each offset visits a distinct candidate. Reserve both sides of
        // existing mappings, the input itself and the rest of its batch.
        for offset in 0..=2 * self.entries.len() + pending.len() + 1 {
            token.truncate(prefix.len());
            if !kind.render(&mut token, &seed, body, offset) {
                break;
            }
            if token != value
                && !pending.contains(token.as_str())
                && !self.is_token(&token)
                && !self.contains(&token)
            {
                return Ok(token);
            }
        }
        Err(LlmError::MaskShape)
    }

    // Expand the seed by hashing once per block of replacement bytes.
    fn expand(&self, value: &str, count: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(count);
        for (round, start) in (0..count).step_by(Sha256::output_size()).enumerate() {
            let mut digest = Sha256::new();
            digest.update(self.key);
            // Keep the first candidate from the original salted format.
            digest.update(0u64.to_le_bytes());
            digest.update((round as u64).to_le_bytes());
            digest.update(value.as_bytes());
            let block = digest.finalize();
            out.extend_from_slice(&block[..block.len().min(count - start)]);
        }
        out
    }
}

/// The shape a token keeps: a digest stays a digest of the same length, so
/// the model can compare, join and count what it reads without seeing it.
enum Kind {
    Hex { upper: bool },
    Base64 { pad: usize },
    Punctuation,
    Text,
}

impl Kind {
    fn of(body: &str) -> Self {
        if !body.is_empty() && body.chars().all(|c| c.is_ascii_hexdigit()) {
            let upper = body.chars().any(|c| c.is_ascii_uppercase())
                && !body.chars().any(|c| c.is_ascii_lowercase());
            return Self::Hex { upper };
        }
        // A body made of punctuation has no data segments to separate.
        if body.chars().all(is_shape_punctuation) {
            return Self::Punctuation;
        }
        if body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_'))
        {
            let pad = body.chars().rev().take_while(|c| *c == '=').count();
            return Self::Base64 { pad };
        }
        Self::Text
    }

    fn bytes(&self, length: usize) -> usize {
        match self {
            Self::Hex { .. } => length.div_ceil(2),
            _ => length,
        }
    }

    // Translate each digit by its seed within the alphabet. An offset with
    // digits left over is past the end of this shape's candidate space.
    fn render(&self, out: &mut String, bytes: &[u8], body: &str, mut offset: usize) -> bool {
        const LOWER: &[u8; 16] = b"0123456789abcdef";
        const UPPER: &[u8; 16] = b"0123456789ABCDEF";
        const BASE64: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        const TEXT: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut next = |alphabet: &[u8], seed: u8| {
            let radix = alphabet.len();
            let digit = (usize::from(seed) + offset % radix) % radix;
            offset /= radix;
            char::from(alphabet[digit])
        };
        match self {
            Self::Hex { upper } => {
                let table = if *upper { UPPER } else { LOWER };
                for index in 0..body.len() {
                    let byte = bytes[index / 2];
                    let nibble = if index % 2 == 0 {
                        byte >> 4
                    } else {
                        byte & 0x0f
                    };
                    out.push(next(table, nibble));
                }
            }
            Self::Base64 { pad } => {
                let letters = body.len() - pad;
                for (index, character) in body.chars().enumerate() {
                    if matches!(character, '-' | '_') {
                        out.push(character);
                    } else if index >= letters {
                        out.push('=');
                    } else {
                        out.push(next(BASE64, bytes[index]));
                    }
                }
            }
            Self::Punctuation => {
                // These characters are safe both in URLs and in JSON text.
                for &byte in bytes {
                    out.push(next(b"-._~", byte));
                }
            }
            Self::Text => {
                for (character, &byte) in body.chars().zip(bytes) {
                    if is_shape_punctuation(character) {
                        out.push(character);
                    } else {
                        out.push(next(TEXT, byte));
                    }
                }
            }
        }
        offset == 0
    }
}

// Punctuation that carries a value's shape and is safe inside JSON text.
fn is_shape_punctuation(character: char) -> bool {
    matches!(
        character,
        '-' | '_' | '.' | ':' | '/' | '@' | '+' | '=' | '~' | '$'
    )
}

fn next_candidate(text: &str, from: usize, buckets: &Buckets) -> Option<usize> {
    // A bucket starts with the first byte of a UTF-8 string, so every match
    // is at a character boundary even though the search walks bytes.
    text.as_bytes()[from..]
        .iter()
        .position(|&byte| !buckets[byte as usize].is_empty())
        .map(|offset| from + offset)
}

fn finish_rewrite(text: &str, out: Option<String>, copied: usize) -> Cow<'_, str> {
    match out {
        Some(mut out) => {
            out.push_str(&text[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(text),
    }
}

fn rewrite_field(text: &mut String, rewrite: impl FnOnce(&str) -> Cow<'_, str>) {
    if let Cow::Owned(changed) = rewrite(text) {
        *text = changed;
    }
}

// Object keys can carry secrets too. Rebuild only those that matched.
fn map_json(value: &mut Value, rewrite: &impl Fn(&str) -> Cow<'_, str>) {
    match value {
        Value::String(text) => rewrite_field(text, rewrite),
        Value::Array(items) => {
            for item in items {
                map_json(item, rewrite);
            }
        }
        Value::Object(map) => {
            let rewritten: Vec<_> = map
                .keys()
                .filter_map(|key| match rewrite(key) {
                    Cow::Owned(changed) if key != &changed => Some((key.clone(), changed)),
                    _ => None,
                })
                .collect();
            for (was, now) in rewritten {
                if let Some(item) = map.remove(&was) {
                    map.insert(now, item);
                }
            }
            for item in map.values_mut() {
                map_json(item, rewrite);
            }
        }
        _ => {}
    }
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
        let mut carry = self.held.remove(&index);
        let text: &str = if let Some(carry) = &mut carry {
            carry.push_str(delta);
            carry
        } else {
            delta
        };
        let (out, tail) = self.mask.restore_chunk(text, json);
        if !tail.is_empty() {
            self.held.insert(index, tail.to_owned());
        }
        out.into_owned()
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

    /// Load the mask if none is armed, keeping a mask installed concurrently.
    pub fn arm_if_empty(&self, load: impl FnOnce() -> Option<Mask>) -> bool {
        if self.is_armed() {
            return true;
        }
        let Some(mask) = load() else {
            return self.is_armed();
        };
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| Arc::new(mask));
        true
    }

    /// The set a request is covered with: what was found in it added to the
    /// armed one. `None` when nothing is armed, so the request goes as it is.
    ///
    /// # Errors
    ///
    /// Returns an error if a newly found value has no free token of its shape.
    pub fn absorb(&self, mut found: Vec<crate::secret::Found>) -> Result<Option<Arc<Mask>>> {
        if found.is_empty() {
            return Ok(self.current());
        }
        let mut slot = self.0.write().unwrap_or_else(PoisonError::into_inner);
        let Some(current) = slot.as_ref() else {
            return Ok(None);
        };
        found.retain(|(_, value)| !current.contains(value) && !current.is_token(value));
        if found.is_empty() {
            return Ok(slot.clone());
        }
        let pending = found.iter().map(|(_, value)| value.as_str()).collect();
        let mut next = current.as_ref().clone();
        for (name, value) in &found {
            next.insert(name.as_deref(), value, &pending)?;
        }
        let next = Arc::new(next);
        *slot = Some(Arc::clone(&next));
        Ok(Some(next))
    }

    pub fn current(&self) -> Option<Arc<Mask>> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn is_armed(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
}

// Every text a request carries, read for what only a credential looks like.
fn secrets_in_request(req: &Request) -> Vec<crate::secret::Found> {
    let mut found = Vec::new();
    let mut take = |value: &str| found.extend(crate::secret::secrets_in(value));
    visit_request_text!(req, take, |value| for_json_text(value, &mut take));
    found
}

fn for_json_text(value: &Value, take: &mut impl FnMut(&str)) {
    match value {
        Value::String(text) => take(text),
        Value::Array(items) => {
            for item in items {
                for_json_text(item, take);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                take(key);
                for_json_text(item, take);
            }
        }
        _ => {}
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
        let mask = if self.armed.is_armed() {
            self.armed.absorb(secrets_in_request(req))?
        } else {
            None
        };
        let Some(mask) = mask.filter(|mask| !mask.is_empty()) else {
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
            restorer: Restorer::new(mask),
            events,
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
            StreamEvent::Complete {
                mut content,
                invalid,
            } => {
                self.release_all();
                for part in &mut content {
                    self.restore_content(part);
                }
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

    fn restore_content(&self, content: &mut AssistantContent) {
        let mask = &self.restorer.mask;
        match content {
            AssistantContent::Text(text) => {
                rewrite_field(&mut text.text, |text| mask.restored(text));
            }
            AssistantContent::ToolCall(call) => {
                map_json(&mut call.args, &|text| mask.restored(text));
            }
            AssistantContent::Reasoning(reasoning) => {
                for part in &mut reasoning.content {
                    if let ReasoningContent::Text { text, .. } = part {
                        rewrite_field(text, |text| mask.restored(text));
                    }
                }
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

    fn mask(key: [u8; 32], values: &[&str]) -> Arc<Mask> {
        let mut mask = Mask::new(key);
        for value in values {
            mask.add(None, value).expect("a token");
        }
        Arc::new(mask)
    }

    // The whole contract: what leaves is not the value, and what comes back is.
    #[test]
    fn a_value_comes_back_from_its_token() {
        let value = "Bearer sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[value]);
        let token = s.cover(value);
        assert_eq!(token.len(), value.len(), "same length, same public prefix");
        assert!(token.starts_with("Bearer sk-"));
        assert_ne!(token, value);
        assert_eq!(
            s.restore(&format!("header: {token}")),
            format!("header: {value}")
        );
    }

    // A prefix of an earlier value must not eat the longer one's head.
    #[test]
    fn the_longer_of_two_overlapping_values_wins() {
        let s = mask(KEY, &["sk-abc", "sk-abcdef"]);
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
        let here = mask(KEY, &[value]);
        let again = mask(KEY, &[value]);
        let elsewhere = mask([9; 32], &[value]);
        assert_eq!(here.cover(value), again.cover(value));
        assert_ne!(here.cover(value), elsewhere.cover(value));
    }

    // SSE cuts deltas anywhere, so a token arrives in pieces; the tail cannot
    // be forwarded before it is known not to be one.
    #[test]
    fn a_token_split_across_two_deltas_is_restored_by_the_second() {
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
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
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
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
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
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
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
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
        let s = mask(KEY, &[value]);
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
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
        let once = s.restore(&s.cover("a sk-live-9f3a1c2b7d5e b"));
        assert_eq!(s.restore(&once), once);
    }

    #[test]
    fn a_value_listed_twice_keeps_the_first_name_and_one_token() {
        let mut s = Mask::new(KEY);
        let value = "hunter2@corp.example";
        s.add(Some("FIRST_PASSWORD"), value).expect("first token");
        let token = s.cover(value);
        s.add(Some("SECOND_PASSWORD"), value)
            .expect("existing token");
        assert_eq!(s.len(), 1);
        assert_eq!(s.footprints()[0].name.as_deref(), Some("FIRST_PASSWORD"));
        assert_eq!(s.cover(value), token);
        assert_eq!(s.restore(&token), value);
    }

    // Text with nothing to cover is the common case by far: it must come
    // through byte for byte, including a `{{` that opens no token.
    #[test]
    fn text_without_a_token_comes_through_unchanged() {
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
        let text = "a {{PI_ b {{PI_NOT_A_TOKEN}} c";
        assert_eq!(s.cover(text), text);
        assert_eq!(s.restore(text), text);
    }

    // The listing is where a person can see what was masked, how much of it
    // is shown, and which name introduced it: a head of four characters is
    // enough to recognise a value and not enough to use it.
    #[test]
    fn a_listing_names_each_value_and_shows_both_ends() {
        let mut s = Mask::new(KEY);
        s.add(Some("TYPESAFE_API_KEY"), "sk-listing-0123456789")
            .expect("API key token");
        s.add(None, "999888777:AAbbCCddEEffGG").expect("bot token");
        s.add(None, "hunter2").expect("password token");
        let text = "sk-listing-0123456789 twice: sk-listing-0123456789";
        let _ = s.cover(text);
        let prints = s.footprints();
        assert_eq!(prints.len(), 3);
        assert_eq!(prints[0].name.as_deref(), Some("TYPESAFE_API_KEY"));
        assert_eq!(prints[0].head, "sk-l");
        assert_eq!(prints[0].tail, "6789");
        assert_eq!(prints[0].len, 21);
        assert_eq!(prints[0].hits, 2);
        assert_eq!(prints[1].name, None);
        assert_eq!(prints[1].hits, 0);
        // Four of a seven-character value would be more than half of it.
        assert_eq!(prints[2].head, "h");
        assert_eq!(prints[2].tail, "2");
        assert_eq!(prints[2].len, 7);
        assert!(prints.iter().all(|p| p.head.chars().count() <= 4));
    }

    // A token keeps the length and the alphabet of the value it hides: a
    // digest stays a digest, so comparing and joining still work.
    #[test]
    fn a_token_keeps_the_length_and_alphabet_of_its_value() {
        let hex = "a3f1c2b4d5e6f708192a3b4c5d6e7f80";
        let s = mask(KEY, &[hex]);
        let token = s.cover(hex);
        assert_eq!(token.len(), hex.len());
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(s.restore(&token), hex);

        let upper = "A3F1C2B4D5E6F708192A3B4C5D6E7F80";
        let s = mask(KEY, &[upper]);
        let token = s.cover(upper);
        assert!(
            token
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()),
            "{token}"
        );

        let long = "0123456789abcdef".repeat(8);
        let s = mask(KEY, &[long.as_str()]);
        let token = s.cover(&long);
        assert_eq!(token.len(), long.len());
        assert_eq!(s.restore(&token), long);
    }

    // A request carries text in a dozen places and opaque bytes in three. The
    // shape has to survive either way: a masked request is still valid JSON
    // with the same keys and the same numbers.
    #[test]
    fn every_text_a_request_carries_is_covered() {
        let value = "sk-live-9f3a1c2b7d5e";
        let s = mask(KEY, &[value]);
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
        let s = mask(KEY, &[value]);
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
        let s = mask(KEY, &[value]);
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
        let s = mask(KEY, &[value]);
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
        let s = mask(KEY, &[value]);
        let (_, seen) = drain_with(request_carrying(value, false), Vec::new(), &s);
        let wire = wire_json(&seen.expect("a call"));
        assert!(!wire.contains(value), "{wire}");
    }

    // A wire that ends on `Done` without closing its last block would swallow
    // the model's parting bytes: the held tail is read out before the end.
    #[test]
    fn a_tail_the_stream_never_completes_is_still_read() {
        let s = mask(KEY, &["sk-live-9f3a1c2b7d5e"]);
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
        let value = "transport-secret-9f3a1c2b7d5e";
        let s = mask(KEY, &[value]);
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
                        args: json!({ "command": format!("echo {token}"), (token.as_str()): 1 }),
                    }),
                    AssistantContent::Reasoning(Reasoning {
                        id: None,
                        by: None,
                        content: vec![ReasoningContent::Text {
                            text: format!("thinking of {token}"),
                            signature: Some("original-signature".into()),
                        }],
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
        assert_eq!(call.args[value], 1);
        let AssistantContent::Reasoning(reasoning) = &content[2] else {
            panic!("{content:?}");
        };
        assert_eq!(reasoning.text(), format!("thinking of {value}"));

        let message = Message::Assistant {
            content: content.clone(),
        };
        let saved = serde_json::to_string(&message).expect("serializable assistant message");
        assert!(saved.contains(value), "{saved}");
        assert!(!saved.contains(&token), "{saved}");
        let replay = Request {
            messages: vec![message],
            ..Request::default()
        };
        let (_, seen) = drain_with(replay, Vec::new(), &s);
        let wire = wire_json(&seen.expect("replayed message"));
        assert_eq!(wire.matches(&token).count(), 4, "{wire}");
        assert!(!wire.contains(value), "{wire}");
        assert!(wire.contains("original-signature"), "{wire}");
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

        armed.arm(Some(mask(KEY, &[value])));
        assert!(!call(&seen).contains(value));
    }

    // What is about to leave is read before it leaves: a credential the set
    // has not seen reaches the wire as a token, and the set keeps it.
    #[test]
    fn a_credential_in_the_request_is_found_and_covered() {
        let seen = Arc::new(Mutex::new(None));
        let wire = Canned {
            events: Vec::new(),
            seen: Arc::clone(&seen),
        };
        let armed = Arc::new(Armed::default());
        armed.arm(Some(Arc::new(Mask::new(KEY))));
        let masked = Masked::new(Arc::new(wire), Arc::clone(&armed));
        let spec = ModelSpec::test();
        let value = "sk-found-0123456789abcdef";
        let mut request = request_carrying(value, false);
        // The first candidate for sk-... is itself a credential in this batch.
        let other = mask(KEY, &["sk-..."]).cover("sk-...");
        request.system = Some(format!(
            "OPENAI_API_KEY=sk-...\nOTHER_KEY={other}\nPASSWD=changeme"
        ));
        let _ = block_on(masked.stream(&spec, &request)).expect("a stream");
        let wire = wire_json(&seen.lock().expect("unpoisoned").clone().expect("a call"));
        let mask = armed.current().expect("armed");
        for value in [value, "sk-...", &other, "changeme"] {
            assert!(mask.contains(value));
            assert!(!wire.contains(value), "{wire}");
            let token = mask.cover(value);
            assert_eq!(token.len(), value.len());
            assert!(wire.contains(&token), "{wire}");
            assert_eq!(mask.restore(&token), value);
        }
        assert!(mask.cover("sk-...").starts_with("sk-"));
    }

    #[test]
    fn an_unmaskable_credential_blocks_the_request() {
        let seen = Arc::new(Mutex::new(None));
        let wire = Canned {
            events: Vec::new(),
            seen: Arc::clone(&seen),
        };
        let armed = Arc::new(Armed::default());
        let mut original = Mask::new(KEY);
        for value in ["sk--", "sk-.", "sk-_", "sk-~"] {
            if !original.is_token(value) {
                original.add(None, value).expect("a free punctuation token");
            }
        }
        let original = Arc::new(original);
        armed.arm(Some(Arc::clone(&original)));
        let masked = Masked::new(Arc::new(wire), Arc::clone(&armed));
        let req = Request {
            system: Some(
                "redis://user:sk-found-0123456789abcdef@host/db redis://user:sk-:@host/db".into(),
            ),
            ..Request::default()
        };
        let result = block_on(masked.stream(&ModelSpec::test(), &req));
        assert!(matches!(result, Err(LlmError::MaskShape)));
        assert!(seen.lock().expect("unpoisoned").is_none());
        assert!(Arc::ptr_eq(&armed.current().expect("armed"), &original));
    }

    // A token seen again in the text is still a token: finding it would mask
    // the mask and hand the wire a token of a token.
    #[test]
    fn a_token_seen_again_is_not_a_secret() {
        let mut set = Mask::new(KEY);
        set.add(None, "sk-live-9f3a1c2b7d5e").expect("a token");
        let token = set.cover("sk-live-9f3a1c2b7d5e");
        let armed = Armed::default();
        armed.arm(Some(Arc::new(set)));
        let after = armed
            .absorb(vec![(None, token)])
            .expect("valid")
            .expect("armed");
        assert_eq!(after.len(), 1);
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
