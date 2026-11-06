//! Transcript writes, off the thread that draws.
//!
//! A session is megabytes by the end of a long run, and writing one whole —
//! pretty JSON, a sync, then another for the rename — takes tens of
//! milliseconds. Done inline after every turn that is a visible hitch, so the
//! write goes to one worker thread instead.
//!
//! One thread and not a pool: writes then land in the order they were asked
//! for, and a save that follows a rename or a rewind cannot be overwritten by
//! an earlier one still in flight.
//!
//! A save whose result the caller must act on — one a move or a rewind depends
//! on — waits for its turn here too, through `now`: still one writer, still one
//! order, and only ever on a path that had to be synchronous anyway.

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

/// Wait for every write handed over so far. Called as the process leaves: a
/// save still queued is exactly the one leaving would have dropped.
pub async fn flush() {
    // The wait is a blocking receive, so it goes off the runtime's own threads.
    let _ = tokio::task::spawn_blocking(|| now(|| ())).await;
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
}
