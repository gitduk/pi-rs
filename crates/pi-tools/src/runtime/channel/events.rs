//! Bounded runtime observation; slow listeners detach without stopping execution.

use super::{EVENT_BYTES as BYTES, EVENT_COUNT as COUNT, Told};
use std::sync::Arc;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Sender {
    to: mpsc::Sender<Queued>,
    budget: Arc<Semaphore>,
    closed: CancellationToken,
}

pub struct Receiver {
    from: mpsc::Receiver<Queued>,
    closed: CancellationToken,
}

struct Queued {
    event: Told,
    _bytes: tokio::sync::OwnedSemaphorePermit,
}

pub fn channel() -> (Sender, Receiver) {
    let (to, from) = mpsc::channel(COUNT);
    let closed = CancellationToken::new();
    (
        Sender {
            to,
            budget: Arc::new(Semaphore::new(BYTES)),
            closed: closed.clone(),
        },
        Receiver { from, closed },
    )
}

impl Sender {
    pub fn send(&self, event: Told) -> Result<(), std::sync::mpsc::SendError<Told>> {
        let bytes = event.retained_bytes();
        let permit = if bytes <= BYTES && !self.closed.is_cancelled() {
            self.budget
                .clone()
                .try_acquire_many_owned(bytes as u32)
                .ok()
        } else {
            None
        };
        let Some(permit) = permit else {
            self.closed.cancel();
            return Err(std::sync::mpsc::SendError(event));
        };
        self.to
            .try_send(Queued {
                event,
                _bytes: permit,
            })
            .map_err(|error| {
                self.closed.cancel();
                std::sync::mpsc::SendError(error.into_inner().event)
            })
    }
}

impl Receiver {
    pub async fn recv(&mut self) -> Option<Told> {
        tokio::select! {biased;
            () = self.closed.cancelled() => None,
            event = self.from.recv() => event.map(|queued|queued.event),
        }
    }

    pub fn try_recv(&mut self) -> Result<Told, mpsc::error::TryRecvError> {
        if self.closed.is_cancelled() {
            return Err(mpsc::error::TryRecvError::Disconnected);
        }
        self.from.try_recv().map(|queued| queued.event)
    }

    pub(crate) fn disconnection(&self) -> CancellationToken {
        self.closed.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn overflow_detaches_the_listener_and_keeps_senders_nonblocking() {
        let (to, mut from) = channel();
        for _ in 0..COUNT {
            to.send(Told::Started).unwrap();
        }
        assert!(to.send(Told::Reply("overflow".into())).is_err());
        assert!(from.recv().await.is_none());
        assert!(to.send(Told::Started).is_err());
    }

    #[tokio::test]
    async fn retained_capacity_counts_against_the_byte_budget() {
        let (to, mut from) = channel();
        let text = String::with_capacity(BYTES);
        assert!(to.send(Told::Progress(text)).is_err());
        assert!(from.recv().await.is_none());
    }

    #[tokio::test]
    async fn reading_releases_capacity_and_a_closed_listener_is_reported() {
        let (to, mut from) = channel();
        for _ in 0..COUNT * 2 {
            to.send(Told::Started).unwrap();
            assert_eq!(from.recv().await, Some(Told::Started));
        }
        drop(from);
        assert!(to.send(Told::Started).is_err());
    }
}
