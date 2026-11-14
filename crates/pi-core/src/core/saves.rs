//! Ordered conversation transactions on a dedicated writer thread.
//!
//! Model checkpoints await durability without blocking the runtime. A move or
//! rewind uses the same writer, so an older save cannot overwrite its result.

use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, channel};

type Job = Box<dyn FnOnce() + Send>;

struct Saver {
    jobs: Sender<Job>,
}

fn saver() -> &'static Saver {
    static SAVER: OnceLock<Saver> = OnceLock::new();
    SAVER.get_or_init(|| {
        let (jobs, queue) = channel::<Job>();
        std::thread::Builder::new()
            .name("pi-save".into())
            .spawn(move || work(queue))
            .expect("a thread to write transcripts on");
        Saver { jobs }
    })
}

fn work(queue: Receiver<Job>) {
    for job in queue {
        job();
    }
}

/// Hand `job` to the saver: it runs before anything asked for later, and the
/// caller does not wait for it.
pub(crate) fn behind(job: impl FnOnce() + Send + 'static) {
    if saver().jobs.send(Box::new(job)).is_err() {
        tracing::error!(target: "pi::session", "the transcript saver is gone");
    }
}

/// The same, waited for: for a write something else depends on being on disk.
pub(crate) fn now<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, answer) = channel();
    behind(move || {
        let _ = done.send(job());
    });
    answer
        .recv()
        .expect("the transcript saver answers every job it takes")
}

/// Await the same writer without occupying a blocking worker while it runs.
pub(crate) async fn wait<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::sync::oneshot::error::RecvError> {
    let (done, answer) = tokio::sync::oneshot::channel();
    behind(move || {
        let _ = done.send(job());
    });
    answer.await
}

/// Wait for every write handed over so far. Called as the process leaves: a
/// save still queued is exactly the one leaving would have dropped.
pub async fn flush() {
    let _ = wait(|| ()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn jobs_run_in_the_order_they_were_asked_for() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        for i in 0..64 {
            let seen = seen.clone();
            behind(move || seen.lock().unwrap().push(i));
        }
        // A waited-for job is the barrier: everything before it has run.
        now(|| ());
        assert_eq!(*seen.lock().unwrap(), (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn a_waited_for_job_answers_with_what_it_made() {
        assert_eq!(now(|| 7 * 6), 42);
    }

    #[test]
    fn a_waited_for_job_queues_behind_one_already_handed_over() {
        let ran = Arc::new(AtomicUsize::new(0));
        let first = ran.clone();
        behind(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            first.fetch_add(1, Ordering::SeqCst);
        });
        // Read after the barrier: the earlier job finished before this ran.
        let after = ran.clone();
        let seen = now(move || after.load(Ordering::SeqCst));
        assert_eq!(seen, 1);
    }

    #[tokio::test]
    async fn cancelling_an_async_wait_does_not_cancel_the_queued_write() {
        let written = Arc::new(AtomicUsize::new(0));
        let began = Arc::new(tokio::sync::Notify::new());
        let (release, ready) = channel();
        let saved = written.clone();
        let started = began.clone();
        let waiter = tokio::spawn(wait(move || {
            started.notify_one();
            let _ = ready.recv();
            saved.store(1, Ordering::SeqCst);
        }));
        began.notified().await;
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        flush().await;
        assert_eq!(written.load(Ordering::SeqCst), 1);
    }
}
