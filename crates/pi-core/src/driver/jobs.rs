//! `jobs`: what runs apart from the turn that started it — a subagent sent
//! to the background, a script tool that detached — and what comes back from
//! it to its checkout: each result a turn of its own, each input a line read
//! as if typed, whose turn's answer goes back to the job.
//!
//! What comes back goes only when the lane is free and nothing typed is
//! waiting. Nothing here outlives this process; past it, use cron.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tool::{Ctx, Tier, Told, Tool, ToolError, ToolOutput};

// Screen lines a job may have waiting before the oldest give way.
const MAX_NOTICES: usize = 200;
// What a job may have waiting at once; past it they are counted, not kept,
// so a job that outruns its lane cannot grow without bound.
const MAX_WAITING: usize = 100;

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
    running: bool,
    // What it said that is not yet delivered, oldest first. A job leaves the
    // table once it has stopped running and these are gone.
    waiting: VecDeque<Back>,
    // What arrived while `waiting` was full.
    dropped: usize,
    // Where the turns its inputs opened are told of; None if it never listens.
    told: Option<UnboundedSender<Told>>,
    // It asked to stop its checkout's running turn, not yet acted on.
    interrupt: bool,
    // Lines for the screen, not yet shown.
    notices: VecDeque<String>,
    // Asks it to wind down: a subagent the way esc does, a process by signal.
    stop: CancellationToken,
}

impl Item {
    fn due(&self, text: &str, spent: llm::stream::Usage) -> Due {
        let (id, name) = (self.id, &self.description);
        let first = text.lines().next().unwrap_or("").trim();
        Due {
            line: format!("Background job #{id} ({name}) reports:\n\n{text}"),
            note: format!(
                "This turn is a result of background job #{id} ({name}), which you \
                 started; the user did not just type it."
            ),
            label: format!("{name} #{id} {first}"),
            spent,
        }
    }
}

impl Item {
    fn wait(&mut self, back: Back) {
        if self.waiting.len() >= MAX_WAITING {
            self.dropped += 1;
        } else {
            self.waiting.push_back(back);
        }
    }
}

// A job that has stopped with nothing left to say leaves.
fn settle(items: &mut Vec<Item>, at: usize) {
    if !items[at].running && items[at].waiting.is_empty() {
        items.remove(at);
    }
}

/// A job as a surface lists it.
pub struct Job {
    pub id: u64,
    pub description: String,
    pub progress: String,
    pub started: Instant,
    /// Done, its results waiting for the lane to be free.
    pub ended: bool,
}

/// What comes back from a job: a result, or a person speaking through it,
/// their words untouched; `note` says only what the cap dropped, if anything.
pub enum Back {
    Result(Due),
    Input { text: String, note: String },
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
    /// Signalled whenever a job starts, moves, reports, ends or goes, so a
    /// surface drawing them can look again.
    pub fn changed(&self) -> &Notify {
        &self.changed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Now, if a job for `root` has something waiting.
    pub fn next_due(&self, root: &Path) -> Option<Instant> {
        self.lock()
            .items
            .iter()
            .any(|i| i.root == root && !i.waiting.is_empty())
            .then(Instant::now)
    }

    /// The oldest thing waiting for `root`, taken, with the job it came from.
    pub fn take_back(&self, root: &Path) -> Option<(u64, Back)> {
        let mut inner = self.lock();
        let at = inner
            .items
            .iter()
            .position(|i| i.root == root && !i.waiting.is_empty())?;
        let item = &mut inner.items[at];
        let id = item.id;
        let mut back = item.waiting.pop_front()?;
        if item.waiting.is_empty() && item.dropped > 0 {
            let dropped = format!(
                "({} later lines were dropped: more than {MAX_WAITING} waited at once.)",
                std::mem::take(&mut item.dropped)
            );
            match &mut back {
                Back::Result(due) => due.line.push_str(&format!("\n\n{dropped}")),
                Back::Input { note, .. } => *note = dropped,
            }
        }
        settle(&mut inner.items, at);
        Some((id, back))
    }

    /// Checkouts whose running turn a job asked to stop since last asked.
    pub fn take_interrupts(&self) -> Vec<PathBuf> {
        self.lock()
            .items
            .iter_mut()
            .filter_map(|i| std::mem::take(&mut i.interrupt).then(|| i.root.clone()))
            .collect()
    }

    /// The screen lines `root`'s jobs left since last asked, oldest first.
    pub fn take_notices(&self, root: &Path) -> Vec<String> {
        self.lock()
            .items
            .iter_mut()
            .filter(|i| i.root == root)
            .flat_map(|i| std::mem::take(&mut i.notices))
            .collect()
    }

    /// Tell job `id` about a turn its input opened, if it is still here.
    pub fn tell(&self, id: u64, told: Told) {
        if let Some(to) = self
            .lock()
            .items
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| i.told.as_ref())
        {
            let _ = to.send(told);
        }
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
                ended: !i.running,
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
            .any(|i| i.root == root && i.running)
    }

    /// Stop `id` of `root`'s and forget it, waiting results and all.
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

    fn add(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<UnboundedSender<Told>>,
    ) -> u64 {
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.items.push(Item {
            id,
            root,
            description,
            progress: String::new(),
            started: Instant::now(),
            running: true,
            waiting: VecDeque::new(),
            dropped: 0,
            told,
            interrupt: false,
            notices: VecDeque::new(),
            stop,
        });
        drop(inner);
        self.changed.notify_one();
        id
    }

    // Change job `id`, if it is still here, and wake whoever draws it.
    fn update(&self, id: u64, change: impl FnOnce(&mut Item)) {
        let mut inner = self.lock();
        if let Some(at) = inner.items.iter().position(|i| i.id == id) {
            change(&mut inner.items[at]);
            settle(&mut inner.items, at);
        }
        drop(inner);
        self.changed.notify_one();
    }
}

/// The `jobs` tool, and where a call that outlives its turn is kept.
pub struct Jobs {
    table: Arc<Table>,
}

impl Jobs {
    pub const NAME: &'static str = "jobs";

    pub fn new(table: Arc<Table>) -> Self {
        Self { table }
    }
}

impl tool::JobSink for Jobs {
    fn start(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<UnboundedSender<Told>>,
    ) -> Arc<dyn tool::JobHandle> {
        let id = self.table.add(root, description, stop, told);
        Arc::new(Handle {
            id,
            table: self.table.clone(),
        })
    }
}

// One detached call's line back into the table.
struct Handle {
    id: u64,
    table: Arc<Table>,
}

// The last holder gone ends the job, so one whose task died before saying
// `end` — a panic — does not stay listed as running for good.
impl Drop for Handle {
    fn drop(&mut self) {
        self.table.update(self.id, |i| i.running = false);
    }
}

impl tool::JobHandle for Handle {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self, text: String) {
        self.table.update(self.id, |i| i.progress = text);
    }

    fn result(&self, text: String, spent: llm::stream::Usage) {
        self.table.update(self.id, |i| {
            let due = i.due(&text, spent);
            i.wait(Back::Result(due));
        });
    }

    fn input(&self, text: String) {
        self.table.update(self.id, |i| {
            i.wait(Back::Input {
                text,
                note: String::new(),
            })
        });
    }

    fn notice(&self, text: String) {
        self.table.update(self.id, |i| {
            if i.notices.len() >= MAX_NOTICES {
                i.notices.pop_front();
            }
            i.notices.push_back(text);
        });
    }

    fn interrupt(&self) {
        self.table.update(self.id, |i| i.interrupt = true);
    }

    fn end(&self) {
        self.table.update(self.id, |i| i.running = false);
    }
}

#[async_trait]
impl Tool for Jobs {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "List what runs in the background for this checkout — subagents sent \
         off with `background`, tools that went on after their call returned — \
         or `stop` one by id. A stopped job's results never come back."
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

    fn start(table: &Arc<Table>, root: &Path, name: &str) -> Arc<dyn tool::JobHandle> {
        tool::JobSink::start(
            &Jobs::new(table.clone()),
            root.to_path_buf(),
            name.into(),
            CancellationToken::new(),
            None,
        )
    }

    fn result_of(table: &Table, root: &Path) -> Option<Due> {
        match table.take_back(root)?.1 {
            Back::Result(due) => Some(due),
            Back::Input { .. } => panic!("an input, not a result"),
        }
    }

    // A person's line and a result keep the order they were said in, and the
    // turn the line opens is told of through the job's own channel.
    #[tokio::test]
    async fn an_input_waits_in_line_and_its_job_hears_of_its_turn() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let job = tool::JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "wechat".into(),
            CancellationToken::new(),
            Some(tx),
        );
        job.result("connected".into(), Default::default());
        job.input("why did the tests fail".into());
        assert!(matches!(table.take_back(&root), Some((_, Back::Result(_)))));
        let Some((id, Back::Input { text, note })) = table.take_back(&root) else {
            panic!("the input comes next")
        };
        assert_eq!((id, text.as_str()), (job.id(), "why did the tests fail"));
        assert!(note.is_empty(), "a person's line is read as typed: {note}");
        table.tell(id, Told::Started);
        table.tell(id, Told::Reply("parse.rs:40".into()));
        assert_eq!(rx.try_recv(), Ok(Told::Started));
        assert_eq!(rx.try_recv(), Ok(Told::Reply("parse.rs:40".into())));
    }

    // What was lost to the cap is said in the note: a person's words are
    // never added to.
    #[tokio::test]
    async fn the_cap_is_noted_beside_a_persons_line_not_in_it() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = start(&table, &root, "wechat");
        for n in 0..MAX_WAITING + 2 {
            job.input(format!("line {n}"));
        }
        let mut last = None;
        while let Some((_, back)) = table.take_back(&root) {
            last = Some(back);
        }
        let Some(Back::Input { text, note }) = last else {
            panic!("an input")
        };
        assert_eq!(text, format!("line {}", MAX_WAITING - 1));
        assert!(note.contains("2 later lines were dropped"), "{note}");
        assert!(!text.contains("dropped"), "{text}");
    }

    #[tokio::test]
    async fn a_notice_is_shown_once_on_its_own_checkout() {
        let table = Arc::new(Table::default());
        let job = start(&table, Path::new("/checkout"), "wechat");
        job.notice("scan this".into());
        assert!(table.take_notices(Path::new("/elsewhere")).is_empty());
        assert_eq!(table.take_notices(Path::new("/checkout")), ["scan this"]);
        assert!(table.take_notices(Path::new("/checkout")).is_empty());
    }

    #[tokio::test]
    async fn an_interrupt_names_the_jobs_checkout_once() {
        let table = Arc::new(Table::default());
        let job = start(&table, Path::new("/checkout"), "wechat");
        job.interrupt();
        job.interrupt();
        assert_eq!(table.take_interrupts(), [PathBuf::from("/checkout")]);
        assert!(table.take_interrupts().is_empty());
    }

    // What a background subagent spent rides with its answer, owed to the session.
    #[tokio::test]
    async fn a_result_carries_what_it_spent() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = start(&table, &root, "find callers");
        assert!(table.working(&root));
        let spent = llm::stream::Usage {
            input: 100,
            output: 20,
            ..Default::default()
        };
        job.result("three callers".into(), spent);
        job.end();
        let due = result_of(&table, &root).unwrap();
        assert!(due.line.contains("three callers"), "{}", due.line);
        assert_eq!(due.label, "find callers #1 three callers");
        assert_eq!((due.spent.input, due.spent.output), (100, 20));
        assert!(result_of(&table, &root).is_none(), "delivered once");
        assert!(table.jobs(&root).is_empty());
    }

    #[tokio::test]
    async fn stopping_a_job_asks_it_to_wind_down_and_forgets_it() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let stop = CancellationToken::new();
        let job = tool::JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "long job".into(),
            stop.clone(),
            None,
        );
        assert!(table.stop(Path::new("/elsewhere"), job.id()).is_err());
        table.stop(&root, job.id()).unwrap();
        assert!(stop.is_cancelled());
        job.result("too late".into(), Default::default());
        job.end();
        assert!(
            result_of(&table, &root).is_none(),
            "a stopped job says nothing more"
        );
        assert!(table.jobs(&root).is_empty());
    }

    // A crawler's batches come back one turn each, in order, and the job
    // stays listed until the last is delivered, even after it has exited.
    #[tokio::test]
    async fn a_detached_job_reports_many_results_in_order() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = tool::JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "crawl".into(),
            CancellationToken::new(),
            None,
        );
        job.status("1/2 pages".into());
        job.result("page one".into(), Default::default());
        job.result("page two".into(), Default::default());
        job.end();
        assert_eq!(table.jobs(&root)[0].progress, "1/2 pages");
        assert!(table.jobs(&root)[0].ended);
        let first = result_of(&table, &root).unwrap();
        assert!(first.line.ends_with("page one"), "{}", first.line);
        assert_eq!(table.jobs(&root).len(), 1, "one result still waits");
        assert!(result_of(&table, &root).unwrap().line.ends_with("page two"));
        assert!(table.jobs(&root).is_empty());
    }

    // A job that outruns its lane keeps what fits and says what it lost.
    #[tokio::test]
    async fn results_past_the_cap_are_counted_not_kept() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = tool::JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "flood".into(),
            CancellationToken::new(),
            None,
        );
        for n in 0..MAX_WAITING + 3 {
            job.result(n.to_string(), Default::default());
        }
        let mut last = None;
        while let Some(due) = result_of(&table, &root) {
            last = Some(due.line);
        }
        let last = last.unwrap();
        assert!(last.contains(&format!("{}", MAX_WAITING - 1)), "{last}");
        assert!(last.contains("3 later lines were dropped"), "{last}");
    }

    #[tokio::test]
    async fn a_job_whose_task_died_without_ending_leaves() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = start(&table, &root, "crashy");
        let _ = tokio::spawn(async move {
            let _job = job;
            panic!("died before end");
        })
        .await;
        assert!(table.jobs(&root).is_empty());
    }

    #[tokio::test]
    async fn a_job_that_ends_with_nothing_to_say_leaves() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = tool::JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "quiet".into(),
            Default::default(),
            None,
        );
        job.end();
        assert!(table.jobs(&root).is_empty());
        assert!(result_of(&table, &root).is_none());
    }

    #[tokio::test]
    async fn a_removed_checkout_stops_its_jobs() {
        let table = Arc::new(Table::default());
        let a = table.add(
            "/repo.worktrees/a".into(),
            "a".into(),
            Default::default(),
            None,
        );
        table.add("/repo".into(), "main".into(), Default::default(), None);
        let stop = table.lock().items[0].stop.clone();
        table.drop_under(Path::new("/repo.worktrees/a"));
        assert!(stop.is_cancelled(), "#{a} was asked to stop");
        assert!(table.jobs(Path::new("/repo.worktrees/a")).is_empty());
        assert_eq!(table.jobs(Path::new("/repo")).len(), 1);
    }
}
