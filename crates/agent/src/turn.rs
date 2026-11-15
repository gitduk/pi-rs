//! One request to the model: streamed, its deltas said as they arrive,
//! retried while the fault is passing, dropped when the stream goes silent.

use futures::StreamExt;
use llm::request::Request;
use llm::stream::{Accumulator, StreamEvent};
use llm::transport::Transport as _;
use tool::Ctx;

use crate::event::Reporter;
use crate::{Agent, AgentError, Event, Retry};

const DRAFT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const DRAFT_BYTES: usize = 64 * 1024;

pub(crate) struct Streaming<'a> {
    pub input: &'a dyn crate::seams::Input,
    pub entry: crate::session::EntryId,
    pub generating: &'a mut Option<std::time::Duration>,
}

struct Draft {
    batch: crate::session::Draft,
    bytes: usize,
    due: tokio::time::Instant,
    started: bool,
}

impl Draft {
    fn new(entry: crate::session::EntryId) -> Self {
        Self {
            batch: crate::session::Draft {
                id: uuid::Uuid::new_v4().simple().to_string(),
                entry,
                from: 0,
                blocks: Vec::new(),
            },
            bytes: 0,
            due: tokio::time::Instant::now() + DRAFT_INTERVAL,
            started: false,
        }
    }

    fn push(&mut self, event: &StreamEvent) {
        use message::{Block, Reasoning, ReasoningContent};
        let (delta, reasoning) = match event {
            StreamEvent::TextDelta { delta, .. } => (delta, false),
            StreamEvent::ReasoningDelta { delta, .. } => (delta, true),
            _ => return,
        };
        self.bytes += delta.len();
        match (self.batch.blocks.last_mut(), reasoning) {
            (Some(Block::Text(text)), false) => text.push_str(delta),
            (Some(Block::Reasoning(reasoning)), true) => {
                let Some(ReasoningContent::Text { text, .. }) = reasoning.content.last_mut() else {
                    unreachable!("draft reasoning contains one text fragment")
                };
                text.push_str(delta);
            }
            (_, false) => self.batch.blocks.push(Block::Text(delta.clone())),
            (_, true) => self.batch.blocks.push(Block::Reasoning(Reasoning {
                id: None,
                by: None,
                content: vec![ReasoningContent::Text {
                    text: delta.clone(),
                    signature: None,
                }],
            })),
        }
    }

    async fn flush(&mut self, input: &dyn crate::seams::Input) -> Result<(), String> {
        if self.started && self.batch.blocks.is_empty() {
            return Ok(());
        }
        let count = self.batch.blocks.len();
        input.draft(self.batch.clone()).await?;
        self.batch.from += count;
        self.batch.blocks.clear();
        self.started = true;
        self.bytes = 0;
        self.due = tokio::time::Instant::now() + DRAFT_INTERVAL;
        Ok(())
    }
}

fn draft_error(error: String) -> (AgentError, Option<llm::stream::Completion>) {
    (AgentError::Interaction(error), None)
}

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
        reporter: &Reporter,
        retry: &Retry,
        mut state: Streaming<'_>,
    ) -> Result<llm::stream::Completion, (AgentError, Option<llm::stream::Completion>)> {
        let mut attempt = 0usize;
        loop {
            let (err, partial) = match self.attempt(req, ctx, reporter, retry, &mut state).await {
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
            reporter
                .report(Event::Retrying {
                    attempt,
                    delay_ms: delay.as_millis() as u64,
                    reason: e.to_string(),
                })
                .await;
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
        reporter: &Reporter,
        retry: &Retry,
        state: &mut Streaming<'_>,
    ) -> Result<llm::stream::Completion, (AgentError, Option<llm::stream::Completion>)> {
        let mut started: Option<std::time::Instant> = None;
        // A fresh accumulator per attempt: half a stream must not bleed into
        // the message the retry produces.
        let mut acc = Accumulator::new(self.model.spec.model.clone());
        let mut draft = Draft::new(state.entry);
        draft.flush(state.input).await.map_err(draft_error)?;
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
                    draft.flush(state.input).await.map_err(draft_error)?;
                    return Err((AgentError::Cancelled, Some(acc.cut_short())));
                }
                _ = tokio::time::sleep_until(draft.due), if !draft.batch.blocks.is_empty() => {
                    draft.flush(state.input).await.map_err(draft_error)?;
                    continue;
                }
                n = leashed(idle, stream.next()) => match n {
                    Ok(n) => n,
                    // A provider that stops sending mid-stream would otherwise
                    // hold the turn open until the user gives up.
                    Err(e) => {
                        draft.flush(state.input).await.map_err(draft_error)?;
                        return Err((AgentError::Llm(e), None));
                    }
                },
            };
            let Some(ev) = next else { break };
            let ev = match ev {
                Ok(ev) => ev,
                Err(e) => {
                    draft.flush(state.input).await.map_err(draft_error)?;
                    return Err((AgentError::from(e), None));
                }
            };
            draft.push(&ev);
            if draft.bytes >= DRAFT_BYTES {
                draft.flush(state.input).await.map_err(draft_error)?;
            }
            if started.is_none()
                && !matches!(
                    ev,
                    StreamEvent::MessageStart { .. } | StreamEvent::Usage { .. }
                )
            {
                started = Some(std::time::Instant::now());
            }
            let reports_usage = matches!(
                ev,
                StreamEvent::MessageStart { .. }
                    | StreamEvent::Usage { .. }
                    | StreamEvent::Done { .. }
            );
            match &ev {
                StreamEvent::TextDelta { delta, .. } => {
                    reporter.report(Event::TextDelta(delta.clone())).await;
                }
                StreamEvent::ReasoningDelta { delta, .. } => {
                    reporter.report(Event::ReasoningDelta(delta.clone())).await;
                }
                _ => {}
            }
            acc.push(ev);
            if reports_usage {
                reporter.latest(Event::Usage(acc.usage()));
            }
        }
        draft.flush(state.input).await.map_err(draft_error)?;
        *state.generating = started.map(|at| at.elapsed());

        // What the host owed and did not send, said once per session by the
        // reporter. Said here too, not just the log — else it looks ordinary.
        for gap in wire.gaps() {
            reporter.report(Event::Warning(gap)).await;
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
