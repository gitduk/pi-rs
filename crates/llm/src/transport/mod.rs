use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::stream::{BoxStream, StreamExt};
use serde_json::Value;

use crate::error::{LlmError, Result};
use crate::model::ModelSpec;
use crate::request::{Request, ToolChoice};
use crate::stream::StreamEvent;

pub mod anthropic;
pub mod chat;
pub mod openai;

/// The connection's headers, and the conversation id when the host asks for
/// one. Pure, so a test can read what a call would carry.
pub(crate) fn headers_for(spec: &ModelSpec, req: &Request) -> Vec<(String, String)> {
    let mut headers = spec.headers.clone();
    if let (Some(name), Some(id)) = (&spec.session_header, &req.session_id) {
        headers.push((name.clone(), id.clone()));
    }
    headers
}

/// A request with the connection's extra headers on it, added before the wire's
/// own: a name the wire also sets is sent twice rather than replaced.
pub(crate) fn extra_headers(
    mut call: reqwest::RequestBuilder,
    spec: &ModelSpec,
    req: &Request,
) -> reqwest::RequestBuilder {
    for (name, value) in headers_for(spec, req) {
        call = call.header(name, value);
    }
    call
}

// The one thing the OpenAI-shaped wires cannot say: a tool result there has no
// `is_error`, so a failure not marked in its text reads to the model as a result.
const FAILED: &str = "[tool error]";

/// One exchange, from the request going out to the response coming back.
///
/// Owns the whole flow rather than logging at scattered call sites: a bare
/// `send().await?` can fail silently (DNS, TLS) without the log seeing it.
pub(crate) async fn exchange(
    format: &'static str,
    url: String,
    spec: &ModelSpec,
    req: &Request,
    body: &serde_json::Value,
    call: reqwest::RequestBuilder,
) -> crate::Result<reqwest::Response> {
    tracing::debug!(
        target: "pi::wire",
        format,
        // Some hosts take their credential as a query parameter. The path is
        // what identifies the endpoint; the rest is not worth the risk.
        url = url.split('?').next().unwrap_or(&url),
        model = %spec.model,
        messages = req.messages.len(),
        tools = req.tools.len(),
        effort = ?req.effort,
        "request"
    );
    // The body runs to hundreds of kilobytes and is the one thing a 400 is
    // actually about, so it rides one level below everything else.
    tracing::trace!(target: "pi::wire", format, body = %body, "request body");

    let began = std::time::Instant::now();
    let resp = match call.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(
                target: "pi::wire",
                format,
                took_ms = began.elapsed().as_millis() as u64,
                // As the trait object, so the log unwinds the source chain —
                // reqwest's own line just says "error sending request".
                error = &e as &dyn std::error::Error,
                "unreachable"
            );
            return Err(e.into());
        }
    };

    let status = resp.status().as_u16();
    let took_ms = began.elapsed().as_millis() as u64;
    if resp.status().is_success() {
        tracing::info!(target: "pi::wire", format, status, took_ms, "response");
        return Ok(resp);
    }

    // Full refusal text — only the provider's wording says what went wrong.
    // Capped; the cut backs off to a digit boundary so `overflow_limit` stays intact.
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("<body unreadable: {e}>"));
    let body = capped(body);
    tracing::warn!(target: "pi::wire", format, status, took_ms, detail = %body, "refused");
    Err(crate::LlmError::Api {
        format,
        status,
        body,
    })
}

/// One wire's call, once its endpoint and credential are on it: the whole of
/// what the three wires differ in. Headers, body and the exchange are one flow.
pub(crate) struct Call<'a> {
    format: &'static str,
    url: String,
    spec: &'a ModelSpec,
    req: &'a Request,
    call: reqwest::RequestBuilder,
}

impl<'a> Call<'a> {
    /// A call to `url`, with the connection's own headers already on it.
    pub(crate) fn to(
        http: &reqwest::Client,
        format: &'static str,
        url: String,
        spec: &'a ModelSpec,
        req: &'a Request,
    ) -> Self {
        Self {
            format,
            call: extra_headers(http.post(url.as_str()), spec, req),
            url,
            spec,
            req,
        }
    }

    /// The credential most hosts take, as `Authorization: Bearer`.
    pub(crate) fn bearer(mut self, key: &Option<String>) -> Self {
        if let Some(key) = key {
            self.call = self.call.bearer_auth(key);
        }
        self
    }

    /// A header this wire sends on every call.
    pub(crate) fn header(mut self, name: &'static str, value: &str) -> Self {
        self.call = self.call.header(name, value);
        self
    }

    /// Body on, out to the wire, and the exchange logged as one flow.
    pub(crate) async fn send(self, body: &Value) -> Result<reqwest::Response> {
        let call = self.call.json(body);
        exchange(self.format, self.url, self.spec, self.req, body, call).await
    }
}

/// The output cap every wire sends: what the caller asked for, never above
/// what the model itself serves.
pub(crate) fn max_output_tokens(spec: &ModelSpec, req: &Request) -> u32 {
    req.max_output_tokens
        .unwrap_or(spec.max_output_tokens)
        .min(spec.max_output_tokens)
}

/// What the caller asked the model to do with tools, settled the same way by
/// every wire: a model that cannot be forced gets `Auto`. Each wire then spells
/// this in its own vocabulary, and nothing else decides it.
pub(crate) enum Forced<'a> {
    Auto,
    None,
    Required,
    Named(&'a str),
}

pub(crate) fn forced<'a>(spec: &ModelSpec, req: &'a Request) -> Forced<'a> {
    match &req.tool_choice {
        ToolChoice::Auto => Forced::Auto,
        ToolChoice::None => Forced::None,
        _ if !spec.can_force_tool => Forced::Auto,
        ToolChoice::Required => Forced::Required,
        ToolChoice::Named(name) => Forced::Named(name),
    }
}

// A refusal's body, capped. The cut backs off past a run of digits so a number
// the body states — the limit an overflow names — is never read cut short.
fn capped(body: String) -> String {
    const MAX_BODY: usize = 4096;
    if body.len() <= MAX_BODY {
        return body;
    }
    let mut cut = MAX_BODY;
    while cut > 0 && body.as_bytes()[cut - 1].is_ascii_digit() {
        cut -= 1;
    }
    format!("{} (truncated)", crate::slice::head_bytes(&body, cut))
}

/// Every wire's SSE frames as events: each frame's JSON goes to `decode`; an
/// unreadable frame or one carrying `error` ends the turn.
pub(crate) fn sse(
    resp: reqwest::Response,
    format: &'static str,
    mut decode: impl FnMut(Value) -> Vec<Result<StreamEvent>> + Send + 'static,
) -> BoxStream<'static, Result<StreamEvent>> {
    resp.bytes_stream()
        .eventsource()
        .flat_map(move |frame| {
            let events = match frame {
                Err(e) => vec![Err(LlmError::Stream(e.to_string()))],
                Ok(f) => frame_events(&f.data, format, &mut decode),
            };
            futures::stream::iter(events)
        })
        .boxed()
}

/// What one frame means, for every wire: nothing, events, or the ending.
///
/// A frame that will not parse ends the turn rather than passing for a turn
/// that simply had less in it; the model's own words would be missing and
/// nothing would say so.
fn frame_events(
    data: &str,
    format: &'static str,
    decode: &mut impl FnMut(Value) -> Vec<Result<StreamEvent>>,
) -> Vec<Result<StreamEvent>> {
    let data = data.trim();
    // Chat Completions closes on a data-only `[DONE]`, no JSON in it; the chunks
    // before it already delivered the turn. A keep-alive carries nothing either.
    if data == "[DONE]" || data.is_empty() {
        return Vec::new();
    }
    let data: Value = match serde_json::from_str(data) {
        Ok(data) => data,
        Err(e) => return vec![Err(LlmError::Stream(e.to_string()))],
    };
    if data.get("error").is_some_and(|e| !e.is_null()) {
        tracing::warn!(
            target: "pi::wire", format,
            detail = %data["error"], "error frame"
        );
        return vec![Err(LlmError::Stream(data["error"].to_string()))];
    }
    decode(data)
}

/// For a wire whose terminal frame is guaranteed: a stream that closes before
/// it was cut short, and a cut must not pass for a finished turn. One that
/// finished without a count is told to `gaps`, or it reads as a free turn.
pub(crate) fn terminated(
    format: &'static str,
    gaps: Shared,
    events: BoxStream<'static, Result<StreamEvent>>,
) -> BoxStream<'static, Result<StreamEvent>> {
    events
        .map(Some)
        .chain(futures::stream::once(async { None }))
        .scan((false, false), move |(done, counted), item| {
            let out = match item {
                Some(Ok(ev)) => {
                    if matches!(ev, StreamEvent::Done { .. }) {
                        *done = true;
                    }
                    if let StreamEvent::MessageStart { usage }
                    | StreamEvent::Usage { usage }
                    | StreamEvent::Done { usage, .. } = &ev
                    {
                        *counted |= *usage != crate::stream::Usage::default();
                    }
                    Some(Ok(ev))
                }
                // The error is the ending: no cut is reported after it.
                Some(Err(e)) => {
                    *done = true;
                    *counted = true;
                    Some(Err(e))
                }
                None if *done => {
                    if !*counted {
                        gaps.frame().uncounted();
                    }
                    None
                }
                None => {
                    tracing::warn!(target: "pi::wire", format, "stream closed before its terminal frame");
                    Some(Err(LlmError::Stream(
                        "the stream closed before its terminal frame".into(),
                    )))
                }
            };
            futures::future::ready(out)
        })
        .boxed()
}

/// One wire protocol. Implementations branch on `spec.format` and never on the
/// model id: identity is resolved once, into the spec.
#[async_trait]
pub trait Transport: Send + Sync {
    // What this host owed and did not send since it was last asked. Drained
    // per turn, so a host quietly losing content gets shown, not just logged.
    fn gaps(&self) -> Vec<String> {
        Vec::new()
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>>;
}

/// A `Gaps` shared between the transport that owns it and the stream it hands
/// out. A poisoned lock still hands the reporter over.
#[derive(Clone)]
pub(crate) struct Shared(Arc<Mutex<Gaps>>);

impl Shared {
    pub(crate) fn new(format: &'static str) -> Self {
        Self(Arc::new(Mutex::new(Gaps::new(format))))
    }

    /// The reporter, for the length of one frame.
    ///
    /// Locked once per frame, not per field: a delta is the per-token path.
    /// Failing to report is never a reason to fail the turn.
    pub(crate) fn frame(&self) -> impl std::ops::DerefMut<Target = Gaps> + '_ {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What has not reached the reader yet, and nothing twice.
    pub(crate) fn drain(&self) -> Vec<String> {
        self.frame().drain()
    }
}

/// What a host owed this turn and did not deliver — not a schema check, just
/// what the decoder reached for and didn't find.
pub(crate) struct Gaps {
    format: &'static str,
    // One line per (event, thing), for the *session*'s life, not the stream —
    // said every turn, a host defect teaches the reader to skip it.
    said: BTreeSet<(String, String)>,
    // Gaps waiting to reach the reader. The log has them either way; this
    // is the half seen without being grepped for.
    pending: Vec<String>,
}

impl Gaps {
    pub(crate) fn new(format: &'static str) -> Self {
        Self {
            format,
            said: BTreeSet::new(),
            pending: Vec::new(),
        }
    }

    /// Take what has not been shown yet.
    pub(crate) fn drain(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending)
    }

    fn first_time(&mut self, event: &str, thing: &str) -> bool {
        self.said.insert((event.to_string(), thing.to_string()))
    }

    /// Something the model produced is not in the turn. Warned rather than
    /// noted: the run continues and looks ordinary, which is exactly why the
    /// log has to say it happened.
    pub(crate) fn lost(&mut self, event: &str, thing: &str) {
        if self.first_time(event, thing) {
            tracing::warn!(
                target: "pi::wire", format = self.format, event, thing,
                "the host did not send this and the turn is smaller for it"
            );
            self.pending.push(format!(
                "the endpoint did not send `{thing}` on `{event}`; \
                 what the model produced there is missing from the turn"
            ));
        }
    }

    /// The turn finished with no token count at all: what it cost reads as
    /// nothing, which looks ordinary unless said.
    pub(crate) fn uncounted(&mut self) {
        if self.first_time("stream", "usage") {
            tracing::warn!(
                target: "pi::wire", format = self.format,
                "the turn finished without a token count"
            );
            self.pending.push(
                "the endpoint sent no token usage; this session's counts and cost read low"
                    .to_string(),
            );
        }
    }

    /// A shape this build does not know, which cost the turn nothing. Vendors
    /// add bookkeeping events routinely, so this is said quietly — crying wolf
    /// here is what would teach a reader to ignore `lost`.
    pub(crate) fn ignored(&mut self, event: &str, thing: &str) {
        if self.first_time(event, thing) {
            tracing::debug!(
                target: "pi::wire", format = self.format, event, thing,
                "unrecognised, and nothing was dropped from the turn"
            );
        }
    }

    /// How many distinct gaps have been reported. Test-only: the reports
    /// themselves go to `tracing`, and pulling in a subscriber to read them
    /// back would test the log line rather than the deduplication.
    #[cfg(test)]
    pub(crate) fn reported(&self) -> usize {
        self.said.len()
    }

    /// Read a string field the frame owes. `None` says the host left it out,
    /// rather than silently dropping the frame the way a bare `?` would.
    pub(crate) fn owed<'a>(
        &mut self,
        frame: &'a Value,
        event: &str,
        field: &str,
    ) -> Option<&'a str> {
        match frame[field].as_str() {
            Some(found) => Some(found),
            None => {
                self.lost(event, field);
                None
            }
        }
    }

    /// Read a numeric index the frame owes, standing in 0 where the host left it out.
    pub(crate) fn owed_index(&mut self, frame: &Value, event: &str, field: &str) -> usize {
        match frame[field].as_u64() {
            Some(i) => i as usize,
            None => {
                self.lost(event, field);
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_frame_that_will_not_parse_ends_the_turn() {
        let none = |_: Value| Vec::new();
        let mut decode = none;

        let cut = frame_events("{\"type\":\"content_bl", "anthropic", &mut decode);
        assert!(matches!(cut.as_slice(), [Err(LlmError::Stream(_))]));
    }

    #[test]
    fn a_keepalive_and_a_done_frame_carry_no_event() {
        let mut decode = |_: Value| {
            vec![Ok(StreamEvent::Done {
                stop: Default::default(),
                usage: Default::default(),
            })]
        };
        for quiet in ["[DONE]", "", "  \n"] {
            assert!(
                frame_events(quiet, "chat", &mut decode).is_empty(),
                "{quiet:?} read as an event"
            );
        }
    }

    #[test]
    fn an_error_frame_is_the_ending_and_never_reaches_the_decoder() {
        let mut decode = |_: Value| panic!("an error frame was decoded as content");
        let out = frame_events(
            r#"{"error":{"message":"invalid_api_key"}}"#,
            "anthropic",
            &mut decode,
        );
        assert!(
            matches!(out.as_slice(), [Err(LlmError::Stream(text))] if text.contains("invalid_api_key"))
        );
    }

    #[test]
    fn a_model_that_cannot_be_forced_is_asked_for_auto() {
        let forced_of = |can_force_tool, choice| {
            let spec = ModelSpec {
                can_force_tool,
                ..ModelSpec::test()
            };
            let req = Request {
                tool_choice: choice,
                ..Request::default()
            };
            matches!(forced(&spec, &req), Forced::Auto)
        };
        assert!(forced_of(false, ToolChoice::Required), "refused but forced");
        assert!(forced_of(false, ToolChoice::Named("read".into())));
        assert!(!forced_of(true, ToolChoice::Required), "capable and forced");
    }

    #[test]
    fn a_conversation_is_named_only_when_the_connection_names_a_header() {
        let named = |header: Option<&str>, id: Option<&str>| {
            let spec = ModelSpec {
                session_header: header.map(str::to_string),
                ..ModelSpec::test()
            };
            let req = Request {
                session_id: id.map(str::to_string),
                ..Request::default()
            };
            headers_for(&spec, &req)
        };
        assert_eq!(
            named(Some("x-opencode-session"), Some("s1")),
            [("x-opencode-session".into(), "s1".into())]
        );
        assert!(named(None, Some("s1")).is_empty(), "no header was named");
        assert!(named(Some("x-opencode-session"), None).is_empty(), "no id");
    }

    #[test]
    fn a_long_refusal_is_capped_short_of_a_number_it_would_split() {
        let short = "x".repeat(4096);
        assert_eq!(capped(short.clone()), short);

        let mut body = "x".repeat(4090);
        body.push_str(" n=131072 tokens");
        assert_eq!(capped(body), format!("{} n= (truncated)", "x".repeat(4090)));
    }

    #[test]
    fn a_stream_cut_before_its_terminal_frame_is_an_error_not_a_turn() {
        let run = |events: Vec<StreamEvent>| {
            let events = futures::stream::iter(events.into_iter().map(Ok)).boxed();
            let gaps = Shared::new("anthropic");
            futures::executor::block_on(terminated("anthropic", gaps, events).collect::<Vec<_>>())
        };
        let text = || StreamEvent::TextDelta {
            index: 0,
            delta: "拿到".into(),
        };

        let cut = run(vec![text()]);
        assert_eq!(cut.len(), 2);
        assert!(matches!(cut.last(), Some(Err(LlmError::Stream(_)))));

        let whole = run(vec![
            text(),
            StreamEvent::Done {
                stop: Default::default(),
                usage: Default::default(),
            },
        ]);
        assert_eq!(whole.len(), 2);
        assert!(whole.iter().all(Result::is_ok));
    }

    #[test]
    fn usage_reported_before_the_terminal_frame_counts_as_reported() {
        let gaps = Shared::new("chat");
        let events = futures::stream::iter([
            Ok(StreamEvent::Usage {
                usage: crate::stream::Usage {
                    input: 100,
                    output: 20,
                    ..Default::default()
                },
            }),
            Ok(StreamEvent::Done {
                stop: Default::default(),
                usage: Default::default(),
            }),
        ])
        .boxed();
        futures::executor::block_on(terminated("chat", gaps.clone(), events).collect::<Vec<_>>());
        assert!(gaps.drain().is_empty());
    }

    // A host that never counts would show a session as free; said once, not
    // per turn, and never for one that did count.
    #[test]
    fn a_turn_finished_without_a_count_is_said_once() {
        let run = |gaps: &Shared, output: u64| {
            let done = StreamEvent::Done {
                stop: Default::default(),
                usage: crate::stream::Usage {
                    output,
                    ..Default::default()
                },
            };
            let events = futures::stream::iter([Ok(done)]).boxed();
            futures::executor::block_on(terminated("chat", gaps.clone(), events).count());
        };
        let counted = Shared::new("chat");
        run(&counted, 7);
        assert!(counted.drain().is_empty());

        let silent = Shared::new("chat");
        run(&silent, 0);
        run(&silent, 0);
        assert_eq!(silent.drain().len(), 1);
        assert!(silent.drain().is_empty());
    }

    #[test]
    fn a_field_the_host_left_out_is_reported_rather_than_blanked() {
        let mut gaps = Gaps::new("anthropic");
        let delta = json!({ "type": "text_delta" });
        assert_eq!(gaps.owed(&delta, "content_block_delta", "text"), None);
        assert_eq!(gaps.reported(), 1);
        // The field that is there reads back without reporting anything.
        assert_eq!(
            gaps.owed(&delta, "content_block_delta", "type"),
            Some("text_delta")
        );
        assert_eq!(gaps.reported(), 1);
    }

    // A malformed delta arrives once per token. Said every time, the one line
    // that matters is buried under two thousand copies of itself.
    #[test]
    fn the_same_gap_is_said_once_however_often_it_arrives() {
        let mut gaps = Gaps::new("openai");
        for _ in 0..2_000 {
            gaps.lost("response.output_text.delta", "delta");
        }
        assert_eq!(gaps.reported(), 1);

        // Distinct gaps are distinct news, including the same field on a
        // different event.
        gaps.lost("response.completed", "delta");
        gaps.ignored("frame", "response.queued");
        assert_eq!(gaps.reported(), 3);
    }

    // A wire told nothing lets the model think by its own default, which ate
    // a whole reply budget on DeepSeek once; off has to be said everywhere.
    #[test]
    fn every_wire_says_off_out_loud() {
        use crate::model::Format;
        use crate::request::Effort;
        let req = |effort| Request {
            messages: vec![crate::message::Message::user("go")],
            effort,
            ..Default::default()
        };
        let on = |format| ModelSpec {
            format,
            ..ModelSpec::test()
        };
        let anthropic = ModelSpec::test();
        let chat = on(Format::Chat);
        let openai = on(Format::OpenAi);

        let off = req(Effort::Off);
        assert_eq!(
            anthropic::build_body(&anthropic, &off)["thinking"],
            json!({ "type": "disabled" })
        );
        assert_eq!(
            chat::build_body(&chat, &off)["thinking"],
            json!({ "type": "disabled" })
        );
        assert_eq!(
            openai::build_body(&openai, &off)["reasoning"],
            json!({ "effort": "none" })
        );

        let high = req(Effort::High);
        let body = anthropic::build_body(&anthropic, &high);
        assert_eq!(body["thinking"], json!({ "type": "adaptive" }));
        assert_eq!(body["output_config"], json!({ "effort": "high" }));
        assert_eq!(
            chat::build_body(&chat, &high)["thinking"],
            json!({ "type": "enabled", "reasoning_effort": "high" })
        );
        assert_eq!(
            openai::build_body(&openai, &high)["reasoning"],
            json!({ "effort": "high" })
        );

        // A model that cannot think is told nothing, off included.
        let mute = |spec: ModelSpec| ModelSpec {
            accepts_effort: false,
            ..spec
        };
        let body = anthropic::build_body(&mute(anthropic), &high);
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none());
        assert!(
            chat::build_body(&mute(chat), &off)
                .get("thinking")
                .is_none()
        );
        assert!(
            openai::build_body(&mute(openai), &high)
                .get("reasoning")
                .is_none()
        );
    }

    #[test]
    fn every_wire_sends_the_endpoint_id_instead_of_the_local_selection_name() {
        let req = Request::default();
        let spec = |format| crate::model::ModelSpec {
            model: "provider/auto/shared".into(),
            wire_id: Some("auto/shared".into()),
            format,
            ..crate::model::ModelSpec::test()
        };
        let anthropic = spec(crate::model::Format::Anthropic {
            cache_control: Default::default(),
        });
        assert_eq!(
            anthropic::build_body(&anthropic, &req)["model"],
            "auto/shared"
        );
        assert_eq!(
            openai::build_body(&spec(crate::model::Format::OpenAi), &req)["model"],
            "auto/shared"
        );
        assert_eq!(
            chat::build_body(&spec(crate::model::Format::Chat), &req)["model"],
            "auto/shared"
        );
    }
}
