//! The terminal itself: the reader thread that turns key presses into events,
//! holding it while a child owns the screen, and the file an external editor
//! works in.
use crossterm::event::Event as TermEvent;
use pi_store::session::Store;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedReceiver;

// crossterm reads blockingly, so input runs on its own thread. `poll`
// wakes the moment a key arrives, costing only idle wakeups.
const INPUT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

// How long `park` waits to be told the reader stopped. Twice the poll: one
// just entered has that long before it looks at the flag again.
const PARK_WAIT: std::time::Duration =
    std::time::Duration::from_millis(INPUT_POLL.as_millis() as u64 * 2);

pub(super) const PARK_STEP: std::time::Duration = std::time::Duration::from_millis(10);

// The reader's pause switch. Two readers on one stdin would split the user's
// keystrokes between them, and a thread inside `read` cannot be told to stop.
#[derive(Clone, Default)]
pub(super) struct Hold {
    pub(super) paused: Arc<AtomicBool>,
    pub(super) parked: Arc<AtomicBool>,
}

impl Hold {
    // Stop reading, and wait to be told it stopped. Bounded: a reader starved
    // past `PARK_WAIT` is handed the race rather than hanging the editor.
    pub(super) async fn park(&self) -> Parked {
        self.paused.store(true, Ordering::Release);
        let mut waited = std::time::Duration::ZERO;
        while waited < PARK_WAIT && !self.parked.load(Ordering::Acquire) {
            tokio::time::sleep(PARK_STEP).await;
            waited += PARK_STEP;
        }
        Parked(self.clone())
    }
}

// SIGINT and SIGQUIT ignored while a child holds the terminal. Cooked mode
// sends both to the whole foreground group, which this process is in.
#[cfg(unix)]
pub(super) struct Deafened([libc::sighandler_t; 2]);

#[cfg(unix)]
impl Deafened {
    pub(super) fn new() -> Self {
        // SAFETY: `signal` is the process-wide disposition; `Drop` puts back
        // exactly what is read here.
        unsafe {
            Self([
                libc::signal(libc::SIGINT, libc::SIG_IGN),
                libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            ])
        }
    }
}

#[cfg(unix)]
impl Drop for Deafened {
    fn drop(&mut self) {
        for (signal, prior) in [(libc::SIGINT, self.0[0]), (libc::SIGQUIT, self.0[1])] {
            // A disposition that could not be read is not one to restore.
            if prior != libc::SIG_ERR {
                // SAFETY: putting back what `new` took, in the same process.
                unsafe { libc::signal(signal, prior) };
            }
        }
    }
}

#[cfg(not(unix))]
pub(super) struct Deafened;

#[cfg(not(unix))]
impl Deafened {
    pub(super) fn new() -> Self {
        Self
    }
}

// Restarts the reader on the way out, however it goes. A reader left parked
// is a dead keyboard with nothing on screen to say why.
pub(super) struct Parked(Hold);

impl Drop for Parked {
    fn drop(&mut self) {
        self.0.paused.store(false, Ordering::Release);
    }
}

pub(super) fn reader() -> (UnboundedReceiver<TermEvent>, Hold) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Hold::default();
    let mine = hold.clone();
    std::thread::spawn(move || {
        loop {
            if mine.paused.load(Ordering::Acquire) {
                mine.parked.store(true, Ordering::Release);
                std::thread::sleep(INPUT_POLL);
                continue;
            }
            mine.parked.store(false, Ordering::Release);
            match crossterm::event::poll(INPUT_POLL) {
                // Re-checked: the terminal may have gone to a child while this
                // poll was waiting, and that keystroke belongs to the child now.
                Ok(true) if !mine.paused.load(Ordering::Acquire) => {
                    match crossterm::event::read() {
                        Ok(event) => {
                            if tx.send(event).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
    (rx, hold)
}

// How long leaving waits for cancelled runs: long enough to notice the
// token, short enough a wedged one won't hold the terminal hostage.
pub(super) const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

// `$VISUAL` before `$EDITOR` before `vi`: the order other terminal
// programs use.
pub(super) fn external_editor() -> (String, Vec<String>) {
    let raw = ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()));
    split_editor(raw.as_deref().unwrap_or("vi"))
}

// Split rather than run whole (`code -w`), and never through a shell, which
// would make every character in it live. The price is that quoting cannot.
fn split_editor(raw: &str) -> (String, Vec<String>) {
    let mut parts = raw.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_string();
    (program, parts.map(str::to_string).collect())
}

// A file for the editor to work in, `0600` because `/tmp` is shared and the
// line holds whatever the user was about to say. `.md` buys highlighting.
pub(super) fn scratch_file(text: &str) -> std::io::Result<std::path::PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("pi-edit-{}-{stamp}.md", std::process::id()));
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open.open(&path)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    Ok(path)
}

// What this run recalls in `workspace`, or nothing.
pub(super) fn history_of(store: &Store, workspace: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(store.history_path(workspace))
        .map(|prior| super::editor::decode(&prior))
        .unwrap_or_default()
}

pub(super) fn drop_shared_history() {
    if let Some(old) = pi_store::dir().map(|d| d.join("history")) {
        let _ = std::fs::remove_file(&old);
        let _ = std::fs::remove_dir_all(&old);
    }
}

// Enough to recall from without the file growing without bound.
pub(super) const HISTORY_KEEP: usize = 1_000;
