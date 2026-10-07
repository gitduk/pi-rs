//! `later`: a prompt the model leaves for itself, to come back to its checkout
//! after a delay, on a period, or when a background command exits.
//!
//! It drives a lane from outside, as `/loop` does: a due prompt goes only
//! when the lane is free and nothing typed is waiting. It lasts as long as
//! this process; work that must outlive pi belongs to the system's scheduler.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tokio::time::Instant;
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

// A shorter period would have the model wake itself faster than it can work.
const MIN_PERIOD: Duration = Duration::from_secs(60);
// How much of a background command's output comes back with its prompt.
const TAIL: usize = 4 << 10;

/// Every prompt left for later, in this process.
#[derive(Default)]
pub struct Table {
    inner: Mutex<Inner>,
    changed: Notify,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    items: Vec<Item>,
}

struct Item {
    id: u64,
    // The checkout it comes back to: one lane holds one.
    root: PathBuf,
    prompt: String,
    when: When,
}

enum When {
    At(Instant),
    Every {
        period: Duration,
        next: Instant,
    },
    // Running until `ended` is filled in; the task dies with the item.
    Done {
        command: String,
        ended: Option<String>,
        task: tokio::task::AbortHandle,
    },
}

/// A prompt that has come due: the line to submit and the note before it.
pub struct Due {
    pub line: String,
    pub note: String,
}

impl Table {
    /// Signalled whenever something is added, removed or finishes, so a
    /// surface waiting on the earliest time can look again.
    pub fn changed(&self) -> &Notify {
        &self.changed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// When the next prompt for `root` comes due; now, if one already has.
    pub fn next_due(&self, root: &Path) -> Option<Instant> {
        self.lock()
            .items
            .iter()
            .filter(|i| i.root == root)
            .filter_map(|i| match &i.when {
                When::At(at) => Some(*at),
                When::Every { next, .. } => Some(*next),
                When::Done { ended: Some(_), .. } => Some(Instant::now()),
                When::Done { ended: None, .. } => None,
            })
            .min()
    }

    /// The first prompt for `root` that is due, taken: a one-off goes, a
    /// period moves on to its next time.
    pub fn take_due(&self, root: &Path) -> Option<Due> {
        let now = Instant::now();
        let mut inner = self.lock();
        let at = inner.items.iter().position(|i| {
            i.root == root
                && match &i.when {
                    When::At(at) => *at <= now,
                    When::Every { next, .. } => *next <= now,
                    When::Done { ended, .. } => ended.is_some(),
                }
        })?;
        let item = &mut inner.items[at];
        let note = format!(
            "This turn is `later` #{}, which you left for yourself; the user did not just type it.",
            item.id
        );
        let (line, repeats) = match &mut item.when {
            When::Every { period, next } => {
                // From now, not from the missed time: a lane busy for an hour
                // owes one round, not sixty.
                *next = now + *period;
                (item.prompt.clone(), true)
            }
            When::At(_) => (item.prompt.clone(), false),
            When::Done { command, ended, .. } => (
                format!(
                    "{}\n\n`{command}` {}",
                    item.prompt,
                    ended.take().unwrap_or_default()
                ),
                false,
            ),
        };
        if !repeats {
            inner.items.remove(at);
        }
        Some(Due { line, note })
    }

    /// What is pending for `root`, one line each.
    pub fn listing(&self, root: &Path) -> Vec<String> {
        let now = Instant::now();
        self.lock()
            .items
            .iter()
            .filter(|i| i.root == root)
            .map(|i| {
                let when = match &i.when {
                    When::At(at) => format!("in {}", span(at.saturating_duration_since(now))),
                    When::Every { period, next } => format!(
                        "every {}, next in {}",
                        span(*period),
                        span(next.saturating_duration_since(now))
                    ),
                    When::Done {
                        command,
                        ended: None,
                        ..
                    } => format!("when `{command}` exits"),
                    When::Done { command, .. } => format!("`{command}` has exited"),
                };
                format!("#{}  {when}: {}", i.id, i.prompt)
            })
            .collect()
    }

    /// Remove `id` from `root`'s, stopping its command if one runs.
    pub fn cancel(&self, root: &Path, id: u64) -> Result<String, String> {
        let mut inner = self.lock();
        let at = inner
            .items
            .iter()
            .position(|i| i.id == id && i.root == root)
            .ok_or_else(|| format!("no `later` #{id} here"))?;
        let item = inner.items.remove(at);
        if let When::Done { task, .. } = &item.when {
            task.abort();
        }
        drop(inner);
        self.changed.notify_one();
        Ok(format!("cancelled #{id}: {}", item.prompt))
    }

    /// Forget everything left for checkouts at or under `root`, stopping
    /// their commands: the checkout is gone, and a new one there is not it.
    pub fn drop_under(&self, root: &Path) {
        let mut inner = self.lock();
        inner.items.retain(|i| {
            let gone = i.root.starts_with(root);
            if gone && let When::Done { task, .. } = &i.when {
                task.abort();
            }
            !gone
        });
        drop(inner);
        self.changed.notify_one();
    }

    fn add(&self, root: PathBuf, prompt: String, when: impl FnOnce(u64) -> When) -> u64 {
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        let when = when(id);
        inner.items.push(Item {
            id,
            root,
            prompt,
            when,
        });
        drop(inner);
        self.changed.notify_one();
        id
    }

    // A background command has exited: its prompt is due.
    fn ended(&self, id: u64, said: String) {
        if let Some(item) = self.lock().items.iter_mut().find(|i| i.id == id)
            && let When::Done { ended, .. } = &mut item.when
        {
            *ended = Some(said);
        }
        self.changed.notify_one();
    }
}

/// The `later` tool, writing into one table.
pub struct Later {
    table: Arc<Table>,
}

impl Later {
    pub const NAME: &'static str = "later";

    pub fn new(table: Arc<Table>) -> Self {
        Self { table }
    }
}

#[async_trait]
impl Tool for Later {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Leave yourself a prompt that comes back to this checkout as a turn of \
         its own: after a delay, on a period, or when a background command \
         exits — a CI run, a deploy, a long build. It arrives once nothing else \
         is running, so end this turn rather than wait for it. Give one of \
         `after`, `every` or `when_done`, with `prompt`. It lasts while pi runs. \
         With no arguments, lists what is pending; `cancel` removes one by id."
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "What to do when it comes due, written as a request: it arrives with nothing of this turn but what you put in it.",
                },
                "after": { "type": "string", "description": "A delay: 30s, 10m, 2h, 1d." },
                "every": { "type": "string", "description": "A period, at least 1m: 1h, 1d." },
                "when_done": {
                    "type": "string",
                    "description": "A shell command run in the background from the workspace root. Its exit status and the end of its output come with the prompt.",
                },
                "cancel": { "type": "integer", "description": "The id of one to remove." },
            },
        })
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let root = ctx.workspace.root().to_path_buf();
        let text = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let given = [text("after"), text("every"), text("when_done")];
        if let Some(id) = args.get("cancel").and_then(Value::as_u64) {
            if given.iter().any(Option::is_some) {
                return Err(ToolError::Invalid(
                    "`cancel` is a call of its own; set the new one in another".into(),
                ));
            }
            return self
                .table
                .cancel(&root, id)
                .map(ToolOutput::text)
                .map_err(ToolError::Invalid);
        }
        if given.iter().all(Option::is_none) && text("prompt").is_some() {
            return Err(ToolError::Invalid(
                "a prompt needs one of `after`, `every` or `when_done` to say when".into(),
            ));
        }
        if given.iter().all(Option::is_none) {
            let pending = self.table.listing(&root);
            return Ok(ToolOutput::text(if pending.is_empty() {
                "nothing pending".to_string()
            } else {
                pending.join("\n")
            }));
        }
        if given.iter().flatten().count() > 1 {
            return Err(ToolError::Invalid(
                "give one of `after`, `every` or `when_done`".into(),
            ));
        }
        let prompt = text("prompt")
            .ok_or_else(|| ToolError::Invalid("`prompt` says what to do when it comes due".into()))?
            .to_string();
        let id = if let Some(after) = text("after") {
            let at = Instant::now() + duration(after)?;
            self.table.add(root, prompt, |_| When::At(at))
        } else if let Some(every) = text("every") {
            let period = duration(every)?;
            if period < MIN_PERIOD {
                return Err(ToolError::Invalid("`every` is at least 1m".into()));
            }
            let next = Instant::now() + period;
            self.table
                .add(root, prompt, |_| When::Every { period, next })
        } else {
            let command = text("when_done").unwrap_or_default().to_string();
            let table = self.table.clone();
            let cwd = root.clone();
            self.table.add(root, prompt, |id| {
                let run = command.clone();
                let task = tokio::spawn(async move {
                    let said = watch(&run, &cwd).await;
                    table.ended(id, said);
                });
                When::Done {
                    command,
                    ended: None,
                    task: task.abort_handle(),
                }
            })
        };
        Ok(ToolOutput::text(format!(
            "set as #{id}; it comes back as a turn of its own, so this one can end"
        )))
    }
}

// Run `command` to its end, and say how it ended with the tail of what it
// printed, stdout and stderr as one stream.
async fn watch(command: &str, cwd: &Path) -> String {
    let mut command_ = tokio::process::Command::new("sh");
    command_
        .arg("-c")
        .arg(format!("{{ {command}\n}} 2>&1"))
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Its own group, so cancelling takes what the shell started too.
    #[cfg(unix)]
    command_.process_group(0);
    let mut child = match command_.spawn() {
        Ok(child) => child,
        Err(e) => return format!("did not start: {e}"),
    };
    let _group = child.id().map(Group);
    let mut tail: Vec<u8> = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = out.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > 2 * TAIL {
                tail.drain(..tail.len() - TAIL);
            }
        }
    }
    let status = match child.wait().await {
        Ok(status) => status.to_string(),
        Err(e) => format!("could not be waited on: {e}"),
    };
    let text = String::from_utf8_lossy(&tail);
    let text = llm::slice::tail_bytes(&text, TAIL).trim();
    if text.is_empty() {
        format!("{status}, printing nothing.")
    } else {
        format!("{status}. The end of its output:\n```\n{text}\n```")
    }
}

// A watched command's process group, killed when the watch is dropped:
// cancelled, removed with its checkout, or pi gone.
#[cfg_attr(not(unix), allow(dead_code))]
struct Group(u32);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: a signal to a group this watch made; a gone one is ESRCH.
        unsafe {
            libc::killpg(self.0 as libc::pid_t, libc::SIGKILL);
        }
    }
}

// `30s`, `10m`, `2h`, `1d`.
fn duration(spec: &str) -> Result<Duration, ToolError> {
    let bad = || ToolError::Invalid(format!("`{spec}` is not a span like 30s, 10m, 2h or 1d"));
    let unit = spec.chars().last().ok_or_else(bad)?;
    let n: u64 = spec[..spec.len() - unit.len_utf8()]
        .trim()
        .parse()
        .map_err(|_| bad())?;
    let per = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    // Past a year the clock it is added to may not reach; nobody waits so long.
    n.checked_mul(per)
        .filter(|&secs| secs <= 365 * 86_400)
        .map(Duration::from_secs)
        .ok_or_else(|| ToolError::Invalid(format!("`{spec}` is longer than a year")))
}

// A span as the listing says it, to the largest whole unit.
fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_reads_its_unit() {
        assert_eq!(duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(duration("1d").unwrap(), Duration::from_secs(86_400));
        assert!(duration("10").is_err());
        assert!(
            duration("99999999999999999d").is_err(),
            "overflow is refused"
        );
        assert!(duration("366d").is_err());
        assert!(duration("m").is_err());
    }

    // A period comes back once however long the lane was busy, then moves
    // on from now; a one-off goes once taken.
    #[tokio::test]
    async fn a_due_prompt_is_taken_once() {
        let table = Table::default();
        let root = PathBuf::from("/w");
        let past = Instant::now() - Duration::from_secs(5);
        table.add(root.clone(), "once".into(), |_| When::At(past));
        table.add(root.clone(), "often".into(), |_| When::Every {
            period: MIN_PERIOD,
            next: past,
        });
        assert!(table.take_due(Path::new("/elsewhere")).is_none());
        assert_eq!(table.take_due(&root).unwrap().line, "once");
        assert_eq!(table.take_due(&root).unwrap().line, "often");
        assert!(table.take_due(&root).is_none(), "the period moved on");
        assert_eq!(table.listing(&root).len(), 1);
    }

    // A removed checkout takes what was left for it: a new one at the same
    // path is not it.
    #[tokio::test]
    async fn a_removed_checkout_takes_its_prompts_along() {
        let table = Table::default();
        let soon = Instant::now();
        table.add("/repo.worktrees/a".into(), "a".into(), |_| When::At(soon));
        table.add("/repo".into(), "main".into(), |_| When::At(soon));
        table.drop_under(Path::new("/repo.worktrees/a"));
        assert!(table.listing(Path::new("/repo.worktrees/a")).is_empty());
        assert_eq!(table.listing(Path::new("/repo")).len(), 1);
    }

    #[tokio::test]
    async fn a_finished_command_brings_its_end_with_the_prompt() {
        let table = Arc::new(Table::default());
        let dir = tempfile::tempdir().unwrap();
        let said = watch("echo built; exit 3", dir.path()).await;
        let id = table.add(dir.path().to_path_buf(), "check the build".into(), |_| {
            When::Done {
                command: "make".into(),
                ended: None,
                task: tokio::spawn(async {}).abort_handle(),
            }
        });
        assert!(table.take_due(dir.path()).is_none(), "still running");
        table.ended(id, said);
        let due = table.take_due(dir.path()).unwrap();
        assert!(
            due.line
                .starts_with("check the build\n\n`make` exit status: 3"),
            "{}",
            due.line
        );
        assert!(due.line.contains("built"), "{}", due.line);
    }
}
