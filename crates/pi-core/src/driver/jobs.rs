//! `job`: what runs apart from the turn that started it — a subagent sent
//! to the background, a script tool that detached — and what comes back from
//! it to its checkout: each result a turn of its own, each input a line read
//! as if typed, whose turn's answer goes back to the job.
//!
//! What comes back goes only when the lane is free and nothing typed is
//! waiting. Nothing here outlives this process; past it, use cron.
//!
//! The questions such calls put to the person stand here too, jobs' and
//! those of calls still in their turn alike: numbered, up until answered
//! from the screen or through a job that follows, and withdrawn once their
//! asker is gone.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};
use toolbox::background::{Answer, Ask, Asked, JobHandle, JobSink, Peek, Rerun, Told};

use crate::core::question::{Asker, Question};

// Screen lines a job may have waiting before the oldest give way.
const MAX_NOTICES: usize = 200;
// How long a restart waits for the last run to let go before giving up.
const LET_GO: std::time::Duration = std::time::Duration::from_secs(10);
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
    next_question: u64,
    // Every question up and not yet answered, by checkout, oldest first.
    standing: Vec<(PathBuf, Question)>,
    // Where the answer to each question asked with `back` goes.
    answers: HashMap<u64, oneshot::Sender<Option<String>>>,
}

impl Inner {
    // Questions whose asker stopped waiting: void, shown or not.
    fn sweep(&mut self) {
        let gone: Vec<u64> = (self.answers.iter())
            .filter(|(_, to)| to.is_closed())
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            self.answers.remove(&id);
            self.close(id, "its asker stopped waiting");
        }
    }

    // Put `question` up at `root`. Asking again is a change of mind: only an
    // asker's latest stands, where its last one stood. One an asker waits
    // on is never replaced: it would wait for good.
    fn put(&mut self, root: PathBuf, question: Question) {
        let told = Told::Question {
            id: question.id,
            asked: question.asked(),
        };
        let stale = (self.standing.iter()).position(|(at, q)| {
            *at == root && q.from == question.from && !q.back && !question.back
        });
        match stale {
            Some(at) => {
                let old = std::mem::replace(&mut self.standing[at].1, question).id;
                tracing::info!(target: "pi::job", question = old, why = "asked again", "closed");
                self.tell_followers(&root, Told::Closed(old));
            }
            None => self.standing.push((root.clone(), question)),
        }
        self.tell_followers(&root, told);
    }

    // Take question `id` down, if it is still up, and say so to every job
    // that follows its checkout.
    fn close(&mut self, id: u64, why: &str) -> Option<Question> {
        let at = self.standing.iter().position(|(_, q)| q.id == id)?;
        let (root, question) = self.standing.remove(at);
        tracing::info!(target: "pi::job", question = id, why, "closed");
        self.tell_followers(&root, Told::Closed(id));
        Some(question)
    }

    // Take question `id` down as answered: the pick goes to an asker that
    // waits on it. What comes back is the pick still to go out as typed.
    fn answered(&mut self, id: u64, why: &str, picked: Option<String>) -> Option<String> {
        let question = self.close(id, why)?;
        match self.answers.remove(&id) {
            Some(to) => {
                let _ = to.send(picked);
                None
            }
            None => picked.filter(|_| !question.back),
        }
    }

    // Take down what job `id` asked: it is gone, or no longer the run that asked.
    fn withdraw(&mut self, id: u64, why: &str) {
        let asked: Vec<u64> = (self.standing.iter())
            .filter(|(_, q)| matches!(q.from, Asker::Job { id: of, .. } if of == id))
            .map(|(_, q)| q.id)
            .collect();
        for question in asked {
            self.close(question, why);
        }
    }

    // The running jobs on `root` told of every turn there.
    fn followers<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a Item> {
        (self.items.iter()).filter(move |i| i.root == root && i.running() && i.listens.follows())
    }

    fn tell_followers(&self, root: &Path, told: Told) {
        for to in self.followers(root).filter_map(|i| i.listens.to()) {
            let _ = to.send(told.clone());
        }
    }

    // Where run `run` of job `id` stands in the table, if it is still here.
    fn mine(&self, id: u64, run: u64) -> Option<usize> {
        (self.items.iter()).position(|i| i.id == id && i.run == run)
    }

    // `question` numbered, so an answer or a withdrawal can name it.
    fn number(&mut self, mut question: Question) -> Question {
        self.next_question += 1;
        question.id = self.next_question;
        question
    }
}

struct Item {
    id: u64,
    // The checkout it comes back to: one lane holds one.
    root: PathBuf,
    description: String,
    progress: String,
    started: Instant,
    // The same moment on the wall clock, for a surface to say when.
    started_at: SystemTime,
    phase: Phase,
    // What it said that is not yet delivered, oldest first. A job leaves the
    // table once it has stopped running and these are gone.
    waiting: VecDeque<Back>,
    // What arrived while `waiting` was full.
    dropped: usize,
    listens: Listens,
    // It asked to stop its checkout's running turn, not yet acted on.
    interrupt: bool,
    // Lines for the screen, not yet shown.
    notices: VecDeque<String>,
    // Reads the latest of what it printed; None for one that keeps none.
    output: Option<Peek>,
    // How it runs again. One that has it is kept once it ends, to be
    // started over; one without leaves.
    rerun: Option<Rerun>,
    // Which run of it this is: a handle from an earlier one is not heard.
    run: u64,
    // Cancelled once this run's task has let go of what it held.
    gone: CancellationToken,
}

// What a job is told of, and where.
enum Listens {
    Never,
    // The turns its inputs opened.
    Inputs(UnboundedSender<Told>),
    // Every turn on its checkout, not just its inputs': it asked to follow.
    Turns(UnboundedSender<Told>),
}

impl Listens {
    fn to(&self) -> Option<&UnboundedSender<Told>> {
        match self {
            Listens::Never => None,
            Listens::Inputs(to) | Listens::Turns(to) => Some(to),
        }
    }

    fn follows(&self) -> bool {
        matches!(self, Listens::Turns(_))
    }
}

// Where a job's latest run stands.
enum Phase {
    // `stop` asks it to wind down: a subagent as a stopped run is, a process
    // by signal.
    Running { stop: CancellationToken },
    // Over: the user stopped it, or it ended by itself.
    Over { stopped: bool },
}

impl Item {
    fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    // Stopped, with nothing left for the lane or the screen, and no way to
    // start over: nothing here is anyone's any more.
    fn spent(&self) -> bool {
        !self.running()
            && self.waiting.is_empty()
            && self.notices.is_empty()
            && self.rerun.is_none()
    }

    fn state(&self) -> State {
        match self.phase {
            Phase::Running { .. } => State::Running,
            Phase::Over { .. } if !self.waiting.is_empty() => State::Back,
            Phase::Over { stopped: true } => State::Stopped,
            Phase::Over { stopped: false } => State::Exited,
        }
    }

    // Nothing of the last run is left to show or deliver.
    fn clear_run(&mut self) {
        self.progress.clear();
        self.waiting.clear();
        self.dropped = 0;
        self.notices.clear();
    }

    // The job as a person reads it: `judge #3`.
    fn name(&self) -> String {
        format!("{} #{}", self.description, self.id)
    }

    fn due(&self, text: &str) -> Due {
        let (id, name) = (self.id, &self.description);
        let first = text.lines().next().unwrap_or("").trim();
        Due {
            line: format!("Background job #{id} ({name}) reports:\n\n{text}"),
            note: format!(
                "This turn is a result of background job #{id} ({name}), which you \
                 started; the user did not just type it."
            ),
            label: format!("{} {first}", self.name()),
            text: text.to_string(),
        }
    }
}

impl Item {
    // What it has to show: what it printed, or what it sends back for one
    // that keeps no output. Read outside the table's lock.
    fn said(&self) -> Peek {
        if let Some(peek) = &self.output {
            return peek.clone();
        }
        let texts: Vec<&str> = (self.waiting.iter())
            .map(|back| match back {
                Back::Result(due) => due.text.as_str(),
                Back::Input { text, .. } => text.as_str(),
            })
            .collect();
        let text = texts.join("\n");
        Arc::new(move || text.clone())
    }

    fn wait(&mut self, back: Back) {
        if self.waiting.len() >= MAX_WAITING {
            tracing::warn!(target: "pi::job", job = self.id, "over the waiting cap, dropped");
            self.dropped += 1;
        } else {
            self.waiting.push_back(back);
        }
    }
}

// `act` on each of `ids`: why each failed, and those done as `#id (what)`.
fn each(ids: &[u64], act: impl Fn(u64) -> Result<String, String>) -> (Vec<String>, Vec<String>) {
    let (mut failed, mut done) = (Vec::new(), Vec::new());
    for &id in ids {
        match act(id) {
            Ok(what) => done.push(format!("#{id} ({what})")),
            Err(why) => failed.push(why),
        }
    }
    (failed, done)
}

// A job that has stopped with nothing left to say leaves, unless it can
// be started over.
fn settle(items: &mut Vec<Item>, at: usize) {
    if items[at].spent() {
        items.remove(at);
    }
}

/// A job as a surface lists it.
#[derive(Clone)]
pub struct Job {
    pub id: u64,
    pub description: String,
    pub progress: String,
    pub started: Instant,
    /// When it started, by the wall clock.
    pub started_at: SystemTime,
    pub state: State,
    /// The last line of what it printed, or of what it sends back for one
    /// that keeps no output; empty when nothing yet.
    pub last: String,
}

/// Where a listed job stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    /// Done, its results waiting for the lane to be free.
    Back,
    /// Stopped by the user, kept to be started over.
    Stopped,
    /// Ended by itself, its results delivered, kept to be started over.
    Exited,
}

impl State {
    /// Running or with something to deliver: one only kept to be started
    /// over holds nothing up and needs no one's eye.
    pub fn busy(self) -> bool {
        match self {
            State::Running | State::Back => true,
            State::Stopped | State::Exited => false,
        }
    }

    /// Whether stopping it stops anything: a finished one's results only
    /// wait for the lane, and stopping it would throw them away.
    pub fn stoppable(self) -> bool {
        self == State::Running
    }
}

/// What comes back from a job: a result, or a person speaking through it,
/// their words untouched; `note` says only what the cap dropped, if anything.
/// Their words are a prompt, unless `as_typed`: a pick of offered options,
/// read as if typed, so one that is a command runs.
pub enum Back {
    Result(Due),
    Input {
        text: String,
        note: String,
        as_typed: bool,
    },
}

/// What a lane is sent when something it left comes back: the line to
/// submit and the note before it.
pub struct Due {
    pub line: String,
    pub note: String,
    /// What the screen names the turn by, since nobody typed it.
    pub label: String,
    /// What the job said, as it said it.
    pub text: String,
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
        let kind = match back {
            Back::Result(_) => "result",
            Back::Input { .. } => "input",
        };
        tracing::info!(target: "pi::job", job = id, kind, "delivered");
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
        self.take(root, |i| std::mem::take(&mut i.notices))
    }

    /// The questions up at `root`, oldest first.
    pub fn questions(&self, root: &Path) -> Vec<Question> {
        let mut inner = self.lock();
        inner.sweep();
        (inner.standing.iter())
            .filter(|(at, _)| at == root)
            .map(|(_, q)| q.clone())
            .collect()
    }

    /// The screen answered question `id`: it comes down, and the pick goes
    /// to an asker that waits on it — `None` when they closed it. One that
    /// does not wait hears nothing: the screen sent the pick as typed.
    pub fn answer(&self, id: u64, picked: Option<String>) {
        let mut inner = self.lock();
        inner.answered(id, "answered on the screen", picked);
        drop(inner);
        self.changed.notify_one();
    }

    // What `root`'s jobs kept for the surface, taken; a job it was all that
    // held leaves with it.
    fn take<T, I: IntoIterator<Item = T>>(
        &self,
        root: &Path,
        from: impl FnMut(&mut Item) -> I,
    ) -> Vec<T> {
        let mut inner = self.lock();
        let taken = (inner.items.iter_mut())
            .filter(|i| i.root == root)
            .flat_map(from)
            .collect();
        inner.items.retain(|i| !i.spent());
        taken
    }

    /// Tell job `id` about a turn its input opened, if it is still here.
    pub fn tell(&self, id: u64, told: Told) {
        match &told {
            Told::Started => tracing::info!(target: "pi::job", job = id, "told started"),
            Told::Reply(text) => tracing::info!(
                target: "pi::job", job = id, chars = text.chars().count(), reply = %text, "told reply"
            ),
            Told::Asked => tracing::info!(target: "pi::job", job = id, "told asked"),
            Told::Answer(answer) => {
                tracing::info!(target: "pi::job", job = id, ?answer, "told answer")
            }
            // Said where they happen: a question up or down, an answer refused.
            Told::Question { .. } | Told::Closed(_) | Told::Refused { .. } => {}
        }
        if let Some(to) = self
            .lock()
            .items
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| i.listens.to())
        {
            let _ = to.send(told);
        }
    }

    /// The running jobs on `root` told of every turn there.
    pub fn followers(&self, root: &Path) -> Vec<u64> {
        (self.lock().followers(root)).map(|i| i.id).collect()
    }

    /// The jobs for `root`, oldest first.
    pub fn jobs(&self, root: &Path) -> Vec<Job> {
        let held: Vec<(Job, Peek)> = (self.lock().items.iter())
            .filter(|i| i.root == root)
            .map(|i| {
                let job = Job {
                    id: i.id,
                    description: i.description.clone(),
                    progress: i.progress.clone(),
                    started: i.started,
                    started_at: i.started_at,
                    state: i.state(),
                    last: String::new(),
                };
                (job, i.said())
            })
            .collect();
        (held.into_iter())
            .map(|(job, said)| Job {
                last: pi_store::text::last_line(&said()),
                ..job
            })
            .collect()
    }

    /// What job `id` of `root`'s has to show under its row; None once it
    /// has left.
    pub fn body(&self, root: &Path, id: u64) -> Option<String> {
        let said = {
            let inner = self.lock();
            let item = inner.items.iter().find(|i| i.id == id && i.root == root)?;
            item.said()
        };
        Some(said())
    }

    /// What job `id` of `root`'s said that is not back yet, oldest first;
    /// None once it has left.
    pub fn waiting(&self, root: &Path, id: u64) -> Option<Vec<String>> {
        let inner = self.lock();
        let item = inner.items.iter().find(|i| i.id == id && i.root == root)?;
        Some(
            item.waiting
                .iter()
                .map(|back| match back {
                    Back::Result(due) => due.text.clone(),
                    Back::Input { text, .. } => text.clone(),
                })
                .collect(),
        )
    }

    /// The latest of what job `id` of `root`'s printed.
    pub fn output(&self, root: &Path, id: u64) -> Result<String, String> {
        let peek = {
            let inner = self.lock();
            let item = (inner.items.iter())
                .find(|i| i.id == id && i.root == root)
                .ok_or_else(|| format!("no job #{id} here"))?;
            item.output.clone().ok_or_else(|| {
                format!("#{id} keeps no output to show; what it says comes back as turns")
            })?
        };
        let printed = peek();
        Ok(match printed.is_empty() {
            true => "nothing printed yet".into(),
            false => printed,
        })
    }

    /// The jobs for `root`, one line each.
    pub fn listing(&self, root: &Path) -> Vec<String> {
        self.jobs(root)
            .into_iter()
            .map(|j| {
                let state = match j.state {
                    State::Running if j.progress.is_empty() => "running".to_string(),
                    State::Running => format!("running, {}", j.progress),
                    State::Back => "done, back when idle".to_string(),
                    State::Stopped => "stopped, can restart".to_string(),
                    State::Exited => "exited, can restart".to_string(),
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
            .any(|i| i.root == root && i.running())
    }

    /// Stop `id` of `root`'s, waiting results and all: what it was. One
    /// that can be started over is kept, to be until this process ends; any
    /// other is forgotten.
    pub fn stop(&self, root: &Path, id: u64) -> Result<String, String> {
        let mut inner = self.lock();
        let at = inner
            .items
            .iter()
            .position(|i| i.id == id && i.root == root)
            .ok_or_else(|| format!("no job #{id} here"))?;
        let item = &mut inner.items[at];
        let what = item.description.clone();
        match (&item.phase, item.rerun.is_some()) {
            (Phase::Over { .. }, true) => return Err(format!("#{id} ({what}) is not running")),
            (Phase::Running { stop }, true) => {
                stop.cancel();
                item.phase = Phase::Over { stopped: true };
                // What the stopped run still says is no one's to hear.
                item.run += 1;
                item.clear_run();
                tracing::info!(target: "pi::job", job = id, "stopped");
            }
            (phase, false) => {
                if let Phase::Running { stop } = phase {
                    stop.cancel();
                }
                inner.items.remove(at);
                tracing::info!(target: "pi::job", job = id, "stopped and forgotten");
            }
        }
        inner.withdraw(id, "its job stopped");
        drop(inner);
        self.changed.notify_one();
        Ok(what)
    }

    /// Start `id` of `root`'s over under the same id, stopping it first if
    /// it still runs: what it is. What the last run left unsaid is dropped.
    pub fn restart(self: &Arc<Self>, root: &Path, id: u64) -> Result<String, String> {
        let mut inner = self.lock();
        let item = (inner.items.iter_mut())
            .find(|i| i.id == id && i.root == root)
            .ok_or_else(|| format!("no job #{id} here"))?;
        let Some(rerun) = item.rerun.clone() else {
            return Err(format!(
                "#{id} ({}) cannot be started over",
                item.description
            ));
        };
        if let Phase::Running { stop } = &item.phase {
            stop.cancel();
        }
        let stop = CancellationToken::new();
        let gone = std::mem::replace(&mut item.gone, CancellationToken::new());
        item.phase = Phase::Running { stop: stop.clone() };
        item.run += 1;
        item.clear_run();
        item.output = None;
        item.started = Instant::now();
        item.started_at = SystemTime::now();
        let handle: Arc<dyn JobHandle> = Arc::new(Handle {
            id,
            run: item.run,
            gone: item.gone.clone(),
            table: self.clone(),
        });
        let what = item.description.clone();
        inner.withdraw(id, "its job started over");
        drop(inner);
        tracing::info!(target: "pi::job", job = id, "restarting");
        self.changed.notify_one();
        tokio::spawn(async move {
            // What the last run held, a port say, is let go before the next.
            if tokio::time::timeout(LET_GO, gone.cancelled())
                .await
                .is_err()
            {
                let secs = LET_GO.as_secs();
                let why = format!("could not start again: the last run held on past {secs}s");
                handle.result(why);
                handle.end();
                return;
            }
            // Stopped or restarted again meanwhile: this run never starts.
            if stop.is_cancelled() {
                return;
            }
            if let Err(e) = rerun(handle.clone(), stop) {
                handle.result(format!("could not start again: {e}"));
                handle.end();
            }
        });
        Ok(what)
    }

    /// Start each of `ids` of `root`'s over: a line for each not restarted,
    /// and those restarted, as `#id (what)`.
    pub fn restart_each(self: &Arc<Self>, root: &Path, ids: &[u64]) -> (Vec<String>, Vec<String>) {
        each(ids, |id| self.restart(root, id))
    }

    /// Whether a job for `root` is running or has something to deliver:
    /// one only kept to be started over holds nothing up.
    pub fn busy(&self, root: &Path) -> bool {
        (self.lock().items.iter())
            .filter(|i| i.root == root)
            .any(|i| i.state().busy())
    }

    /// Stop each of `ids` of `root`'s, or every one still running there when
    /// none are named: a line for each not stopped, and those stopped, as
    /// `#id (what)`.
    pub fn stop_each(&self, root: &Path, ids: &[u64]) -> (Vec<String>, Vec<String>) {
        let ids = if ids.is_empty() {
            (self.jobs(root).iter())
                .filter(|j| j.state.stoppable())
                .map(|j| j.id)
                .collect()
        } else {
            ids.to_vec()
        };
        if ids.is_empty() {
            return (
                vec!["nothing running in the background to stop".into()],
                Vec::new(),
            );
        }
        each(&ids, |id| self.stop(root, id))
    }

    /// Every job of `from` comes back to `to` from now on: the conversation
    /// it reports to moved there. It goes on running where it started.
    pub fn rehome(&self, from: &Path, to: &Path) {
        let mut inner = self.lock();
        for item in inner.items.iter_mut().filter(|i| i.root == from) {
            tracing::info!(target: "pi::job", job = item.id, to = %to.display(), "rehomed");
            item.root = to.to_path_buf();
        }
        for (at, _) in inner.standing.iter_mut().filter(|(at, _)| at == from) {
            *at = to.to_path_buf();
        }
        drop(inner);
        self.changed.notify_one();
    }

    /// Stop and forget every job of checkouts at or under `root`, saying how
    /// many went: the checkout is gone, and a new one there is not it.
    pub fn drop_under(&self, root: &Path) -> usize {
        let mut inner = self.lock();
        let had = inner.items.len();
        inner.items.retain(|i| {
            let gone = i.root.starts_with(root);
            if gone {
                tracing::info!(target: "pi::job", job = i.id, "stopped with its checkout");
                if let Phase::Running { stop } = &i.phase {
                    stop.cancel();
                }
            }
            !gone
        });
        // No one is left there to follow them, nor to show them; one that
        // waits hears none, and goes on.
        let (gone, kept) = std::mem::take(&mut inner.standing)
            .into_iter()
            .partition(|(at, _)| at.starts_with(root));
        inner.standing = kept;
        for (_, q) in gone {
            inner.answers.remove(&q.id);
        }
        let gone = had - inner.items.len();
        drop(inner);
        self.changed.notify_one();
        gone
    }

    fn add(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<UnboundedSender<Told>>,
    ) -> (u64, CancellationToken) {
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        let gone = CancellationToken::new();
        tracing::info!(
            target: "pi::job", job = id, name = %description, root = %root.display(), "started"
        );
        inner.items.push(Item {
            id,
            root,
            description,
            progress: String::new(),
            started: Instant::now(),
            started_at: SystemTime::now(),
            phase: Phase::Running { stop },
            waiting: VecDeque::new(),
            dropped: 0,
            listens: told.map_or(Listens::Never, Listens::Inputs),
            interrupt: false,
            notices: VecDeque::new(),
            output: None,
            rerun: None,
            run: 0,
            gone: gone.clone(),
        });
        drop(inner);
        self.changed.notify_one();
        (id, gone)
    }

    // Change run `run` of job `id`, if it is still here, and wake whoever
    // draws it.
    fn update(&self, id: u64, run: u64, change: impl FnOnce(&mut Item)) {
        let mut inner = self.lock();
        if let Some(at) = (inner.items.iter()).position(|i| i.id == id && i.run == run) {
            change(&mut inner.items[at]);
            settle(&mut inner.items, at);
        }
        drop(inner);
        self.changed.notify_one();
    }
}

/// A job's id as a person or the model writes it: `3` or `#3`.
pub fn parse_id(word: &str) -> Option<u64> {
    word.trim_start_matches('#').parse().ok()
}

/// The `job` tool, and where a call that outlives its turn is kept.
pub struct Jobs {
    table: Arc<Table>,
}

impl Jobs {
    pub const NAME: &'static str = "job";

    pub fn new(table: Arc<Table>) -> Self {
        Self { table }
    }
}

impl Ask for Jobs {
    fn ask(&self, root: PathBuf, asker: &str, asked: Asked) -> Result<Option<Answer>, String> {
        let from = Asker::Tool {
            name: asker.to_string(),
        };
        let question = Question::of(from, &asked)?;
        let mut inner = self.table.lock();
        let question = inner.number(question);
        tracing::info!(
            target: "pi::job", tool = asker, question = question.id, back = asked.back,
            text = %question.text, options = ?question.options, "ask"
        );
        let answer = asked.back.then(|| {
            let (to, answer) = oneshot::channel();
            inner.answers.insert(question.id, to);
            answer
        });
        inner.put(root, question);
        drop(inner);
        self.table.changed.notify_one();
        Ok(answer)
    }
}

impl JobSink for Jobs {
    fn start(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<UnboundedSender<Told>>,
    ) -> Arc<dyn JobHandle> {
        let (id, gone) = self.table.add(root, description, stop, told);
        Arc::new(Handle {
            id,
            run: 0,
            gone,
            table: self.table.clone(),
        })
    }
}

// One run of a detached call's line back into the table.
struct Handle {
    id: u64,
    run: u64,
    gone: CancellationToken,
    table: Arc<Table>,
}

// The last holder gone ends the job, so one whose task died before saying
// `end` — a panic — does not stay listed as running for good.
impl Drop for Handle {
    fn drop(&mut self) {
        self.table.update(self.id, self.run, |i| {
            if i.running() {
                tracing::warn!(target: "pi::job", job = i.id, "its task went without ending");
                i.phase = Phase::Over { stopped: false };
            }
        });
        self.gone.cancel();
    }
}

impl JobHandle for Handle {
    fn id(&self) -> u64 {
        self.id
    }

    fn status(&self, text: String) {
        tracing::debug!(target: "pi::job", job = self.id, status = %text, "status");
        self.table.update(self.id, self.run, |i| i.progress = text);
    }

    fn result(&self, text: String) {
        tracing::info!(target: "pi::job", job = self.id, text = %text, "result");
        self.table.update(self.id, self.run, |i| {
            let due = i.due(&text);
            i.wait(Back::Result(due));
        });
    }

    // Writing instead of picking answers every question that does not wait:
    // what they wrote is the answer.
    fn input(&self, text: String) {
        tracing::info!(target: "pi::job", job = self.id, text = %text, "input");
        let mut inner = self.table.lock();
        let Some(at) = inner.mine(self.id, self.run) else {
            return;
        };
        let root = inner.items[at].root.clone();
        let lapsed: Vec<u64> = (inner.standing.iter())
            .filter(|(r, q)| *r == root && !q.back)
            .map(|(_, q)| q.id)
            .collect();
        for id in lapsed {
            inner.close(id, "the person wrote instead");
        }
        inner.items[at].wait(Back::Input {
            text,
            note: String::new(),
            as_typed: false,
        });
        drop(inner);
        self.table.changed.notify_one();
    }

    fn notice(&self, text: String) {
        tracing::info!(target: "pi::job", job = self.id, text = %text, "notice");
        self.table.update(self.id, self.run, |i| {
            if i.notices.len() >= MAX_NOTICES {
                i.notices.pop_front();
            }
            i.notices.push_back(text);
        });
    }

    fn choose(&self, asked: Asked) {
        let mut inner = self.table.lock();
        let Some(at) = inner.mine(self.id, self.run) else {
            return;
        };
        let item = &inner.items[at];
        let from = Asker::Job {
            id: item.id,
            name: item.name(),
        };
        let question = match Question::of(from, &asked) {
            Ok(q) => inner.number(q),
            Err(why) => {
                tracing::warn!(target: "pi::job", job = self.id, %why, "a question dropped");
                return;
            }
        };
        tracing::info!(
            target: "pi::job", job = self.id, question = question.id, text = %question.text,
            options = ?question.options, "choose"
        );
        let root = inner.items[at].root.clone();
        inner.put(root, question);
        drop(inner);
        self.table.changed.notify_one();
    }

    fn answer(&self, question: u64, picks: Vec<usize>) {
        let mut inner = self.table.lock();
        inner.sweep();
        let Some(at) = inner.mine(self.id, self.run) else {
            return;
        };
        let root = &inner.items[at].root;
        let picked = (inner.standing.iter())
            .find(|(r, q)| r == root && q.id == question)
            .ok_or_else(|| "it is no longer up".to_string())
            .and_then(|(_, q)| q.picked(&picks));
        let picked = match picked {
            Ok(picked) => picked,
            Err(why) => {
                tracing::warn!(
                    target: "pi::job", job = self.id, question, ?picks, %why, "an answer refused"
                );
                if let Some(to) = inner.items[at].listens.to() {
                    let _ = to.send(Told::Refused { question, why });
                }
                return;
            }
        };
        tracing::info!(target: "pi::job", job = self.id, question, ?picks, "answer");
        if let Some(text) = inner.answered(question, "answered through a job", picked) {
            inner.items[at].wait(Back::Input {
                text,
                note: String::new(),
                as_typed: true,
            });
        }
        drop(inner);
        self.table.changed.notify_one();
    }

    fn interrupt(&self) {
        tracing::info!(target: "pi::job", job = self.id, "interrupt");
        self.table.update(self.id, self.run, |i| i.interrupt = true);
    }

    fn watch(&self, output: Peek) {
        self.table
            .update(self.id, self.run, |i| i.output = Some(output));
    }

    // What is already up is told too, as if just asked.
    fn follow(&self) {
        tracing::info!(target: "pi::job", job = self.id, "follows");
        let mut inner = self.table.lock();
        let Some(at) = inner.mine(self.id, self.run) else {
            return;
        };
        let item = &mut inner.items[at];
        // One that never listens has nowhere to be told.
        let (Listens::Inputs(to) | Listens::Turns(to)) =
            std::mem::replace(&mut item.listens, Listens::Never)
        else {
            return;
        };
        item.listens = Listens::Turns(to.clone());
        let root = item.root.clone();
        for (_, q) in inner.standing.iter().filter(|(at, _)| *at == root) {
            let _ = to.send(Told::Question {
                id: q.id,
                asked: q.asked(),
            });
        }
        drop(inner);
        self.table.changed.notify_one();
    }

    fn rerun(&self, how: Rerun) {
        self.table
            .update(self.id, self.run, |i| i.rerun = Some(how));
    }

    fn end(&self) {
        tracing::info!(target: "pi::job", job = self.id, "ended");
        self.table.update(self.id, self.run, |i| {
            i.phase = Phase::Over { stopped: false }
        });
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
         show the latest a command printed with `output`, `stop` some by id, \
         or `stop_all`. A stopped job's results never come back. `restart` \
         runs a background command again under its id, stopped first if it \
         still runs; a stopped or exited one stays listed for that."
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "stop": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "The ids of those to stop.",
                },
                "stop_all": { "type": "boolean", "description": "Stop every one still running." },
                "restart": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "The ids of those to start over.",
                },
                "output": {
                    "type": "integer",
                    "description": "The id of one whose latest output to show, as it runs.",
                },
            },
        })
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let root = ctx.workspace.root();
        // A lone id, as the field once took, still names one; so does `"#3"`.
        let id = |v: &Value| {
            v.as_u64()
                .or_else(|| parse_id(v.as_str()?))
                .ok_or_else(|| ToolError::Invalid(format!("`{v}` is not a job id")))
        };
        let ids_of = |key: &str| -> Result<Vec<u64>, ToolError> {
            match args.get(key) {
                Some(Value::Array(ids)) => ids.iter().map(id).collect(),
                Some(Value::Null) | None => Ok(Vec::new()),
                Some(v) => Ok(vec![id(v)?]),
            }
        };
        let ids = ids_of("stop")?;
        let restart = ids_of("restart")?;
        let all = args.get("stop_all").and_then(Value::as_bool) == Some(true);
        if !restart.is_empty() && (all || !ids.is_empty()) {
            return Err(ToolError::Invalid(
                "stop or restart in one call, not both".into(),
            ));
        }
        let named = |verb: &str, ids: &[u64]| {
            let ids: Vec<String> = ids.iter().map(|id| format!("#{id}")).collect();
            format!("{verb} {}", ids.join(", "))
        };
        // What was done, then each that was not; an error when none was.
        let reply = |asked: String, verb: &str, (failed, done): (Vec<String>, Vec<String>)| {
            if done.is_empty() {
                return Err(ToolError::Invalid(failed.join("\n")));
            }
            let said = std::iter::once(format!("{verb} {}", done.join(", ")));
            Ok(
                ToolOutput::text(said.chain(failed).collect::<Vec<_>>().join("\n"))
                    .with_preview(asked),
            )
        };
        if !restart.is_empty() {
            let asked = named("restart", &restart);
            return reply(asked, "restarted", self.table.restart_each(root, &restart));
        }
        if all || !ids.is_empty() {
            let asked = match all {
                true => "stop all".to_string(),
                false => named("stop", &ids),
            };
            return reply(
                asked,
                "stopped",
                self.table.stop_each(root, if all { &[] } else { &ids }),
            );
        }
        if let Some(v) = args.get("output").filter(|v| !v.is_null()) {
            let id = id(v)?;
            return (self.table.output(root, id))
                .map(|out| ToolOutput::text(out).with_preview(named("output", &[id])))
                .map_err(ToolError::Invalid);
        }
        let jobs = self.table.listing(root);
        let out = match jobs.is_empty() {
            true => "nothing in the background".to_string(),
            false => jobs.join("\n"),
        };
        Ok(ToolOutput::text(out).with_preview("list"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(table: &Arc<Table>, root: &Path, name: &str) -> Arc<dyn JobHandle> {
        JobSink::start(
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
        let job = JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "wechat".into(),
            CancellationToken::new(),
            Some(tx),
        );
        job.result("connected".into());
        job.input("why did the tests fail".into());
        assert!(matches!(table.take_back(&root), Some((_, Back::Result(_)))));
        let Some((id, Back::Input { text, note, .. })) = table.take_back(&root) else {
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
        let Some(Back::Input { text, note, .. }) = last else {
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

    fn asked(question: &str, options: &[&str], back: bool) -> Asked {
        Asked {
            question: question.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            back,
        }
    }

    // Said and gone: what a job leaves for the screen outlives its exit.
    #[tokio::test]
    async fn a_question_outlives_its_job_and_a_newer_one_replaces_it() {
        let table = Arc::new(Table::default());
        let job = start(&table, Path::new("/checkout"), "judge");
        job.choose(asked("a new task?", &["/new"], false));
        job.choose(asked("a long one?", &["/compact", "go on"], false));
        job.choose(asked(
            "hidden?",
            &["/compact\nand what was not shown"],
            false,
        ));
        job.notice("judged".into());
        job.end();
        assert!(table.questions(Path::new("/elsewhere")).is_empty());
        let up = table.questions(Path::new("/checkout"));
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].text, "a long one?");
        assert_eq!(up[0].options, ["/compact", "go on"]);
        assert!(
            matches!(&up[0].from, Asker::Job { name, .. } if name.starts_with("judge #")),
            "{:?}",
            up[0].from
        );
        assert_eq!(table.take_notices(Path::new("/checkout")), ["judged"]);
        assert!(
            table.jobs(Path::new("/checkout")).is_empty(),
            "spent, so gone"
        );
        assert_eq!(
            table.questions(Path::new("/checkout")),
            up,
            "ending is not withdrawing"
        );
    }

    // One asker, one question: asking again replaces it where it stood, so
    // the front stays in front; one an asker waits on is never replaced.
    #[tokio::test]
    async fn asking_again_replaces_unless_the_asker_waits() {
        let table = Arc::new(Table::default());
        let jobs = Jobs::new(table.clone());
        let root = PathBuf::from("/checkout");
        let job = start(&table, &root, "judge");
        job.choose(asked("first?", &["/new"], false));
        jobs.ask(root.clone(), "choose", asked("which?", &["a"], false))
            .unwrap();
        job.choose(asked("second?", &["/new"], false));
        let texts = |table: &Table| -> Vec<String> {
            (table.questions(&root).into_iter())
                .map(|q| q.text)
                .collect()
        };
        assert_eq!(texts(&table), ["second?", "which?"]);
        let _waits = (0..2)
            .map(|_| jobs.ask(root.clone(), "guard", asked("allow?", &["yes"], true)))
            .collect::<Vec<_>>();
        assert_eq!(texts(&table), ["second?", "which?", "allow?", "allow?"]);
    }

    #[tokio::test]
    async fn stopping_a_job_withdraws_what_it_asked() {
        let table = Arc::new(Table::default());
        let job = start(&table, Path::new("/checkout"), "judge");
        job.choose(asked("new?", &["/new"], false));
        assert_eq!(table.questions(Path::new("/checkout")).len(), 1);
        table.stop(Path::new("/checkout"), job.id()).unwrap();
        assert!(table.questions(Path::new("/checkout")).is_empty());
    }

    // A call in its turn asks: the answer reaches it, once; an asker that
    // stopped waiting has its question withdrawn, never answered.
    #[tokio::test]
    async fn an_answer_goes_back_to_the_call_that_waits_on_it() {
        let table = Arc::new(Table::default());
        let jobs = Jobs::new(table.clone());
        let root = PathBuf::from("/checkout");
        let mut answer = jobs
            .ask(
                root.clone(),
                "guard",
                asked("run rm -rf?", &["allow", "deny"], true),
            )
            .unwrap()
            .expect("it waits");
        let fire = jobs
            .ask(root.clone(), "choose", asked("which?", &["a"], false))
            .unwrap();
        assert!(
            fire.is_none(),
            "one that goes out as typed waits on nothing"
        );
        let up = table.questions(&root);
        assert_eq!(up.len(), 2);
        assert!(up[0].back && !up[1].back);
        assert_eq!(
            up[0].from,
            Asker::Tool {
                name: "guard".into()
            }
        );
        assert_ne!(up[0].id, up[1].id);
        table.answer(up[0].id, Some("deny".into()));
        assert_eq!(answer.try_recv(), Ok(Some("deny".into())));
        table.answer(up[0].id, Some("allow".into()));
        table.answer(up[1].id, None);
        assert!(table.questions(&root).is_empty(), "answered, so down");

        let gone = jobs.ask(root.clone(), "guard", asked("again?", &["yes"], true));
        drop(gone);
        assert!(
            table.questions(&root).is_empty(),
            "withdrawn before it was ever shown"
        );
        assert!(
            jobs.ask(root, "guard", asked("?", &[], true)).is_err(),
            "one that cannot be put says why"
        );
    }

    // A job that follows is told each question up on its checkout — even one
    // up before it followed — and when it comes down; its pick goes out as
    // typed, while writing instead lets every question that does not wait lapse.
    #[tokio::test]
    async fn a_follower_hears_each_question_and_answers_it() {
        let table = Arc::new(Table::default());
        let jobs = Jobs::new(table.clone());
        let root = PathBuf::from("/checkout");
        jobs.ask(
            root.clone(),
            "choose",
            asked("which?", &["/compact", "go on"], false),
        )
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = JobSink::start(
            &jobs,
            root.clone(),
            "telegram".into(),
            CancellationToken::new(),
            Some(tx),
        );
        bridge.follow();
        let first = table.questions(&root)[0].id;
        assert!(matches!(rx.try_recv(), Ok(Told::Question { id, .. }) if id == first));

        for picks in [vec![5], vec![0, 1], vec![0, 0]] {
            bridge.answer(first, picks.clone());
            assert!(
                matches!(rx.try_recv(), Ok(Told::Refused { question, .. }) if question == first),
                "{picks:?} is told back why not"
            );
        }
        assert_eq!(table.questions(&root).len(), 1, "no such answer, still up");
        bridge.answer(first, vec![0]);
        assert_eq!(rx.try_recv(), Ok(Told::Closed(first)));
        let Some((_, Back::Input { text, as_typed, .. })) = table.take_back(&root) else {
            panic!("the pick went nowhere");
        };
        assert_eq!((text.as_str(), as_typed), ("/compact", true));
        bridge.answer(first, vec![1]);
        assert!(table.take_back(&root).is_none(), "answered once");
        assert!(
            matches!(rx.try_recv(), Ok(Told::Refused { .. })),
            "and told so"
        );

        let mut waits = jobs
            .ask(root.clone(), "guard", asked("allow?", &["yes", "no"], true))
            .unwrap()
            .expect("it waits");
        jobs.ask(root.clone(), "choose", asked("next?", &["a"], false))
            .unwrap();
        let up = table.questions(&root);
        for q in &up {
            assert!(matches!(rx.try_recv(), Ok(Told::Question { id, .. }) if id == q.id));
        }
        bridge.input("neither".into());
        assert_eq!(rx.try_recv(), Ok(Told::Closed(up[1].id)), "it lapsed");
        assert_eq!(table.questions(&root).len(), 1, "one that waits stays");
        bridge.answer(up[0].id, vec![1]);
        assert_eq!(waits.try_recv(), Ok(Some("no".into())));
        assert_eq!(rx.try_recv(), Ok(Told::Closed(up[0].id)));
        let Some((_, Back::Input { text, as_typed, .. })) = table.take_back(&root) else {
            panic!("their words went nowhere");
        };
        assert_eq!((text.as_str(), as_typed), ("neither", false));
        assert!(
            table.take_back(&root).is_none(),
            "a pick that goes back is not typed"
        );
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

    #[tokio::test]
    async fn a_result_is_delivered_once() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = start(&table, &root, "find callers");
        assert!(table.working(&root));
        job.result("three callers".into());
        job.end();
        let due = result_of(&table, &root).unwrap();
        assert!(due.line.contains("three callers"), "{}", due.line);
        assert_eq!(due.label, "find callers #1 three callers");
        assert!(result_of(&table, &root).is_none(), "delivered once");
        assert!(table.jobs(&root).is_empty());
    }

    #[tokio::test]
    async fn stopping_a_job_asks_it_to_wind_down_and_forgets_it() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let stop = CancellationToken::new();
        let job = JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "long job".into(),
            stop.clone(),
            None,
        );
        assert!(table.stop(Path::new("/elsewhere"), job.id()).is_err());
        table.stop(&root, job.id()).unwrap();
        assert!(stop.is_cancelled());
        job.result("too late".into());
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
        let job = JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "crawl".into(),
            CancellationToken::new(),
            None,
        );
        job.status("1/2 pages".into());
        job.result("page one".into());
        job.result("page two".into());
        job.end();
        assert_eq!(
            table.waiting(&root, job.id()).unwrap(),
            ["page one", "page two"]
        );
        assert!(table.waiting(Path::new("/elsewhere"), job.id()).is_none());
        assert_eq!(table.jobs(&root)[0].progress, "1/2 pages");
        assert_eq!(table.jobs(&root)[0].state, State::Back);
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
        let job = JobSink::start(
            &Jobs::new(table.clone()),
            root.clone(),
            "flood".into(),
            CancellationToken::new(),
            None,
        );
        for n in 0..MAX_WAITING + 3 {
            job.result(n.to_string());
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
        let job = JobSink::start(
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
        let (a, _) = table.add(
            "/repo.worktrees/a".into(),
            "a".into(),
            Default::default(),
            None,
        );
        table.add("/repo".into(), "main".into(), Default::default(), None);
        let stop = match &table.lock().items[0].phase {
            Phase::Running { stop } => stop.clone(),
            Phase::Over { .. } => panic!("it runs"),
        };
        table.drop_under(Path::new("/repo.worktrees/a"));
        assert!(stop.is_cancelled(), "#{a} was asked to stop");
        assert!(table.jobs(Path::new("/repo.worktrees/a")).is_empty());
        assert_eq!(table.jobs(Path::new("/repo")).len(), 1);
    }

    type Reruns = tokio::sync::mpsc::UnboundedReceiver<(Arc<dyn JobHandle>, CancellationToken)>;

    // A job that can run again: each rerun hands back the handle and stop
    // it was given, as a launch would keep them.
    fn rerunnable(table: &Arc<Table>, root: &Path) -> (Arc<dyn JobHandle>, Reruns) {
        let job = start(table, root, "serve");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        job.rerun(Arc::new(move |job, stop| {
            let _ = tx.send((job, stop));
            Ok(())
        }));
        (job, rx)
    }

    // Stopped, it stays listed; restarted, it runs again under its id once
    // the last run has let go, and that run's late words are not heard.
    #[tokio::test]
    async fn a_restarted_job_waits_for_the_last_run_and_keeps_its_id() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (old, mut reruns) = rerunnable(&table, &root);
        let id = old.id();

        table.stop(&root, id).unwrap();
        old.result("said as it went".into());
        assert_eq!(table.jobs(&root)[0].state, State::Stopped);

        table.restart(&root, id).unwrap();
        assert_eq!(table.jobs(&root)[0].state, State::Running);
        tokio::task::yield_now().await;
        assert!(reruns.try_recv().is_err(), "the last run still holds on");

        old.result("late".into());
        drop(old);
        let (new, stop) = reruns.recv().await.expect("run again");
        assert_eq!(new.id(), id);
        assert!(table.waiting(&root, id).unwrap().is_empty(), "{id}");
        assert_eq!(table.jobs(&root)[0].state, State::Running);

        table.restart(&root, id).unwrap();
        assert!(stop.is_cancelled(), "a running one is stopped first");
        new.result("exited 1".into());
        new.end();
        assert_eq!(table.jobs(&root)[0].state, State::Running);
    }

    // One that ended by itself is kept to start over; a stop leaves it be.
    #[tokio::test]
    async fn an_exited_job_is_kept_to_start_over() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (job, _reruns) = rerunnable(&table, &root);
        let id = job.id();
        job.result("exited 1".into());
        job.end();
        assert!(result_of(&table, &root).is_some());
        assert_eq!(table.jobs(&root)[0].state, State::Exited);
        assert!(table.stop(&root, id).is_err());
        assert_eq!(table.jobs(&root)[0].state, State::Exited);
    }

    // Stopped while waiting for the last run to let go: nothing starts.
    #[tokio::test]
    async fn a_restart_stopped_before_it_starts_never_runs() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (old, mut reruns) = rerunnable(&table, &root);
        let id = old.id();
        table.restart(&root, id).unwrap();
        table.stop(&root, id).unwrap();
        drop(old);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(reruns.try_recv().is_err(), "it was stopped first");
        assert_eq!(table.jobs(&root)[0].state, State::Stopped);
    }

    #[tokio::test]
    async fn only_a_job_that_says_how_can_restart() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let job = start(&table, &root, "find callers");
        let why = table.restart(&root, job.id()).unwrap_err();
        assert!(why.contains("cannot be started over"), "{why}");
        assert!(table.restart(&root, 99).is_err());
    }
}
