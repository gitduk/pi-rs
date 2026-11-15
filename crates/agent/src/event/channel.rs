//! Bounded event delivery. Complete facts wait; replaceable progress coalesces.

use super::Event;
use futures::task::AtomicWaker;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::{
    Notify,
    mpsc::error::{SendError, TryRecvError, TrySendError},
};

const CAPACITY: usize = 256;
const BYTES: usize = 8 * 1024 * 1024;

struct Shared {
    state: Mutex<State>,
    readable: AtomicWaker,
    writable: Notify,
}

struct State {
    queue: VecDeque<(Event, usize)>,
    bytes: usize,
    senders: usize,
    closed: bool,
}

pub struct Sender(Arc<Shared>);
pub struct Receiver(Arc<Shared>);

pub fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            bytes: 0,
            senders: 1,
            closed: false,
        }),
        readable: AtomicWaker::new(),
        writable: Notify::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.0.state.lock().expect("event channel poisoned").senders += 1;
        Self(self.0.clone())
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.0.state.lock().expect("event channel poisoned").senders -= 1;
        self.0.readable.wake();
    }
}

impl Sender {
    pub async fn send(&self, mut event: Event) -> Result<(), SendError<Event>> {
        let bytes = event.retained_bytes();
        loop {
            let available = self.0.writable.notified();
            tokio::pin!(available);
            available.as_mut().enable();
            match self.enqueue(event, bytes) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed(event)) => return Err(SendError(event)),
                Err(TrySendError::Full(pending)) => event = pending,
            }
            available.await;
        }
    }

    pub fn try_send(&self, event: Event) -> Result<(), TrySendError<Event>> {
        let bytes = event.retained_bytes();
        self.enqueue(event, bytes)
    }

    fn enqueue(&self, event: Event, bytes: usize) -> Result<(), TrySendError<Event>> {
        let mut state = self.0.state.lock().expect("event channel poisoned");
        if state.closed {
            return Err(TrySendError::Closed(event));
        }
        if state.queue.len() >= CAPACITY || state.bytes.saturating_add(bytes) > BYTES {
            state.queue.retain(|(event, _)| !event.is_progress());
            state.bytes = state.queue.iter().map(|(_, bytes)| bytes).sum();
        }
        // A single complete fact may exceed the byte budget. It enters alone,
        // so a large atomic result neither deadlocks nor loses its content.
        if !state.queue.is_empty()
            && (state.queue.len() >= CAPACITY || state.bytes.saturating_add(bytes) > BYTES)
        {
            return Err(TrySendError::Full(event));
        }
        state.bytes += bytes;
        state.queue.push_back((event, bytes));
        drop(state);
        self.0.readable.wake();
        Ok(())
    }

    pub(super) fn latest(&self, event: Event) {
        let bytes = event.retained_bytes();
        let mut state = self.0.state.lock().expect("event channel poisoned");
        if state.closed {
            return;
        }
        if let Some(at) = state.queue.iter().position(|(old, _)| event.replaces(old)) {
            let (_, old_bytes) = state.queue.remove(at).expect("located progress");
            state.bytes -= old_bytes;
        }
        drop(state);
        // Progress is optional; completion remains reliable even under pressure.
        let _ = self.enqueue(event, bytes);
    }
}

impl Receiver {
    pub fn try_recv(&mut self) -> Result<Event, TryRecvError> {
        let mut state = self.0.state.lock().expect("event channel poisoned");
        if let Some((event, bytes)) = state.queue.pop_front() {
            state.bytes -= bytes;
            drop(state);
            self.0.writable.notify_waiters();
            Ok(event)
        } else if state.senders == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }

    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.0.readable.register(cx.waker());
        match self.try_recv() {
            Ok(event) => Poll::Ready(Some(event)),
            Err(TryRecvError::Disconnected) => Poll::Ready(None),
            Err(TryRecvError::Empty) => Poll::Pending,
        }
    }

    pub async fn recv(&mut self) -> Option<Event> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().expect("event channel poisoned");
        state.closed = true;
        state.queue.clear();
        state.bytes = 0;
        drop(state);
        self.0.writable.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_slow_receiver_applies_backpressure_without_losing_text() {
        let (tx, mut rx) = channel();
        for n in 0..CAPACITY {
            tx.try_send(Event::TextDelta(n.to_string())).unwrap();
        }
        let sending = tx.send(Event::TextDelta("last".into()));
        tokio::pin!(sending);
        assert!(futures::poll!(&mut sending).is_pending());
        assert_eq!(rx.recv().await, Some(Event::TextDelta("0".into())));
        sending.await.unwrap();
        for n in 1..CAPACITY {
            assert_eq!(rx.recv().await, Some(Event::TextDelta(n.to_string())));
        }
        assert_eq!(rx.recv().await, Some(Event::TextDelta("last".into())));
        for n in 0..10_000 {
            tx.latest(Event::ToolProgress {
                id: "call".into(),
                text: n.to_string(),
            });
        }
        assert_eq!(
            rx.recv().await,
            Some(Event::ToolProgress {
                id: "call".into(),
                text: "9999".into()
            })
        );
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    }

    #[tokio::test]
    async fn cancellation_releases_a_reporter_waiting_for_a_receiver() {
        let (tx, _rx) = channel();
        for _ in 0..CAPACITY {
            tx.try_send(Event::TextDelta("buffered".into())).unwrap();
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        let reporter =
            super::super::Reporter::new(tx, Arc::new(crate::NoHooks)).with_cancel(cancel.clone());
        let reporting = reporter.warn("waiting".into());
        tokio::pin!(reporting);
        assert!(futures::poll!(&mut reporting).is_pending());
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), reporting)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn byte_pressure_keeps_complete_results_and_releases_closed_senders() {
        let (tx, mut rx) = channel();
        let half = BYTES / 2 - std::mem::size_of::<Event>();
        tx.try_send(Event::TextDelta("x".repeat(half))).unwrap();
        tx.try_send(Event::TextDelta("y".repeat(half))).unwrap();
        assert!(matches!(
            tx.try_send(Event::TextDelta("wait".into())),
            Err(TrySendError::Full(_))
        ));
        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        tx.latest(Event::ToolProgress {
            id: "other".into(),
            text: "running".into(),
        });
        let large = Event::TextDelta("complete".repeat(BYTES / 4));
        tokio::time::timeout(std::time::Duration::from_secs(1), tx.send(large.clone()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rx.recv().await, Some(large));
        drop(rx);
        assert!(tx.send(Event::Warning("closed".into())).await.is_err());
    }
}
