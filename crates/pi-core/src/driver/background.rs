//! Background shell commands, their session channels, and queued results.
//!
//! Results reach the owning checkout between model requests. Inputs open their
//! own turns and receive replies through the command's channel. Commands stop
//! with this process; saved launch specifications allow restarting them later.
//!
//! The table also owns foreground questions and routes answers from the user
//! or from commands that follow the checkout.

use std::collections::{HashMap, HashSet, VecDeque};
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
use tool::{Ctx, ToolError, ToolOutput};
use toolbox::background::{Again, Controls, Peek, Rerun, RunHandle, RunSink};
use toolbox::channel::{Answer, Ask, Asked, Told};

use crate::core::question::{Asker, Question};

// How long a restart waits for the last run to let go before giving up.
const LET_GO: std::time::Duration = std::time::Duration::from_secs(10);
// Bound queued messages even when a command outruns its checkout.
const MAX_WAITING: usize = 100;
pub(super) const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// Every execution of this process.
#[derive(Default)]
pub struct Table {
    inner: Mutex<Inner>,
    changed: Notify,
    // Every call waiting on an execution, woken together: `changed` wakes one.
    moved: Notify,
}

#[derive(Default)]
struct Inner {
    progress_at: Option<Instant>,
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
                tracing::info!(target: "pi::background", question = old, why = "asked again", "closed");
                self.tell_followers(&root, Told::Closed(old));
            }
            None => self.standing.push((root.clone(), question)),
        }
        self.tell_followers(&root, told);
    }

    // Take question `id` down, if it is still up, and say so to every execution
    // that follows its checkout.
    fn close(&mut self, id: u64, why: &str) -> Option<Question> {
        let at = self.standing.iter().position(|(_, q)| q.id == id)?;
        let (root, question) = self.standing.remove(at);
        tracing::info!(target: "pi::background", question = id, why, "closed");
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

    fn withdraw(&mut self, id: u64, why: &str) {
        let asked: Vec<u64> = (self.standing.iter())
            .filter(|(_, q)| matches!(q.from, Asker::Background { id: of, .. } if of == id))
            .map(|(_, q)| q.id)
            .collect();
        for question in asked {
            self.close(question, why);
        }
    }

    // The running executions on `root` told of every turn there.
    fn followers<'a>(&'a self, root: &'a Path) -> impl Iterator<Item = &'a Item> {
        (self.items.iter()).filter(move |i| i.root == root && i.running() && i.listens.follows())
    }

    fn tell_followers(&self, root: &Path, told: Told) {
        for to in self.followers(root).filter_map(|i| i.listens.to()) {
            let _ = to.send(told.clone());
        }
    }

    // Where run `run` of execution `id` stands in the table, if it is still here.
    fn mine(&self, id: u64, run: u64) -> Option<usize> {
        (self.items.iter()).position(|i| i.id == id && i.run == run)
    }

    fn back_at(&self, root: &Path, results_only: bool) -> Option<(usize, usize)> {
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.root == root)
            .find_map(|(at, item)| {
                item.waiting
                    .iter()
                    .position(|back| !results_only || matches!(back, Back::Result(_)))
                    .map(|back| (at, back))
            })
    }

    // One queued message, with the same bookkeeping for results and inputs.
    fn take_back(&mut self, at: usize, back: usize) -> Option<(u64, Back)> {
        let item = &mut self.items[at];
        let id = item.id;
        let mut back = item.waiting.remove(back)?;
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
        settle(&mut self.items, at);
        Some((id, back))
    }

    // `question` numbered, so an answer or a withdrawal can name it.
    fn number(&mut self, mut question: Question) -> Question {
        self.next_question += 1;
        question.id = self.next_question;
        question
    }

    fn stop(&mut self, at: usize, why: &str) -> Result<String, String> {
        let item = &mut self.items[at];
        let id = item.id;
        let what = item.description.clone();
        match (&item.phase, item.rerun.is_some()) {
            (Phase::Over { .. }, true) => return Err(format!("#{id} ({what}) is not running")),
            (Phase::Running { stop }, true) => {
                stop.cancel();
                item.phase = Phase::Over { stopped: true };
                // What the stopped run still says is no one's to hear.
                item.run += 1;
                item.clear_run();
                tracing::info!(target: "pi::background", execution = id, why, "stopped");
            }
            (phase, false) => {
                if let Phase::Running { stop } = phase {
                    stop.cancel();
                }
                self.items.remove(at);
                tracing::info!(target: "pi::background", execution = id, why, "stopped and forgotten");
            }
        }
        self.withdraw(id, "its execution stopped");
        Ok(what)
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
    // What it said that is not yet delivered, oldest first. A command leaves the
    // table once it has stopped running and these are gone.
    waiting: VecDeque<Back>,
    // What arrived while `waiting` was full.
    dropped: usize,
    listens: Listens,
    // It asked to stop its checkout's running turn, not yet acted on.
    interrupt: bool,
    // Reads the latest of what it printed; None for one that keeps none.
    output: Option<Peek>,
    last_output: String,
    // How it runs again, and the same as data for a restart to carry. One
    // that has it is kept once it ends, to be started over; one without leaves.
    rerun: Option<(Rerun, Option<Again>)>,
    // Which run of it this is: a handle from an earlier one is not heard.
    run: u64,
    // Cancelled once this run's task has let go of what it held.
    gone: CancellationToken,
}

// What an execution is told of, and where.
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

// Where a command's latest run stands.
enum Phase {
    // `stop` asks it to wind down: a subagent as a stopped run is, a process
    // by signal.
    Running { stop: CancellationToken },
    // Over: the user stopped it, or it ended by itself.
    Over { stopped: bool },
}

impl Item {
    // Started now, running nothing yet and with nothing to say.
    fn blank(id: u64, root: PathBuf, description: String, phase: Phase) -> Self {
        Item {
            id,
            root,
            description,
            progress: String::new(),
            started: Instant::now(),
            started_at: SystemTime::now(),
            phase,
            waiting: VecDeque::new(),
            dropped: 0,
            listens: Listens::Never,
            interrupt: false,
            output: None,
            last_output: String::new(),
            rerun: None,
            run: 0,
            gone: CancellationToken::new(),
        }
    }

    fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    // Stopped, with nothing left for the lane, and no way to start over:
    // nothing here is anyone's any more.
    fn spent(&self) -> bool {
        !self.running() && self.waiting.is_empty() && self.rerun.is_none()
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
        self.last_output.clear();
        self.progress.clear();
        self.waiting.clear();
        self.dropped = 0;
    }

    // The execution as a person reads it: `judge #3`.
    fn name(&self) -> String {
        format!("{} #{}", self.description, self.id)
    }

    fn due(&self, text: &str) -> Due {
        let (id, name) = (self.id, &self.description);
        let first = text.lines().next().unwrap_or("").trim();
        Due {
            line: format!("Background command #{id} ({name}) reports:\n\n{text}"),
            note: format!(
                "This turn is a result of background command #{id} ({name}), which you \
                 started. The user did not just type it."
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
        let texts: Vec<&str> = (self.waiting.iter()).map(Back::text).collect();
        let text = texts.join("\n");
        Arc::new(move || text.clone())
    }

    fn wait(&mut self, back: Back) {
        if self.waiting.len() >= MAX_WAITING {
            tracing::warn!(target: "pi::background", execution = self.id, "over the waiting cap, dropped");
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

// A command that has stopped with nothing left to say leaves, unless it can
// be started over.
fn settle(items: &mut Vec<Item>, at: usize) {
    if items[at].spent() {
        items.remove(at);
    }
}

/// A command a pi leaves behind that the next pi lists again, under the same
/// id where it is free, stopped, to be started over there.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Carried {
    pub id: u64,
    pub root: PathBuf,
    pub description: String,
    /// Stopped by the user or by pi leaving, not ended by itself.
    pub stopped: bool,
    pub started_at: SystemTime,
    pub again: Again,
}

/// Write `executions` to `path` for the next pi to take over; with none, what an
/// earlier pi left there goes, so it is not listed again later.
pub fn hand_over(path: &Path, executions: &[Carried]) -> anyhow::Result<()> {
    if executions.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    // Command lines can carry secrets, as transcripts can.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    std::io::Write::write_all(&mut file.open(path)?, &serde_json::to_vec(executions)?)?;
    tracing::info!(target: "pi::background", executions = executions.len(), path = %path.display(), "handed over");
    Ok(())
}

/// The executions [`hand_over`] wrote to `path`, taken: the file goes once read.
pub fn take_over(path: &Path) -> anyhow::Result<Vec<Carried>> {
    let read = match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        read => read,
    };
    let _ = std::fs::remove_file(path);
    Ok(serde_json::from_slice(&read?)?)
}

/// A command as a surface lists it.
#[derive(Clone)]
pub struct BackgroundCommand {
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

/// Where a listed execution stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    /// Done, its results waiting to be delivered.
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

/// What comes back from an execution: a result, or a person speaking through it,
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

impl Back {
    /// What the execution said, as it said it.
    pub fn text(&self) -> &str {
        match self {
            Back::Result(due) => &due.text,
            Back::Input { text, .. } => text,
        }
    }
}

/// What a lane is sent when something it left comes back: the line to
/// submit and the note before it.
pub struct Due {
    pub line: String,
    pub note: String,
    /// What the screen names the turn by, since nobody typed it.
    pub label: String,
    /// What the execution said, as it said it.
    pub text: String,
}

impl Table {
    /// Signalled whenever an execution starts, moves, reports, ends or goes, so a
    /// surface drawing them can look again.
    pub fn changed(&self) -> &Notify {
        &self.changed
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake(&self) {
        self.changed.notify_one();
        self.moved.notify_waiters();
    }

    /// Wait, up to `within`, for execution `id` of `root`'s to stop: what it said,
    /// taken so it does not come back as a turn too; None while it runs.
    pub async fn wait(
        &self,
        root: &Path,
        id: u64,
        within: std::time::Duration,
    ) -> Result<Option<Vec<String>>, String> {
        self.wait_each(root, &[id], within)
            .await
            .pop()
            .expect("one execution was waited for")
    }

    /// Wait for all named executions under one deadline, returning each result in
    /// order. Keep results queued until the wait ends so cancellation loses none.
    pub async fn wait_each(
        &self,
        root: &Path,
        ids: &[u64],
        within: std::time::Duration,
    ) -> Vec<Result<Option<Vec<String>>, String>> {
        let deadline = Instant::now() + within;
        loop {
            // Enabled before looking, so an end between the two still wakes it.
            let moved = self.moved.notified();
            tokio::pin!(moved);
            moved.as_mut().enable();
            let running = self
                .lock()
                .items
                .iter()
                .any(|i| i.root == root && ids.contains(&i.id) && i.running());
            if !running {
                break;
            }
            if tokio::time::timeout_at(deadline, moved).await.is_err() {
                break;
            }
        }
        ids.iter().map(|&id| self.take_results(root, id)).collect()
    }

    // What execution `id` of `root`'s said, taken, once it no longer runs; a
    // person's words through it stay in line for the lane.
    fn take_results(&self, root: &Path, id: u64) -> Result<Option<Vec<String>>, String> {
        let mut inner = self.lock();
        let at = (inner.items.iter())
            .position(|i| i.id == id && i.root == root)
            .ok_or_else(|| format!("no command #{id} here"))?;
        let item = &mut inner.items[at];
        match item.state() {
            State::Running => return Ok(None),
            State::Back => {}
            State::Stopped => return Err(format!("#{id} was stopped. It has nothing to say")),
            State::Exited => {
                return Err(format!("#{id} already ended. Its results were delivered"));
            }
        }
        let mut said = Vec::new();
        item.waiting.retain(|back| match back {
            Back::Result(_) => {
                said.push(back.text().to_string());
                false
            }
            Back::Input { .. } => true,
        });
        tracing::info!(target: "pi::background", execution = id, results = said.len(), "results collected");
        settle(&mut inner.items, at);
        drop(inner);
        self.wake();
        Ok(Some(said))
    }

    /// Now, if an execution for `root` has something waiting.
    pub fn next_due(&self, root: &Path) -> Option<Instant> {
        self.lock()
            .items
            .iter()
            .any(|i| i.root == root && !i.waiting.is_empty())
            .then(Instant::now)
    }

    /// Take the next queued message for `root`, combining consecutive results
    /// into one turn. A person's input stays separate, in its original place.
    /// The id names the first result's execution, or the input's sender.
    pub fn take_back(&self, root: &Path) -> Option<(u64, Back)> {
        self.take_next(root, false)
    }

    /// Results for an active run. Person-originated inputs keep their own
    /// turns; they do not hold back results the active run may depend on.
    pub fn take_ready_results(&self, root: &Path) -> Option<Due> {
        match self.take_next(root, true)? {
            (_, Back::Result(due)) => Some(due),
            (_, Back::Input { .. }) => unreachable!("only results were requested"),
        }
    }

    fn take_next(&self, root: &Path, results_only: bool) -> Option<(u64, Back)> {
        let mut inner = self.lock();
        let (at, back) = inner.back_at(root, results_only)?;
        let (id, back) = inner.take_back(at, back)?;
        let mut due = match back {
            Back::Result(due) => due,
            input @ Back::Input { .. } => {
                tracing::info!(target: "pi::background", execution = id, kind = "input", "delivered");
                return Some((id, input));
            }
        };
        let mut executions = vec![id];
        // Hold the lock through the snapshot; later arrivals form another batch.
        while let Some((at, back)) = inner.back_at(root, results_only) {
            match inner.items[at].waiting.get(back) {
                Some(Back::Result(_)) => {}
                Some(Back::Input { .. }) | None => break,
            }
            let Some((execution, Back::Result(next))) = inner.take_back(at, back) else {
                unreachable!("the next queued message was a result")
            };
            due.line.push_str("\n\n");
            due.line.push_str(&next.line);
            due.text.push_str("\n\n");
            due.text.push_str(&next.text);
            executions.push(execution);
        }
        if executions.len() > 1 {
            due.label = format!("{} background command results", executions.len());
            due.note = format!(
                "This turn combines {} queued results from background commands you started. \
                 The user did not type these messages. Handle these results together in this turn.",
                executions.len()
            );
        }
        tracing::info!(target: "pi::background", ?executions, kind = "result", "delivered");
        Some((id, Back::Result(due)))
    }

    /// Checkouts whose running turn an execution asked to stop since last asked.
    pub fn take_interrupts(&self) -> Vec<PathBuf> {
        self.lock()
            .items
            .iter_mut()
            .filter_map(|i| std::mem::take(&mut i.interrupt).then(|| i.root.clone()))
            .collect()
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
        self.wake();
    }

    /// Tell execution `id` about a turn its input opened, if it is still here.
    pub fn tell(&self, id: u64, told: Told) {
        match &told {
            Told::Started => {
                tracing::info!(target: "pi::background", execution = id, "told started")
            }
            Told::Progress(text) => tracing::info!(
                target: "pi::background", execution = id, chars = text.chars().count(), "told progress"
            ),
            Told::Reply(text) => tracing::info!(
                target: "pi::background", execution = id, chars = text.chars().count(), reply = %text, "told reply"
            ),
            Told::Asked => tracing::info!(target: "pi::background", execution = id, "told asked"),
            Told::Answer(answer) => {
                tracing::info!(target: "pi::background", execution = id, ?answer, "told answer")
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

    /// Relay changed output from running executions to their checkout's followers.
    pub fn relay_progress(&self) {
        let watched: Vec<_> = {
            let mut inner = self.lock();
            let now = Instant::now();
            if inner
                .progress_at
                .is_some_and(|at| now.duration_since(at) < PROGRESS_EVERY)
            {
                return;
            }
            inner.progress_at = Some(now);
            let followed: HashSet<_> = inner
                .items
                .iter()
                .filter(|i| i.listens.follows())
                .map(|i| &i.root)
                .collect();
            inner
                .items
                .iter()
                .filter(|i| i.running() && i.listens.to().is_none())
                .filter(|i| followed.contains(&i.root))
                .filter_map(|i| {
                    i.output
                        .clone()
                        .map(|peek| (i.id, i.run, i.root.clone(), peek))
                })
                .collect()
        };
        for (id, run, root, peek) in watched {
            let latest = pi_store::text::last_line(&peek());
            if latest.trim().is_empty() {
                continue;
            }
            let followers = {
                let mut inner = self.lock();
                let Some(at) = inner.mine(id, run) else {
                    continue;
                };
                if inner.items[at].last_output == latest {
                    continue;
                }
                inner.items[at].last_output = latest.clone();
                inner
                    .items
                    .iter()
                    .filter(|i| i.root == root && i.listens.follows())
                    .map(|i| i.id)
                    .collect::<Vec<_>>()
            };
            let text = format!("#{id}: {latest}");
            for follower in followers {
                self.tell(follower, Told::Progress(text.clone()));
            }
        }
    }

    /// The running executions on `root` told of every turn there.
    pub fn followers(&self, root: &Path) -> Vec<u64> {
        (self.lock().followers(root)).map(|i| i.id).collect()
    }

    /// The executions for `root`, oldest first.
    pub fn background(&self, root: &Path) -> Vec<BackgroundCommand> {
        let held: Vec<(BackgroundCommand, Peek)> = (self.lock().items.iter())
            .filter(|i| i.root == root)
            .map(|i| {
                let execution = BackgroundCommand {
                    id: i.id,
                    description: i.description.clone(),
                    progress: i.progress.clone(),
                    started: i.started,
                    started_at: i.started_at,
                    state: i.state(),
                    last: String::new(),
                };
                (execution, i.said())
            })
            .collect();
        (held.into_iter())
            .map(|(execution, said)| BackgroundCommand {
                last: pi_store::text::last_line(&said()),
                ..execution
            })
            .collect()
    }

    /// What execution `id` of `root`'s has to show under its row; None once it
    /// has left.
    pub fn body(&self, root: &Path, id: u64) -> Option<String> {
        let said = {
            let inner = self.lock();
            let item = inner.items.iter().find(|i| i.id == id && i.root == root)?;
            item.said()
        };
        Some(said())
    }

    /// What execution `id` of `root`'s said that is not back yet, oldest first;
    /// None once it has left.
    pub fn waiting(&self, root: &Path, id: u64) -> Option<Vec<String>> {
        let inner = self.lock();
        let item = inner.items.iter().find(|i| i.id == id && i.root == root)?;
        Some(
            item.waiting
                .iter()
                .map(|back| back.text().to_string())
                .collect(),
        )
    }

    /// The latest of what execution `id` of `root`'s printed.
    pub fn output(&self, root: &Path, id: u64) -> Result<String, String> {
        let peek = {
            let inner = self.lock();
            let item = (inner.items.iter())
                .find(|i| i.id == id && i.root == root)
                .ok_or_else(|| format!("no command #{id} here"))?;
            item.output.clone().ok_or_else(|| {
                format!("#{id} keeps no output to show. What it says comes back as turns")
            })?
        };
        let printed = peek();
        Ok(match printed.is_empty() {
            true => "nothing printed yet".into(),
            false => printed,
        })
    }

    /// The executions for `root`, one line each.
    pub fn listing(&self, root: &Path) -> Vec<String> {
        self.background(root)
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

    /// Whether an execution for `root` is still running, so a surface keeps
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
            .ok_or_else(|| format!("no command #{id} here"))?;
        let what = inner.stop(at, "requested")?;
        drop(inner);
        self.wake();
        Ok(what)
    }

    /// Stop only executions still running on `root`, returning their labels. The
    /// caller supplies the cause so it can be kept with the state change.
    pub fn stop_running(&self, root: &Path, why: &str) -> Vec<String> {
        let mut inner = self.lock();
        let mut stopped = Vec::new();
        for at in (0..inner.items.len()).rev() {
            let item = &inner.items[at];
            if item.root == root && item.running() {
                let id = item.id;
                let what = inner.stop(at, why).expect("the execution is running");
                stopped.push(format!("#{id} ({what})"));
            }
        }
        stopped.reverse();
        drop(inner);
        if !stopped.is_empty() {
            self.wake();
        }
        stopped
    }

    /// Start `id` of `root`'s over under the same id, stopping it first if
    /// it still runs: what it is. What the last run left unsaid is dropped.
    pub fn restart(self: &Arc<Self>, root: &Path, id: u64) -> Result<String, String> {
        let mut inner = self.lock();
        let item = (inner.items.iter_mut())
            .find(|i| i.id == id && i.root == root)
            .ok_or_else(|| format!("no command #{id} here"))?;
        let Some((rerun, _)) = item.rerun.clone() else {
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
        let handle: Arc<dyn RunHandle> = Arc::new(Handle {
            id,
            run: item.run,
            gone: item.gone.clone(),
            table: self.clone(),
        });
        let what = item.description.clone();
        inner.withdraw(id, "its execution started over");
        drop(inner);
        tracing::info!(target: "pi::background", execution = id, "restarting");
        self.wake();
        tokio::spawn(async move {
            // What the last run held, a port say, is let go before the next.
            if tokio::time::timeout(LET_GO, gone.cancelled())
                .await
                .is_err()
            {
                let secs = LET_GO.as_secs();
                let why = format!("could not start again: the last run held on past {secs}s");
                handle.channel().result(why);
                handle.end();
                return;
            }
            // Stopped or restarted again meanwhile: this run never starts.
            if stop.is_cancelled() {
                return;
            }
            if let Err(e) = rerun(handle.clone(), stop) {
                handle
                    .channel()
                    .result(format!("could not start again: {e}"));
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

    /// Every execution of `root`'s that says as data how it runs again, for the
    /// next pi; one still running counts as stopped, since leaving stops it.
    pub fn carried(&self, root: &Path) -> Vec<Carried> {
        (self.lock().items.iter())
            .filter(|i| i.root == root)
            .filter_map(|i| {
                let Some((_, Some(again))) = &i.rerun else {
                    return None;
                };
                let stopped = match i.phase {
                    Phase::Running { .. } => true,
                    Phase::Over { stopped } => stopped,
                };
                Some(Carried {
                    id: i.id,
                    root: i.root.clone(),
                    description: i.description.clone(),
                    stopped,
                    started_at: i.started_at,
                    again: again.clone(),
                })
            })
            .collect()
    }

    /// List `execution`, carried from the pi before this one, as it was there,
    /// started over through `rerun`. Ids given out later come after it.
    pub fn revive(&self, mut execution: Carried, rerun: Rerun) {
        let mut inner = self.lock();
        // Lanes reopened from different pis can bring the same id.
        if inner.items.iter().any(|i| i.id == execution.id) {
            inner.next_id += 1;
            execution.id = inner.next_id;
        }
        inner.next_id = inner.next_id.max(execution.id);
        let ago = execution.started_at.elapsed().unwrap_or_default();
        tracing::info!(
            target: "pi::background", execution = execution.id, name = %execution.description,
            root = %execution.root.display(), "carried over"
        );
        let phase = Phase::Over {
            stopped: execution.stopped,
        };
        let item = Item {
            started: Instant::now().checked_sub(ago).unwrap_or_else(Instant::now),
            started_at: execution.started_at,
            rerun: Some((rerun, Some(execution.again))),
            ..Item::blank(execution.id, execution.root, execution.description, phase)
        };
        // No last run here to let go of.
        item.gone.cancel();
        inner.items.push(item);
        drop(inner);
        self.wake();
    }

    /// Whether an execution for `root` is running or has something to deliver:
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
            (self.background(root).iter())
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

    /// Every execution of `from` comes back to `to` from now on: the conversation
    /// it reports to moved there. It goes on running where it started.
    pub fn rehome(&self, from: &Path, to: &Path) {
        let mut inner = self.lock();
        for item in inner.items.iter_mut().filter(|i| i.root == from) {
            tracing::info!(target: "pi::background", execution = item.id, to = %to.display(), "rehomed");
            item.root = to.to_path_buf();
        }
        for (at, _) in inner.standing.iter_mut().filter(|(at, _)| at == from) {
            *at = to.to_path_buf();
        }
        drop(inner);
        self.wake();
    }

    /// Stop and forget every execution of checkouts at or under `root`, saying how
    /// many went: the checkout is gone, and a new one there is not it.
    pub fn drop_under(&self, root: &Path) -> usize {
        let mut inner = self.lock();
        let had = inner.items.len();
        inner.items.retain(|i| {
            let gone = i.root.starts_with(root);
            if gone {
                tracing::info!(target: "pi::background", execution = i.id, "stopped with its checkout");
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
        self.wake();
        gone
    }

    fn add(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
    ) -> (u64, CancellationToken) {
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        tracing::info!(
            target: "pi::background", execution = id, name = %description, root = %root.display(), "started"
        );
        let item = Item::blank(id, root, description, Phase::Running { stop });
        let gone = item.gone.clone();
        inner.items.push(item);
        drop(inner);
        self.wake();
        (id, gone)
    }

    // Change run `run` of execution `id`, if it is still here, and wake whoever
    // draws it.
    fn update(&self, id: u64, run: u64, change: impl FnOnce(&mut Item)) {
        let mut inner = self.lock();
        if let Some(at) = (inner.items.iter()).position(|i| i.id == id && i.run == run) {
            change(&mut inner.items[at]);
            settle(&mut inner.items, at);
        }
        drop(inner);
        self.wake();
    }
}

/// A command's id as a person or the model writes it: `3` or `#3`.
pub fn parse_id(word: &str) -> Option<u64> {
    word.trim_start_matches('#').parse().ok()
}

/// The shell controls, and where a call that outlives its turn is kept.
pub struct Commands {
    table: Arc<Table>,
}

impl Commands {
    pub fn new(table: Arc<Table>) -> Self {
        Self { table }
    }
}

impl Ask for Commands {
    fn ask(&self, root: PathBuf, asker: &str, asked: Asked) -> Result<Option<Answer>, String> {
        let from = Asker::Tool {
            name: asker.to_string(),
        };
        let question = Question::of(from, &asked)?;
        let mut inner = self.table.lock();
        let question = inner.number(question);
        tracing::info!(
            target: "pi::background", tool = asker, question = question.id, back = asked.back,
            text = %question.text, options = ?question.options, "ask"
        );
        let answer = asked.back.then(|| {
            let (to, answer) = oneshot::channel();
            inner.answers.insert(question.id, to);
            answer
        });
        inner.put(root, question);
        drop(inner);
        self.table.wake();
        Ok(answer)
    }
}

impl RunSink for Commands {
    fn start(
        &self,
        root: PathBuf,
        description: String,
        stop: CancellationToken,
    ) -> Arc<dyn RunHandle> {
        let (id, gone) = self.table.add(root, description, stop);
        Arc::new(Handle {
            id,
            run: 0,
            gone,
            table: self.table.clone(),
        })
    }
}

struct Handle {
    id: u64,
    run: u64,
    gone: CancellationToken,
    table: Arc<Table>,
}

// The last holder gone ends the execution, so one whose task died before saying
// `end` — a panic — does not stay listed as running for good.
impl Drop for Handle {
    fn drop(&mut self) {
        self.table.update(self.id, self.run, |i| {
            if i.running() {
                tracing::warn!(target: "pi::background", execution = i.id, "its task went without ending");
                i.phase = Phase::Over { stopped: false };
            }
        });
        self.gone.cancel();
    }
}

impl RunHandle for Handle {
    fn channel(&self) -> &dyn toolbox::channel::Channel {
        self
    }

    fn id(&self) -> u64 {
        self.id
    }

    fn watch(&self, output: Peek) {
        self.table
            .update(self.id, self.run, |i| i.output = Some(output));
    }

    fn rerun(&self, how: Rerun, again: Option<Again>) {
        self.table
            .update(self.id, self.run, |i| i.rerun = Some((how, again)));
    }

    fn end(&self) {
        self.table.update(self.id, self.run, |i| {
            if i.running() {
                tracing::info!(target: "pi::background", execution = self.id, "ended");
                i.phase = Phase::Over { stopped: false };
            }
        });
    }
}

impl toolbox::channel::Channel for Handle {
    fn listen(&self, to: UnboundedSender<Told>) {
        self.table
            .update(self.id, self.run, |i| i.listens = Listens::Inputs(to));
    }

    fn status(&self, text: String) {
        self.table.update(self.id, self.run, |i| {
            if i.progress != text {
                tracing::info!(target: "pi::background", execution = self.id, status = %text, "status");
                i.progress = text;
            }
        });
    }

    fn result(&self, text: String) {
        tracing::info!(target: "pi::background", execution = self.id, text = %text, "result");
        self.table.update(self.id, self.run, |i| {
            let due = i.due(&text);
            i.wait(Back::Result(due));
        });
    }

    // Writing instead of picking answers every question that does not wait:
    // what they wrote is the answer.
    fn input(&self, text: String) {
        tracing::info!(target: "pi::background", execution = self.id, text = %text, "input");
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
        self.table.wake();
    }

    fn choose(&self, asked: Asked) {
        let mut inner = self.table.lock();
        let Some(at) = inner.mine(self.id, self.run) else {
            return;
        };
        let item = &inner.items[at];
        let from = Asker::Background {
            id: item.id,
            name: item.name(),
        };
        let question = match Question::of(from, &asked) {
            Ok(q) => inner.number(q),
            Err(why) => {
                tracing::warn!(target: "pi::background", execution = self.id, %why, "a question dropped");
                return;
            }
        };
        tracing::info!(
            target: "pi::background", execution = self.id, question = question.id, text = %question.text,
            options = ?question.options, "choose"
        );
        let root = inner.items[at].root.clone();
        inner.put(root, question);
        drop(inner);
        self.table.wake();
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
                    target: "pi::background", execution = self.id, question, ?picks, %why, "an answer refused"
                );
                if let Some(to) = inner.items[at].listens.to() {
                    let _ = to.send(Told::Refused { question, why });
                }
                return;
            }
        };
        tracing::info!(target: "pi::background", execution = self.id, question, ?picks, "answer");
        if let Some(text) = inner.answered(question, "answered through an execution", picked) {
            inner.items[at].wait(Back::Input {
                text,
                note: String::new(),
                as_typed: true,
            });
        }
        drop(inner);
        self.table.wake();
    }

    fn interrupt(&self) {
        tracing::info!(target: "pi::background", execution = self.id, "interrupt");
        self.table.update(self.id, self.run, |i| i.interrupt = true);
    }

    fn follow(&self) {
        tracing::info!(target: "pi::background", execution = self.id, "follows");
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
        self.table.wake();
    }
}

#[async_trait]
impl Controls for Commands {
    fn description(&self) -> &str {
        "Call without a command to list background commands. Use `output` to read a command's latest output. \
             Use `stop` to stop commands by id, or `stop_all` to stop all commands. \
             Use `restart` to run a command again under its existing id. It stops the previous run first.\n\n\
             Results arrive automatically before the next model request. \
             If the run ended, results open a new turn. \
             Continue independent work, or report progress and end this turn. \
             Do not poll or wait for background work."
    }

    fn parameters(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            (
                "stop".into(),
                json!({
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Specify the command ids to stop.",
                }),
            ),
            (
                "stop_all".into(),
                json!({ "type": "boolean", "description": "Stop every running background command." }),
            ),
            (
                "restart".into(),
                json!({
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Specify the command ids to restart.",
                }),
            ),
            (
                "output".into(),
                json!({
                    "type": "integer",
                    "description": "Specify one command id to read its latest output while it runs.",
                }),
            ),
        ])
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let root = ctx.workspace.root();
        let parameters = self.parameters();
        if let Some(fields) = args.as_object() {
            for (key, value) in fields {
                if !value.is_null() && !parameters.contains_key(key) {
                    return Err(ToolError::Invalid(format!(
                        "unknown background control `{key}`"
                    )));
                }
            }
        }
        // A lone id still names one; so does `"#3"`.
        let id = |v: &Value| {
            v.as_u64()
                .or_else(|| parse_id(v.as_str()?))
                .ok_or_else(|| ToolError::Invalid(format!("`{v}` is not a command id")))
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
        let executions = self.table.listing(root);
        let out = match executions.is_empty() {
            true => "nothing in the background".to_string(),
            false => executions.join("\n"),
        };
        Ok(ToolOutput::text(out).with_preview("list"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tool::Tool;

    #[tokio::test]
    async fn stopping_a_delayed_command_prevents_the_command_from_starting() {
        let (dir, ctx) = context();
        let table = Arc::new(Table::default());
        let control = Arc::new(Commands::new(table.clone()));
        let executions = toolbox::bash::Bash::new(control.clone()).with_controls(control);
        executions
            .execute(
                json!({"background": true, "command": "echo started >> starts", "wait": 1}),
                &ctx,
            )
            .await
            .unwrap();
        let listed = executions.execute(json!({}), &ctx).await.unwrap().flatten();
        assert!(
            listed.contains("waiting") && listed.contains("#1"),
            "{listed}"
        );
        let output = executions
            .execute(json!({"output": 1}), &ctx)
            .await
            .unwrap()
            .flatten();
        assert_eq!(output, "nothing printed yet");
        executions
            .execute(json!({"stop": [1]}), &ctx)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(!dir.path().join("starts").exists());
        assert_eq!(
            table.background(ctx.workspace.root())[0].state,
            State::Stopped
        );
        assert!(table.take_back(ctx.workspace.root()).is_none());

        executions
            .execute(json!({"restart": [1]}), &ctx)
            .await
            .unwrap();
        let result = table
            .wait(ctx.workspace.root(), 1, std::time::Duration::from_secs(5))
            .await
            .unwrap()
            .expect("the command can still be restarted");
        assert!(result[0].contains("exited 0"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("starts")).unwrap(),
            "started\n"
        );
    }

    #[tokio::test]
    async fn a_command_without_a_start_delay_runs_immediately_in_its_cwd() {
        for wait in [None, Some(0)] {
            let (dir, ctx) = context();
            let table = Arc::new(Table::default());
            let control = Arc::new(Commands::new(table.clone()));
            let executions = toolbox::bash::Bash::new(control.clone()).with_controls(control);
            std::fs::create_dir(dir.path().join("subdir")).unwrap();
            let mut args = json!({"background": true, "command": "echo made > result; echo failed >&2; exit 3", "cwd": "subdir"});
            if let Some(wait) = wait {
                args["wait"] = json!(wait);
            }
            executions.execute(args, &ctx).await.unwrap();
            let result = table
                .wait(ctx.workspace.root(), 1, std::time::Duration::from_secs(5))
                .await
                .unwrap()
                .expect("a zero delay starts immediately");
            assert!(
                result[0].contains("exited 3") && result[0].contains("failed"),
                "{result:?}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("subdir/result")).unwrap(),
                "made\n"
            );
            assert!(
                table.take_back(ctx.workspace.root()).is_none(),
                "delivered once"
            );
        }
    }

    #[tokio::test]
    async fn invalid_start_requests_do_not_create_commands() {
        let (dir, ctx) = context();
        let table = Arc::new(Table::default());
        let control = Arc::new(Commands::new(table.clone()));
        let executions = toolbox::bash::Bash::new(control.clone()).with_controls(control);
        let marker = dir.path().join("starts");
        for args in [
            json!({"wait": [10], "timeout_ms": 420000}),
            json!({"wait": 30}),
            json!({"command": "echo started > starts", "wait": 30}),
            json!({"command": "echo started > starts", "timeout_ms": 420000}),
            json!({"background": true, "command": "echo started > starts", "wait": -1}),
            json!({"background": true, "command": "echo started > starts", "wait": 0.5}),
            json!({"background": true, "command": "echo started > starts", "wait": [10]}),
            json!({"background": true, "command": "echo started > starts", "wait": u64::MAX}),
            json!({"background": true, "command": "echo started > starts", "stop_all": true}),
            json!({"background": true, "command": " "}),
            json!({"join": 10}),
            json!({"command":"echo started > starts", "background":"true"}),
        ] {
            assert!(
                executions.execute(args.clone(), &ctx).await.is_err(),
                "{args}"
            );
            assert!(table.background(ctx.workspace.root()).is_empty(), "{args}");
            assert!(!marker.exists());
        }
    }

    #[tokio::test]
    async fn a_scheduled_command_that_cannot_launch_reports_the_failure() {
        let (_dir, ctx) = context();
        let table = Arc::new(Table::default());
        let control = Arc::new(Commands::new(table.clone()));
        let executions = toolbox::bash::Bash::new(control.clone()).with_controls(control);
        executions
            .execute(
                json!({"background": true, "command": "echo started", "cwd": "missing", "wait": 0}),
                &ctx,
            )
            .await
            .unwrap();
        let result = table
            .wait(ctx.workspace.root(), 1, std::time::Duration::from_secs(5))
            .await
            .unwrap()
            .expect("a failed launch must end the execution");
        assert!(result[0].contains("could not start:"), "{result:?}");
        assert_eq!(
            table.background(ctx.workspace.root())[0].state,
            State::Exited
        );
    }

    #[tokio::test]
    async fn changed_command_output_is_paced_and_only_goes_to_its_checkout() {
        let table = Arc::new(Table::default());
        let root = Path::new("/checkout");
        let sink = Commands::new(table.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let follower = sink.start(root.into(), "bridge".into(), Default::default());
        follower.channel().listen(tx);
        follower.channel().follow();
        let (elsewhere, mut quiet) = tokio::sync::mpsc::unbounded_channel();
        let other = sink.start("/other".into(), "bridge".into(), Default::default());
        other.channel().listen(elsewhere);
        other.channel().follow();
        let execution = start(&table, root, "scan");
        let output = Arc::new(Mutex::new("1/3\n".to_string()));
        let seen = output.clone();
        execution.watch(Arc::new(move || seen.lock().unwrap().clone()));
        table.relay_progress();
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Progress(format!("#{}: 1/3", execution.id())))
        );
        *output.lock().unwrap() = "1/3\n2/3\n".into();
        table.relay_progress();
        assert!(rx.try_recv().is_err(), "rapid changes wait");
        table.lock().progress_at = Some(Instant::now() - PROGRESS_EVERY);
        table.relay_progress();
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Progress(format!("#{}: 2/3", execution.id())))
        );
        table.lock().progress_at = Some(Instant::now() - PROGRESS_EVERY);
        table.relay_progress();
        assert!(rx.try_recv().is_err(), "unchanged output is silent");
        execution.end();
        *output.lock().unwrap() = "3/3\n".into();
        table.lock().progress_at = Some(Instant::now() - PROGRESS_EVERY);
        table.relay_progress();
        assert!(
            rx.try_recv().is_err(),
            "ended executions report through their result"
        );
        assert!(quiet.try_recv().is_err(), "another checkout hears nothing");
    }

    fn start(table: &Arc<Table>, root: &Path, name: &str) -> Arc<dyn RunHandle> {
        RunSink::start(
            &Commands::new(table.clone()),
            root.to_path_buf(),
            name.into(),
            CancellationToken::new(),
        )
    }

    fn result_of(table: &Table, root: &Path) -> Option<Due> {
        match table.take_back(root)?.1 {
            Back::Result(due) => Some(due),
            Back::Input { .. } => panic!("an input, not a result"),
        }
    }

    // A wait wakes when its execution ends, and takes the result from the lane.
    #[tokio::test]
    async fn a_wait_takes_the_result_its_command_ends_with() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = start(&table, &root, "build");
        let id = execution.id();
        let waiting = {
            let (table, root) = (table.clone(), root.clone());
            tokio::spawn(async move {
                table
                    .wait(&root, id, std::time::Duration::from_secs(5))
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished(), "it waits while the execution runs");
        execution.channel().input("a person's line".into());
        execution.channel().result("exited 0".into());
        execution.end();
        let said = waiting.await.expect("the wait ran");
        assert_eq!(said, Ok(Some(vec!["exited 0".to_string()])));
        assert!(
            matches!(table.take_back(&root), Some((_, Back::Input { .. }))),
            "a person's line still goes to the lane"
        );
        assert!(table.take_back(&root).is_none(), "the result went once");
    }

    #[tokio::test]
    async fn a_wait_gives_up_on_a_command_still_running() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = start(&table, &root, "server");
        let waited = table
            .wait(&root, execution.id(), std::time::Duration::from_millis(20))
            .await;
        assert_eq!(waited, Ok(None));
        assert!(table.wait(&root, 99, Default::default()).await.is_err());
    }

    fn context() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        (dir, ctx)
    }

    #[tokio::test]
    async fn an_input_waits_in_line_and_its_command_hears_of_its_turn() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let execution = RunSink::start(
            &Commands::new(table.clone()),
            root.clone(),
            "wechat".into(),
            CancellationToken::new(),
        );
        execution.channel().listen(tx);
        execution.channel().result("connected".into());
        execution.channel().input("why did the tests fail".into());
        assert!(matches!(table.take_back(&root), Some((_, Back::Result(_)))));
        let Some((id, Back::Input { text, note, .. })) = table.take_back(&root) else {
            panic!("the input comes next")
        };
        assert_eq!(
            (id, text.as_str()),
            (execution.id(), "why did the tests fail")
        );
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
        let execution = start(&table, &root, "wechat");
        for n in 0..MAX_WAITING + 2 {
            execution.channel().input(format!("line {n}"));
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

    fn asked(question: &str, options: &[&str], back: bool) -> Asked {
        Asked {
            question: question.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            back,
        }
    }

    // Said and gone: what an execution leaves for the screen outlives its exit.
    #[tokio::test]
    async fn a_question_outlives_its_command_and_a_newer_one_replaces_it() {
        let table = Arc::new(Table::default());
        let execution = start(&table, Path::new("/checkout"), "judge");
        execution
            .channel()
            .choose(asked("a new task?", &["/new"], false));
        execution
            .channel()
            .choose(asked("a long one?", &["/compact", "go on"], false));
        execution.channel().choose(asked(
            "hidden?",
            &["/compact\nand what was not shown"],
            false,
        ));
        execution.end();
        assert!(table.questions(Path::new("/elsewhere")).is_empty());
        let up = table.questions(Path::new("/checkout"));
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].text, "a long one?");
        assert_eq!(up[0].options, ["/compact", "go on"]);
        assert!(
            matches!(&up[0].from, Asker::Background { name, .. } if name.starts_with("judge #")),
            "{:?}",
            up[0].from
        );
        assert!(
            table.background(Path::new("/checkout")).is_empty(),
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
        let executions = Commands::new(table.clone());
        let root = PathBuf::from("/checkout");
        let execution = start(&table, &root, "judge");
        execution
            .channel()
            .choose(asked("first?", &["/new"], false));
        executions
            .ask(root.clone(), "choose", asked("which?", &["a"], false))
            .unwrap();
        execution
            .channel()
            .choose(asked("second?", &["/new"], false));
        let texts = |table: &Table| -> Vec<String> {
            (table.questions(&root).into_iter())
                .map(|q| q.text)
                .collect()
        };
        assert_eq!(texts(&table), ["second?", "which?"]);
        let _waits = (0..2)
            .map(|_| executions.ask(root.clone(), "guard", asked("allow?", &["yes"], true)))
            .collect::<Vec<_>>();
        assert_eq!(texts(&table), ["second?", "which?", "allow?", "allow?"]);
    }

    #[tokio::test]
    async fn stopping_a_command_withdraws_what_it_asked() {
        let table = Arc::new(Table::default());
        let execution = start(&table, Path::new("/checkout"), "judge");
        execution.channel().choose(asked("new?", &["/new"], false));
        assert_eq!(table.questions(Path::new("/checkout")).len(), 1);
        table.stop(Path::new("/checkout"), execution.id()).unwrap();
        assert!(table.questions(Path::new("/checkout")).is_empty());
    }

    // A call in its turn asks: the answer reaches it, once; an asker that
    // stopped waiting has its question withdrawn, never answered.
    #[tokio::test]
    async fn an_answer_goes_back_to_the_call_that_waits_on_it() {
        let table = Arc::new(Table::default());
        let executions = Commands::new(table.clone());
        let root = PathBuf::from("/checkout");
        let mut answer = executions
            .ask(
                root.clone(),
                "guard",
                asked("run rm -rf?", &["allow", "deny"], true),
            )
            .unwrap()
            .expect("it waits");
        let fire = executions
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

        let gone = executions.ask(root.clone(), "guard", asked("again?", &["yes"], true));
        drop(gone);
        assert!(
            table.questions(&root).is_empty(),
            "withdrawn before it was ever shown"
        );
        assert!(
            executions
                .ask(root, "guard", asked("?", &[], true))
                .is_err(),
            "one that cannot be put says why"
        );
    }

    // A command that follows is told each question up on its checkout — even one
    // up before it followed — and when it comes down; its pick goes out as
    // typed, while writing instead lets every question that does not wait lapse.
    #[tokio::test]
    async fn a_follower_hears_each_question_and_answers_it() {
        let table = Arc::new(Table::default());
        let executions = Commands::new(table.clone());
        let root = PathBuf::from("/checkout");
        executions
            .ask(
                root.clone(),
                "choose",
                asked("which?", &["/compact", "go on"], false),
            )
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bridge = RunSink::start(
            &executions,
            root.clone(),
            "telegram".into(),
            CancellationToken::new(),
        );
        bridge.channel().listen(tx);
        bridge.channel().follow();
        let first = table.questions(&root)[0].id;
        assert!(matches!(rx.try_recv(), Ok(Told::Question { id, .. }) if id == first));

        for picks in [vec![5], vec![0, 1], vec![0, 0]] {
            bridge.channel().answer(first, picks.clone());
            assert!(
                matches!(rx.try_recv(), Ok(Told::Refused { question, .. }) if question == first),
                "{picks:?} is told back why not"
            );
        }
        assert_eq!(table.questions(&root).len(), 1, "no such answer, still up");
        bridge.channel().answer(first, vec![0]);
        assert_eq!(rx.try_recv(), Ok(Told::Closed(first)));
        let Some((_, Back::Input { text, as_typed, .. })) = table.take_back(&root) else {
            panic!("the pick went nowhere");
        };
        assert_eq!((text.as_str(), as_typed), ("/compact", true));
        bridge.channel().answer(first, vec![1]);
        assert!(table.take_back(&root).is_none(), "answered once");
        assert!(
            matches!(rx.try_recv(), Ok(Told::Refused { .. })),
            "and told so"
        );

        let mut waits = executions
            .ask(root.clone(), "guard", asked("allow?", &["yes", "no"], true))
            .unwrap()
            .expect("it waits");
        executions
            .ask(root.clone(), "choose", asked("next?", &["a"], false))
            .unwrap();
        let up = table.questions(&root);
        for q in &up {
            assert!(matches!(rx.try_recv(), Ok(Told::Question { id, .. }) if id == q.id));
        }
        bridge.channel().input("neither".into());
        assert_eq!(rx.try_recv(), Ok(Told::Closed(up[1].id)), "it lapsed");
        assert_eq!(table.questions(&root).len(), 1, "one that waits stays");
        bridge.channel().answer(up[0].id, vec![1]);
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
    async fn an_interrupt_names_the_commands_checkout_once() {
        let table = Arc::new(Table::default());
        let execution = start(&table, Path::new("/checkout"), "wechat");
        execution.channel().interrupt();
        execution.channel().interrupt();
        assert_eq!(table.take_interrupts(), [PathBuf::from("/checkout")]);
        assert!(table.take_interrupts().is_empty());
    }

    #[tokio::test]
    async fn a_result_is_delivered_once() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = start(&table, &root, "find callers");
        assert!(table.working(&root));
        execution.channel().result("three callers".into());
        execution.end();
        let due = result_of(&table, &root).unwrap();
        assert!(due.line.contains("three callers"), "{}", due.line);
        assert_eq!(due.label, "find callers #1 three callers");
        assert!(result_of(&table, &root).is_none(), "delivered once");
        assert!(table.background(&root).is_empty());
    }

    #[tokio::test]
    async fn stopping_a_command_asks_it_to_wind_down_and_forgets_it() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let stop = CancellationToken::new();
        let execution = RunSink::start(
            &Commands::new(table.clone()),
            root.clone(),
            "long execution".into(),
            stop.clone(),
        );
        assert!(table.stop(Path::new("/elsewhere"), execution.id()).is_err());
        table.stop(&root, execution.id()).unwrap();
        assert!(stop.is_cancelled());
        execution.channel().result("too late".into());
        execution.end();
        assert!(
            result_of(&table, &root).is_none(),
            "a stopped execution says nothing more"
        );
        assert!(table.background(&root).is_empty());
    }

    // A crawler's queued results come back together, in order, and the execution
    // stays listed until they are delivered, even after it has exited.
    #[tokio::test]
    async fn a_detached_command_reports_queued_results_together_in_order() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = RunSink::start(
            &Commands::new(table.clone()),
            root.clone(),
            "crawl".into(),
            CancellationToken::new(),
        );
        execution.channel().status("1/2 pages".into());
        execution.channel().result("page one".into());
        execution.channel().result("page two".into());
        execution.end();
        assert_eq!(
            table.waiting(&root, execution.id()).unwrap(),
            ["page one", "page two"]
        );
        assert!(
            table
                .waiting(Path::new("/elsewhere"), execution.id())
                .is_none()
        );
        assert_eq!(table.background(&root)[0].progress, "1/2 pages");
        assert_eq!(table.background(&root)[0].state, State::Back);
        let first = result_of(&table, &root).unwrap();
        assert_eq!(
            first.line,
            "Background command #1 (crawl) reports:\n\npage one\n\nBackground command #1 (crawl) reports:\n\npage two"
        );
        assert_eq!(first.text, "page one\n\npage two");
        assert!(result_of(&table, &root).is_none(), "reported once");
        assert!(table.background(&root).is_empty());
    }

    // A command that outruns its lane keeps what fits and says what it lost.
    #[tokio::test]
    async fn results_past_the_cap_are_counted_not_kept() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = RunSink::start(
            &Commands::new(table.clone()),
            root.clone(),
            "flood".into(),
            CancellationToken::new(),
        );
        for n in 0..MAX_WAITING + 3 {
            execution.channel().result(n.to_string());
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
    async fn a_command_whose_task_died_without_ending_leaves() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = start(&table, &root, "crashy");
        let _ = tokio::spawn(async move {
            let _command = execution;
            panic!("died before end");
        })
        .await;
        assert!(table.background(&root).is_empty());
    }

    #[tokio::test]
    async fn a_command_that_ends_with_nothing_to_say_leaves() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = RunSink::start(
            &Commands::new(table.clone()),
            root.clone(),
            "quiet".into(),
            Default::default(),
        );
        execution.end();
        assert!(table.background(&root).is_empty());
        assert!(result_of(&table, &root).is_none());
    }

    #[tokio::test]
    async fn a_removed_checkout_stops_its_commands() {
        let table = Arc::new(Table::default());
        let (a, _) = table.add("/repo.worktrees/a".into(), "a".into(), Default::default());
        table.add("/repo".into(), "main".into(), Default::default());
        let stop = match &table.lock().items[0].phase {
            Phase::Running { stop } => stop.clone(),
            Phase::Over { .. } => panic!("it runs"),
        };
        table.drop_under(Path::new("/repo.worktrees/a"));
        assert!(stop.is_cancelled(), "#{a} was asked to stop");
        assert!(table.background(Path::new("/repo.worktrees/a")).is_empty());
        assert_eq!(table.background(Path::new("/repo")).len(), 1);
    }

    type Reruns = tokio::sync::mpsc::UnboundedReceiver<(Arc<dyn RunHandle>, CancellationToken)>;

    // A command that can run again: each rerun hands back the handle and stop
    // it was given, as a launch would keep them.
    fn rerunnable(table: &Arc<Table>, root: &Path) -> (Arc<dyn RunHandle>, Reruns) {
        let execution = start(table, root, "serve");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        execution.rerun(
            Arc::new(move |execution, stop| {
                let _ = tx.send((execution, stop));
                Ok(())
            }),
            None,
        );
        (execution, rx)
    }

    // Shutdown must preserve completed results and ignore late reports.
    #[tokio::test]
    async fn shutdown_stops_only_running_commands_on_its_checkout() {
        let table = Arc::new(Table::default());
        let root = Path::new("/checkout");
        let (running, _) = rerunnable(&table, root);
        let (finished, _) = rerunnable(&table, root);
        finished.channel().result("done".into());
        finished.end();
        let stop = CancellationToken::new();
        let plain = Commands::new(table.clone()).start(root.into(), "plain".into(), stop.clone());
        let elsewhere = start(&table, Path::new("/elsewhere"), "elsewhere");

        let stopped = table.stop_running(root, "process exiting");
        assert_eq!(stopped.len(), 2);
        assert!(stopped[0].starts_with(&format!("#{} ", running.id())));
        assert!(stopped[1].starts_with(&format!("#{} ", plain.id())));
        assert!(stop.is_cancelled());
        assert!(table.stop_running(root, "process exiting").is_empty());
        assert_eq!(table.background(root)[0].state, State::Stopped);
        assert_eq!(table.background(root)[1].state, State::Back);
        assert_eq!(
            table.background(Path::new("/elsewhere"))[0].id,
            elsewhere.id()
        );

        running.channel().result("late".into());
        plain.channel().result("late".into());
        running.end();
        assert_eq!(table.waiting(root, running.id()), Some(vec![]));
        assert_eq!(table.waiting(root, plain.id()), None);
        let Some((id, Back::Result(due))) = table.take_back(root) else {
            panic!("the completed result is still due")
        };
        assert_eq!(id, finished.id());
        assert_eq!(due.text, "done");
    }

    // Stopped, it stays listed; restarted, it runs again under its id once
    // the last run has let go, and that run's late words are not heard.
    #[tokio::test]
    async fn a_restarted_command_waits_for_the_last_run_and_keeps_its_id() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (old, mut reruns) = rerunnable(&table, &root);
        let id = old.id();

        table.stop(&root, id).unwrap();
        old.channel().result("said as it went".into());
        assert_eq!(table.background(&root)[0].state, State::Stopped);

        table.restart(&root, id).unwrap();
        assert_eq!(table.background(&root)[0].state, State::Running);
        tokio::task::yield_now().await;
        assert!(reruns.try_recv().is_err(), "the last run still holds on");

        old.channel().result("late".into());
        drop(old);
        let (new, stop) = reruns.recv().await.expect("run again");
        assert_eq!(new.id(), id);
        assert!(table.waiting(&root, id).unwrap().is_empty(), "{id}");
        assert_eq!(table.background(&root)[0].state, State::Running);

        table.restart(&root, id).unwrap();
        assert!(stop.is_cancelled(), "a running one is stopped first");
        new.channel().result("exited 1".into());
        new.end();
        assert_eq!(table.background(&root)[0].state, State::Running);
    }

    // A command that says as data how it runs again crosses to the next pi
    // through its file: listed there under its id, stopped even if it was
    // running, and it starts over; ids given out later pass it. One that
    // cannot say it is left behind, and the file is read once.
    #[tokio::test]
    async fn a_command_is_carried_to_the_next_pi() {
        let old = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (plain, _reruns) = rerunnable(&old, &root);
        let execution = start(&old, &root, "serve");
        let again = Again {
            command: "python3 serve.py".into(),
            cwd: root.clone(),
            ..Again::default()
        };
        execution.rerun(Arc::new(|_, _| Ok(())), Some(again.clone()));
        let running = start(&old, &root, "watch");
        running.rerun(Arc::new(|_, _| Ok(())), Some(again.clone()));
        old.stop(&root, plain.id()).unwrap();
        old.stop(&root, execution.id()).unwrap();

        let carried = old.carried(&root);
        assert_eq!(carried.len(), 2, "{carried:?}");
        assert_eq!((carried[0].id, carried[0].stopped), (execution.id(), true));
        assert_eq!((carried[1].id, carried[1].stopped), (running.id(), true));
        assert_eq!(carried[0].again, again);
        assert!(old.carried(Path::new("/elsewhere")).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("background.json");
        hand_over(&path, &carried[..1]).unwrap();
        let back = take_over(&path).unwrap();
        assert!(take_over(&path).unwrap().is_empty(), "read once");

        let new = Arc::new(Table::default());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for c in back {
            let tx = tx.clone();
            new.revive(
                c,
                Arc::new(move |execution, _| {
                    let _ = tx.send(execution.id());
                    Ok(())
                }),
            );
        }
        let listed = new.background(&root);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].id, listed[0].state),
            (execution.id(), State::Stopped)
        );
        new.restart(&root, execution.id()).unwrap();
        assert_eq!(rx.recv().await, Some(execution.id()));
        assert!(start(&new, &root, "next").id() > execution.id());

        // Another lane's pi gave out the same id: this one takes a free one.
        new.revive(carried[0].clone(), Arc::new(|_, _| Ok(())));
        let ids: Vec<_> = new.background(&root).iter().map(|j| j.id).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{ids:?}");

        hand_over(&path, &carried).unwrap();
        hand_over(&path, &[]).unwrap();
        assert!(!path.exists(), "nothing to carry clears what was left");
    }

    // One that ended by itself is kept to start over; a stop leaves it be.
    #[tokio::test]
    async fn an_exited_command_is_kept_to_start_over() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let (execution, _reruns) = rerunnable(&table, &root);
        let id = execution.id();
        execution.channel().result("exited 1".into());
        execution.end();
        assert!(result_of(&table, &root).is_some());
        assert_eq!(table.background(&root)[0].state, State::Exited);
        assert!(table.stop(&root, id).is_err());
        assert_eq!(table.background(&root)[0].state, State::Exited);
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
        assert_eq!(table.background(&root)[0].state, State::Stopped);
    }

    #[tokio::test]
    async fn only_a_command_that_says_how_can_restart() {
        let table = Arc::new(Table::default());
        let root = PathBuf::from("/checkout");
        let execution = start(&table, &root, "find callers");
        let why = table.restart(&root, execution.id()).unwrap_err();
        assert!(why.contains("cannot be started over"), "{why}");
        assert!(table.restart(&root, 99).is_err());
    }
}
