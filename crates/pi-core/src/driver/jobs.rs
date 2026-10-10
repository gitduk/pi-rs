//! `jobs`: what runs apart from the turn that started it — today a subagent
//! sent to the background — and comes back to its checkout when it ends.
//!
//! Like `later`, its answer goes only when the lane is free and nothing typed
//! is waiting. Nothing here outlives this process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// Every job of this process.
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
    description: String,
    progress: String,
    started: Instant,
    // Its answer, once it has one; delivered when the lane is free.
    ended: Option<subagent::Ran>,
    // Asks it to wind down the way esc does, grace and all.
    stop: CancellationToken,
}

/// A job as a surface lists it.
pub struct Job {
    pub id: u64,
    pub description: String,
    pub progress: String,
    pub started: Instant,
    /// Done, its answer waiting for the lane to be free.
    pub ended: bool,
}

/// What a lane is sent when something it left comes back: the line to
/// submit and the note before it.
pub struct Due {
    pub line: String,
    pub note: String,
    /// What the screen names the turn by, since nobody typed it.
    pub label: String,
    /// What a background subagent spent getting here, owed to the session.
    pub spent: llm::stream::Usage,
}

impl Table {
    /// Signalled whenever a job starts, moves, ends or goes, so a surface
    /// drawing them can look again.
    pub fn changed(&self) -> &Notify {
        &self.changed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Now, if a job for `root` has an answer waiting.
    pub fn next_due(&self, root: &Path) -> Option<Instant> {
        self.lock()
            .items
            .iter()
            .any(|i| i.root == root && i.ended.is_some())
            .then(Instant::now)
    }

    /// The first finished job for `root`, taken.
    pub fn take_due(&self, root: &Path) -> Option<Due> {
        let mut inner = self.lock();
        let at = inner
            .items
            .iter()
            .position(|i| i.root == root && i.ended.is_some())?;
        let item = inner.items.remove(at);
        drop(inner);
        let ran = item.ended.unwrap_or_else(|| subagent::Ran {
            answer: String::new(),
            spent: Default::default(),
        });
        let first = ran.answer.lines().next().unwrap_or("").trim();
        Some(Due {
            line: format!(
                "Background subagent #{} ({}) has finished:\n\n{}",
                item.id, item.description, ran.answer
            ),
            note: format!(
                "This turn is the answer of background subagent #{}, which you started; \
                 the user did not just type it.",
                item.id
            ),
            label: format!("subagent #{} {first}", item.id),
            spent: ran.spent,
        })
    }

    /// The jobs for `root`, oldest first.
    pub fn jobs(&self, root: &Path) -> Vec<Job> {
        self.lock()
            .items
            .iter()
            .filter(|i| i.root == root)
            .map(|i| Job {
                id: i.id,
                description: i.description.clone(),
                progress: i.progress.clone(),
                started: i.started,
                ended: i.ended.is_some(),
            })
            .collect()
    }

    /// The jobs for `root`, one line each.
    pub fn listing(&self, root: &Path) -> Vec<String> {
        self.jobs(root)
            .into_iter()
            .map(|j| {
                let state = if j.ended {
                    "done, back when idle".to_string()
                } else if j.progress.is_empty() {
                    "running".to_string()
                } else {
                    format!("running, {}", j.progress)
                };
                format!("#{}  {state}: {}", j.id, j.description)
            })
            .collect()
    }

    /// Whether a job for `root` is still running, so a surface keeps
    /// redrawing its clock.
    pub fn working(&self, root: &Path) -> bool {
        self.lock()
            .items
            .iter()
            .any(|i| i.root == root && i.ended.is_none())
    }

    /// Stop `id` of `root`'s and forget it, answer and all.
    pub fn stop(&self, root: &Path, id: u64) -> Result<String, String> {
        let mut inner = self.lock();
        let at = inner
            .items
            .iter()
            .position(|i| i.id == id && i.root == root)
            .ok_or_else(|| format!("no job #{id} here"))?;
        let item = inner.items.remove(at);
        item.stop.cancel();
        drop(inner);
        self.changed.notify_one();
        Ok(format!("stopped #{id}: {}", item.description))
    }

    /// Stop and forget every job of checkouts at or under `root`: the
    /// checkout is gone, and a new one there is not it.
    pub fn drop_under(&self, root: &Path) {
        let mut inner = self.lock();
        inner.items.retain(|i| {
            let gone = i.root.starts_with(root);
            if gone {
                i.stop.cancel();
            }
            !gone
        });
        drop(inner);
        self.changed.notify_one();
    }

    fn add(&self, root: PathBuf, description: String, stop: CancellationToken) -> u64 {
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.items.push(Item {
            id,
            root,
            description,
            progress: String::new(),
            started: Instant::now(),
            ended: None,
            stop,
        });
        drop(inner);
        self.changed.notify_one();
        id
    }

    // Change job `id`, if it is still here, and wake whoever draws it.
    fn update(&self, id: u64, change: impl FnOnce(&mut Item)) {
        if let Some(item) = self.lock().items.iter_mut().find(|i| i.id == id) {
            change(item);
        }
        self.changed.notify_one();
    }
}

/// The `jobs` tool, and where a subagent's `background` sends it.
pub struct Jobs {
    table: Arc<Table>,
}

impl Jobs {
    pub const NAME: &'static str = "jobs";

    pub fn new(table: Arc<Table>) -> Self {
        Self { table }
    }
}

impl subagent::Background for Jobs {
    fn start(&self, root: PathBuf, description: String, job: subagent::Job) -> u64 {
        let stop = CancellationToken::new();
        let id = self.table.add(root, description, stop.clone());
        let progress: tool::Progress = {
            let table = self.table.clone();
            Arc::new(move |said| table.update(id, |i| i.progress = said))
        };
        let run = job(progress, stop);
        let table = self.table.clone();
        tokio::spawn(async move {
            let ran = run.await;
            table.update(id, |i| i.ended = Some(ran));
        });
        id
    }
}

#[async_trait]
impl Tool for Jobs {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "List what runs in the background for this checkout — subagents sent \
         off with `background` — or `stop` one by id. A stopped job's answer \
         never comes back."
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "stop": { "type": "integer", "description": "The id of one to stop." },
            },
        })
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let root = ctx.workspace.root();
        if let Some(id) = args.get("stop").and_then(Value::as_u64) {
            return self
                .table
                .stop(root, id)
                .map(ToolOutput::text)
                .map_err(ToolError::Invalid);
        }
        let jobs = self.table.listing(root);
        Ok(ToolOutput::text(if jobs.is_empty() {
            "nothing in the background".to_string()
        } else {
            jobs.join("\n")
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subagent::Background as _;

    #[tokio::test]
    async fn a_background_subagent_comes_back_once_with_what_it_spent() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (go, gate) = tokio::sync::oneshot::channel::<()>();
        let id = Jobs::new(table.clone()).start(
            root.clone(),
            "find callers".into(),
            Box::new(|progress, _stop| {
                Box::pin(async move {
                    progress("turn 2".into());
                    let _ = gate.await;
                    subagent::Ran {
                        answer: "three callers".into(),
                        spent: llm::stream::Usage {
                            input: 100,
                            output: 20,
                            ..Default::default()
                        },
                    }
                })
            }),
        );
        tokio::task::yield_now().await;
        let job = &table.jobs(&root)[0];
        assert_eq!(
            (job.id, job.progress.as_str(), job.ended),
            (id, "turn 2", false)
        );
        assert!(table.working(&root));
        assert!(table.take_due(&root).is_none(), "still running");

        go.send(()).unwrap();
        while table.working(&root) {
            tokio::task::yield_now().await;
        }
        let due = table.take_due(&root).unwrap();
        assert!(due.line.contains("three callers"), "{}", due.line);
        assert_eq!((due.spent.input, due.spent.output), (100, 20));
        assert!(table.take_due(&root).is_none(), "delivered once");
        assert!(table.jobs(&root).is_empty());
    }

    #[tokio::test]
    async fn stopping_a_background_subagent_asks_it_to_wind_down() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (seen, heard) = tokio::sync::oneshot::channel();
        let id = Jobs::new(table.clone()).start(
            root.clone(),
            "long job".into(),
            Box::new(|_, stop| {
                Box::pin(async move {
                    stop.cancelled().await;
                    let _ = seen.send(());
                    subagent::Ran {
                        answer: String::new(),
                        spent: Default::default(),
                    }
                })
            }),
        );
        assert!(table.stop(Path::new("/elsewhere"), id).is_err());
        table.stop(&root, id).unwrap();
        heard.await.expect("the job heard the stop");
        assert!(!table.working(&root));
        assert!(table.jobs(&root).is_empty());
    }

    #[tokio::test]
    async fn a_removed_checkout_stops_its_jobs() {
        let table = Arc::new(Table::default());
        let a = table.add("/repo.worktrees/a".into(), "a".into(), Default::default());
        table.add("/repo".into(), "main".into(), Default::default());
        let stop = table.lock().items[0].stop.clone();
        table.drop_under(Path::new("/repo.worktrees/a"));
        assert!(stop.is_cancelled(), "#{a} was asked to stop");
        assert!(table.jobs(Path::new("/repo.worktrees/a")).is_empty());
        assert_eq!(table.jobs(Path::new("/repo")).len(), 1);
    }
}
