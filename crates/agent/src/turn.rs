//! One request to the model: streamed, its deltas said as they arrive,
//! retried while the fault is passing, dropped when the stream goes silent.

use futures::StreamExt;
use llm::request::Request;
use llm::stream::{Accumulator, StreamEvent};
use llm::transport::Transport as _;
use tool::Ctx;

use crate::event::Voice;
use crate::{Agent, AgentError, Event, Retry};

impl Agent {
    // Run one request, retrying while the provider says it is a passing problem.
    #[allow(
        clippy::result_large_err,
        reason = "Ok carries a Completion too; boxing Err would not shrink the Result"
    )]
    pub(crate) async fn stream_turn(
        &self,
        req: &Request,
        ctx: &Ctx,
        voice: &Voice,
        retry: &Retry,
        generating: &mut Option<std::time::Duration>,
    ) -> Result<llm::stream::Completion, (AgentError, Option<llm::stream::Completion>)> {
        let mut attempt = 0usize;
        loop {
            let (err, partial) = match self.attempt(req, ctx, voice, retry, generating).await {
                Ok(done) => return Ok(done),
                Err(err) => err,
            };
            if matches!(err, AgentError::Cancelled) {
                return Err((err, partial));
            }
            let e = match err {
                AgentError::Llm(e) => e,
                other => return Err((other, partial)),
            };

            if attempt >= retry.attempts || llm::classify(&e) != llm::Fault::Transient {
                // The classification, not just the error: "why was this not
                // retried" is answerable from the fault and from nothing else.
                tracing::error!(
                    target: "pi::wire",
                    attempts = attempt,
                    fault = ?llm::classify(&e),
                    error = %e,
                    "giving up"
                );
                return Err((AgentError::Llm(e), partial));
            }

            attempt += 1;
            let delay = retry.delay(attempt);
            voice.say(Event::Retrying {
                attempt,
                delay_ms: delay.as_millis() as u64,
                reason: e.to_string(),
            });
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = ctx.cancel.cancelled() => return Err((AgentError::Cancelled, None)),
            }
        }
    }

    // One attempt. Deltas reach the renderer as they arrive, so a retry shows
    // as a false start — which the Retrying event is there to explain.
    #[allow(
        clippy::result_large_err,
        reason = "Ok carries a Completion too; boxing Err would not shrink the Result"
    )]
    /// `generating` comes back as how long the model took to stream what it
    /// sent: from the first piece of it to the end, waiting and reading the
    /// prompt left out, so a long context does not read as a slow model.
    async fn attempt(
        &self,
        req: &Request,
        ctx: &Ctx,
        voice: &Voice,
        retry: &Retry,
        generating: &mut Option<std::time::Duration>,
    ) -> Result<llm::stream::Completion, (AgentError, Option<llm::stream::Completion>)> {
        let mut started: Option<std::time::Instant> = None;
        // A fresh accumulator per attempt: half a stream must not bleed into
        // the message the retry produces.
        let mut acc = Accumulator::new(self.model.spec.model.clone());
        let idle = retry.idle;

        let wire = self.wire();
        // Biased, cancel first: a stop asked for must win over a reply that is
        // already buffered, which an unbiased pick loses half the time.
        let mut stream = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err((AgentError::Cancelled, None)),
            r = leashed(idle, wire.stream(&self.model.spec, req)) => match r {
                Ok(r) => r.map_err(|e| (AgentError::from(e), None))?,
                Err(e) => return Err((AgentError::Llm(e), None)),
            },
        };

        loop {
            let next = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => {
                    return Err((AgentError::Cancelled, Some(acc.finish())));
                }
                n = leashed(idle, stream.next()) => match n {
                    Ok(n) => n,
                    // A provider that stops sending mid-stream would otherwise
                    // hold the turn open until the user gives up.
                    Err(e) => return Err((AgentError::Llm(e), None)),
                },
            };
            let Some(ev) = next else { break };
            let ev = match ev {
                Ok(ev) => ev,
                Err(e) => return Err((AgentError::from(e), None)),
            };
            if started.is_none() && !matches!(ev, StreamEvent::MessageStart { .. }) {
                started = Some(std::time::Instant::now());
            }
            match &ev {
                StreamEvent::TextDelta { delta, .. } => {
                    voice.say(Event::TextDelta(delta.clone()));
                }
                StreamEvent::ReasoningDelta { delta, .. } => {
                    voice.say(Event::ReasoningDelta(delta.clone()));
                }
                // The one measured number that arrives before the answer does.
                StreamEvent::MessageStart { usage, .. } => {
                    voice.say(Event::Usage(*usage));
                }
                _ => {}
            }
            acc.push(ev);
        }
        *generating = started.map(|at| at.elapsed());

        // What the host owed and did not send, said once per session by the
        // reporter. Said here too, not just the log — else it looks ordinary.
        for gap in wire.gaps() {
            voice.say(Event::Warning(gap));
        }

        Ok(acc.finish())
    }
}

fn wedged(idle: std::time::Duration) -> llm::LlmError {
    let idle = llm::figures::elapsed(idle);
    llm::LlmError::Stream(format!("the stream sent nothing for {idle}"))
}

// The leash every provider call here keeps: silence past `idle` reads as a
// wedged stream, whether the call drives a turn or a compaction.
pub(crate) async fn leashed<T>(
    idle: std::time::Duration,
    fut: impl std::future::Future<Output = T>,
) -> llm::Result<T> {
    tokio::time::timeout(idle, fut)
        .await
        .map_err(|_| wedged(idle))
}
