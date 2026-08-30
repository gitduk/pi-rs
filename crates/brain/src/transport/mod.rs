use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value;

use crate::model::ModelSpec;
use crate::error::Result;
use crate::request::Request;
use crate::stream::StreamEvent;

pub mod anthropic;
pub mod openai;

/// One exchange, from the request going out to the response coming back.
///
/// Both wires go through this rather than calling a set of logging helpers at
/// the right moments themselves. Sharing the words but not the control flow is
/// how the two came to share a hole: `send().await?` returns on a refused
/// connection, a DNS failure or a TLS failure without either wire noticing, so
/// the journal showed a request and then nothing — the one class of failure a
/// journal is most needed for. Owning the flow makes that unrecordable.
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
                // As the trait object, so the journal unwinds the source chain.
                // A reqwest error's own line says only "error sending request";
                // `Connection refused` is two links down, and it is the answer.
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

    // The refusal text in full where the level allows: providers put the real
    // reason in it, and a status alone never says which of a dozen things
    // went wrong.
    let body = resp.text().await.unwrap_or_default();
    tracing::warn!(target: "pi::wire", format, status, took_ms, detail = %body, "refused");
    Err(crate::BrainError::Api {
        format,
        status,
        body,
    })
}

/// One wire protocol. Implementations branch on `spec.format` and never on the
/// model id: identity is resolved once, into the spec.
#[async_trait]
pub trait Transport: Send + Sync {
    /// What this host owed and did not send, since the last time it was asked.
    /// Drained per turn and shown once per session, so a host quietly losing
    /// content is something the reader is told rather than something they have
    /// to go looking for.
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
/// out. Cloneable, so the stream takes one rather than borrowing the transport.
///
/// The lock and its poison tolerance live here. They were four call sites that
/// each had to get them right, and getting them wrong is silent: a reporter
/// that cannot be reached reports nothing, which reads exactly like a host with
/// nothing wrong.
#[derive(Clone)]
pub(crate) struct Shared(Arc<Mutex<Gaps>>);

impl Shared {
    pub(crate) fn new(format: &'static str) -> Self {
        Self(Arc::new(Mutex::new(Gaps::new(format))))
    }

    /// The reporter, for the length of one frame.
    ///
    /// Once per frame rather than once per field read: a malformed delta asks
    /// twice, and a delta is the per-token path. A poisoned lock hands the
    /// reporter over anyway — failing to report is never a reason to fail the
    /// turn.
    pub(crate) fn frame(&self) -> impl std::ops::DerefMut<Target = Gaps> + '_ {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What has not reached the reader yet, and nothing twice.
    pub(crate) fn drain(&self) -> Vec<String> {
        self.frame().drain()
    }
}

/// What a host owed this turn and did not deliver.
///
/// Deliberately not a schema check. Validating against the published protocols
/// means carrying both of them and keeping them current — the same bet as a
/// built-in model catalog, and it goes stale the same week a vendor ships. The
/// question that needs no spec is narrower and the one that matters: the
/// decoder reached for something, it was not there, and the turn came out
/// smaller than the one the model produced. That only ever depends on what pi
/// itself reads, so it cannot fall behind a vendor.
///
/// Both wires report through here rather than each logging in its own words.
/// The two already shared a hole once by sharing the words and not the flow;
/// a reader grepping for what a host got wrong should not have to know which
/// transport was speaking.
pub(crate) struct Gaps {
    format: &'static str,
    /// One line per (event, thing) for the life of the *session*, not the
    /// stream. A malformed delta otherwise repeats once per token and a missing
    /// field once per turn; a defect belongs to the host, and said every turn
    /// it teaches the reader to skip it.
    said: BTreeSet<(String, String)>,
    /// Reported gaps waiting to reach the reader. The journal has them either
    /// way — this is the half that gets seen without being grepped for, which
    /// is the half that matters when every turn is quietly losing content.
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
    /// and says so where a bare `?` used to drop the frame in silence.
    pub(crate) fn owed<'a>(&mut self, frame: &'a Value, event: &str, field: &str) -> Option<&'a str> {
        match frame[field].as_str() {
            Some(found) => Some(found),
            None => {
                self.lost(event, field);
                None
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
        assert_eq!(gaps.owed(&delta, "content_block_delta", "type"), Some("text_delta"));
        assert_eq!(gaps.reported(), 1);
    }

    /// A malformed delta arrives once per token. Said every time, the one line
    /// that matters is buried under two thousand copies of itself.
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
