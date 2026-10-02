use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::error::Result;
use crate::model::ModelSpec;
use crate::request::Request;
use crate::stream::StreamEvent;

pub mod anthropic;
pub mod chat;
pub mod openai;

// The one thing the OpenAI-shaped wires cannot say: a tool result there has no
// `is_error`, so a failure not marked in its text reads to the model as a result.
const FAILED: &str = "[tool error]";

/// One exchange, from the request going out to the response coming back.
///
/// Owns the whole flow rather than logging at scattered call sites: a bare
/// `send().await?` can fail silently (DNS, TLS) without the journal seeing it.
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
                // As the trait object, so the journal unwinds the source chain —
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
    const MAX_BODY: usize = 4096;
    let body = if body.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while cut > 0 && body.as_bytes()[cut - 1].is_ascii_digit() {
            cut -= 1;
        }
        format!("{} (truncated)", crate::slice::head_bytes(&body, cut))
    } else {
        body
    };
    tracing::warn!(target: "pi::wire", format, status, took_ms, detail = %body, "refused");
    Err(crate::LlmError::Api {
        format,
        status,
        body,
    })
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
    // Gaps waiting to reach the reader. The journal has them either way; this
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
    /// journal has to say it happened.
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
}
