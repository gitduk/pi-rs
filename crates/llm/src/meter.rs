//! What calls have cost, counted where every call leaves for the provider:
//! one place, so no path to it — a turn, a retry, a summary, a child run —
//! can go around the count.

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::stream::{BoxStream, Stream, StreamExt};

use crate::Result;
use crate::model::{ModelSpec, Pricing};
use crate::request::Request;
use crate::stream::{StreamEvent, Usage};
use crate::totals::Totals;
use crate::transport::Transport;

/// The running total of every call made through a [`Metered`] wire on it.
#[derive(Debug, Default)]
pub struct Meter(Mutex<Totals>);

impl Meter {
    pub fn read(&self) -> Totals {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count on from `from` instead: whoever the calls are charged to has
    /// changed, and brings its own figure.
    pub fn reset(&self, from: Totals) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = from;
    }

    fn charge(&self, usage: &Usage, cost: f64) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add(usage, cost);
    }
}

/// A wire whose every call is charged to a meter, priced at the model the
/// call went to rather than at whatever the caller runs.
#[derive(Clone, Copy)]
pub struct Metered<'a> {
    inner: &'a dyn Transport,
    meter: &'a Arc<Meter>,
}

impl<'a> Metered<'a> {
    pub fn new(inner: &'a dyn Transport, meter: &'a Arc<Meter>) -> Self {
        Self { inner, meter }
    }

    /// Another wire on the same meter: a summary written by a model of its
    /// own is still spent by whoever asked for it.
    pub fn over(self, inner: &'a dyn Transport) -> Self {
        Self { inner, ..self }
    }
}

#[async_trait]
impl Transport for Metered<'_> {
    fn gaps(&self) -> Vec<String> {
        self.inner.gaps()
    }

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let events = self.inner.stream(spec, req).await?;
        Ok(Counted {
            events,
            usage: Usage::default(),
            pricing: spec.pricing,
            meter: self.meter.clone(),
        }
        .boxed())
    }
}

// Charged when dropped rather than at `Done`: a call cut short, or abandoned
// for a retry, was still billed for what it had reported.
struct Counted {
    events: BoxStream<'static, Result<StreamEvent>>,
    usage: Usage,
    pricing: Pricing,
    meter: Arc<Meter>,
}

impl Stream for Counted {
    type Item = Result<StreamEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let polled = this.events.poll_next_unpin(cx);
        if let Poll::Ready(Some(Ok(ev))) = &polled {
            this.usage.observe(ev);
        }
        polled
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.meter
            .charge(&self.usage, self.pricing.cost(&self.usage));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StopReason;
    use futures::executor::block_on;

    struct Canned(Vec<StreamEvent>);

    #[async_trait]
    impl Transport for Canned {
        async fn stream(
            &self,
            _: &ModelSpec,
            _: &Request,
        ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
            Ok(futures::stream::iter(self.0.clone().into_iter().map(Ok)).boxed())
        }
    }

    fn priced(input_per_mtok: f64) -> ModelSpec {
        ModelSpec {
            pricing: Pricing {
                input_per_mtok,
                ..Default::default()
            },
            ..ModelSpec::test()
        }
    }

    fn req() -> Request {
        Request {
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            effort: crate::request::Effort::Off,
            tool_choice: crate::request::ToolChoice::None,
        }
    }

    fn input(n: u64) -> Usage {
        Usage {
            input: n,
            ..Default::default()
        }
    }

    // Each call at its own model's rate, both wires on the one meter.
    #[test]
    fn every_call_is_charged_at_the_rate_of_the_model_it_went_to() {
        let meter = Arc::new(Meter::default());
        let run = Canned(vec![StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: input(1_000_000),
        }]);
        let writer = Canned(run.0.clone());
        let wire = Metered::new(&run, &meter);
        block_on(async {
            let s = wire.stream(&priced(1.0), &req()).await.unwrap();
            s.collect::<Vec<_>>().await;
            let s = wire
                .over(&writer)
                .stream(&priced(3.0), &req())
                .await
                .unwrap();
            s.collect::<Vec<_>>().await;
        });
        assert_eq!(meter.read().usage.input, 2_000_000);
        assert_eq!(meter.read().cost, 4.0);
    }

    // A stream given up on after its start frame still cost its prompt.
    #[test]
    fn a_call_cut_short_is_charged_for_what_it_reported() {
        let meter = Arc::new(Meter::default());
        let wire = Canned(vec![
            StreamEvent::MessageStart { usage: input(500) },
            StreamEvent::TextDelta {
                index: 0,
                delta: "half".into(),
            },
        ]);
        block_on(async {
            let mut s = Metered::new(&wire, &meter)
                .stream(&priced(0.0), &req())
                .await
                .unwrap();
            s.next().await;
        });
        assert_eq!(meter.read().usage.input, 500);
    }
}
