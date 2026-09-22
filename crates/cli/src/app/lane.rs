//! One checkout being worked in, and everything the workspace root decides.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

pub fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

use std::path::Path;

use agent::session::{EntryId, Session};
use agent::{Agent, Event, Steer, Totals};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use tools::Ctx;

use crate::app::looping::{Looping, Round};
use crate::app::meter::{Snapshot, Tally};

/// Where this lane's run stands.
///
/// One value rather than a flag beside an outcome: a lane is idle, or running,
/// or holding the end of a run nobody has seen — never two of those, and the
/// compiler is what should say so.
pub enum Run {
    // Nothing running, nothing waiting to be looked at.
    Idle,
    // A run under way.
    Running {
        // What `esc` cancels, and only for the lane in front.
        cancel: CancellationToken,
        // Where a line typed mid-run goes, when the job in flight is a run
        // and can hear one: the run takes it at its next turn boundary.
        //
        // `None` for a `!` or a `/compact`. Neither calls a model, so neither
        // has a boundary to take a line at, and a line typed at one waits for
        // the lane the way every line used to. Optional rather than an empty
        // mailbox on every job, because the two are not the same thing to say.
        steer: Option<Steer>,
        // Esc caught the prompt on its way out: stop the run, then unsend it.
        // Set while the run works, acted on when it ends.
        unsend: bool,
    },
    // A run that ended while this lane was out of sight, kept until the screen
    // is looking at it and can show how it went.
    //
    // Only for work that can be finished off on the view in front: closing a
    // partial stream, landing animated tool rows, `say` without a prefix. A
    // job with none of those settles where it ended instead, or its report
    // waits on a screen that may never come back.
    Ended {
        out: Result<llm::stream::Usage, agent::AgentError>,
        // Esc asked for the prompt back while this was still running.
        unsend: bool,
    },
}

/// What a job leaves behind when it hands the transcript back.
///
/// One value rather than a bool and a leftover list read from two places: a
/// caller that took the `unsend` and forgot the rest would drop what the user
/// said into the gap between the run's last look and its return.
#[derive(Default)]
pub struct Handback {
    /// Esc asked for the prompt back.
    pub unsend: bool,
    /// Said to the run after its last look at the mailbox, so never heard.
    /// It goes back to the surface to be run as an ordinary line.
    pub unheard: Vec<String>,
}

pub struct Lane {
    pub token: u64,
    /// Shared so a run can take it with it: `Agent::run` needs only `&self`,
    /// and a run outlives the borrow the surface could lend it. `/model` and
    /// `/reload` write through `Arc::make_mut`, so a run in flight keeps the
    /// agent it started on — which is what they meant all along.
    pub agent: std::sync::Arc<Agent>,
    /// The transcript, or None while a run has it — it is lent out for the
    /// length of a run. An empty session left in its place would read like a
    /// session with nothing in it, which is a different thing to anyone asking.
    pub session: Option<Session>,
    pub id: String,
    /// When this session began. Held rather than read back: it is set once and
    /// never changes, and going to disk for it made every save parse the whole
    /// transcript to recover one integer.
    pub created: u64,
    /// What this lane's finished runs have cost, in and out and in money.
    /// Per lane, not per surface: with lanes working off-screen the surface
    /// that shows the bill has to be able to say which lane ran it up.
    pub totals: Totals,
    /// What the run in flight has cost so far, as its events stated it. Seeded
    /// from `totals` when a run arms and cleared when a session begins; the
    /// status lines read this run alone, and `/status` reads the session.
    pub tally: Tally,

    /// What the user calls this session, if anything.
    pub name: Option<String>,
    /// The instruction files this run stands on, named as a person would.
    /// Shown under the banner; rebuilt by `/reload` like everything else the
    /// config decides.
    pub context: Vec<String>,
    /// What this checkout tells an agent, verbatim — the tail of the system
    /// prompt that came from the tree. Held so a subagent rebuilt after
    /// `/model` gets the same one the lane was armed with.
    pub standing: std::sync::Arc<str>,
    /// Carried across turns: the file locks and edit shifts outlive any one run.
    pub ctx: Ctx,
    /// Which worktree the session is in, or None in the repository's own
    /// checkout. Held so the status line can say where the work is landing.
    pub worktree: Option<String>,
    /// Where this lane's runs post what they are doing. One channel per lane,
    /// so an event needs no label to say which screen it belongs on.
    pub events: UnboundedSender<Event>,
    /// The other end. Drained by the loop, into the view when this lane is in
    /// front and into `pending` when it is not.
    pub inbox: UnboundedReceiver<Event>,
    /// What arrived while nobody was looking, in order, waiting to be replayed
    /// into the view the moment this lane comes back to the front.
    pub pending: Vec<Event>,
    /// Where this lane's run stands.
    pub run: Run,
    /// What a slash answers to here, and the key map in force. Both are what
    /// this root's config and skills resolved to, so they travel with the lane
    /// rather than with the run — a tree switched back to answers to its own.
    pub keys: std::sync::Arc<crate::store::keys::Keys>,
    pub commands: std::sync::Arc<Vec<crate::input::commands::Command>>,
    /// The `/loop` this lane is under, if any.
    pub looping: Option<Looping>,
    /// The `/loop` round waiting in the queue, taken by the run it arms: the
    /// ask it opens records which automatic round it is.
    pub pending_round: Option<u64>,
}

impl Lane {
    /// A lane is born with its own channel: nothing else can post to it, and
    /// nothing it posts can land on another screen.
    pub fn channel() -> (UnboundedSender<Event>, UnboundedReceiver<Event>) {
        unbounded_channel()
    }

    /// Take the end of a run this lane has been holding, if it is holding one.
    ///
    /// Only when it is: a lane still working must keep its `Running`, or the
    /// token `esc` reaches and the request to unsend go with it.
    pub fn take_ended(&mut self) -> Option<(Result<llm::stream::Usage, agent::AgentError>, bool)> {
        match self.run {
            Run::Ended { .. } => match std::mem::replace(&mut self.run, Run::Idle) {
                Run::Ended { out, unsend } => Some((out, unsend)),
                _ => None,
            },
            _ => None,
        }
    }

    /// A job has given this lane's transcript back. The one place `Running`
    /// ends: a lane left in it queues every later prompt and never drains.
    pub fn finish(&mut self) -> Handback {
        match std::mem::replace(&mut self.run, Run::Idle) {
            Run::Running { unsend, steer, .. } => Handback {
                unsend,
                unheard: steer.map(|s| s.take()).unwrap_or_default(),
            },
            _ => Handback::default(),
        }
    }

    /// Where a line typed mid-run goes, while a run is there to hear one.
    pub fn steer(&self) -> Option<&Steer> {
        match &self.run {
            Run::Running { steer, .. } => steer.as_ref(),
            _ => None,
        }
    }

    /// Whether a run has this lane's transcript right now.
    pub fn is_running(&self) -> bool {
        matches!(self.run, Run::Running { .. })
    }

    /// The round this lane's loop queued has begun. Nothing else it runs is
    /// one, so nothing else moves it on.
    pub fn loop_running(&mut self) {
        if let Some(looping) = &mut self.looping {
            looping.mark_running();
        }
    }

    /// Put this lane under a loop, marked from where the tree stands now.
    pub fn loop_start(&mut self, goal: String) {
        self.looping = Some(Looping::start(&self.ctx, goal));
    }

    /// What the loop in force does now that a round has ended — `None` when
    /// there was no loop, or when the round that ended was not the loop's.
    ///
    /// The loop is taken out and only put back to go round again, so every
    /// ending drops it without a second place to remember that.
    pub fn loop_step(&mut self, finished: bool, cap: Option<usize>) -> Option<Round> {
        let mut looping = self.looping.take()?;
        if !looping.is_running() {
            self.looping = Some(looping);
            return None;
        }
        let out = looping.step(&self.ctx, finished, cap);
        if matches!(out, Round::Again { .. }) {
            self.looping = Some(looping);
        }
        Some(out)
    }

    // ------------------------------------------------------- what a run does

    /// A run has taken this lane. `steer` is the mailbox it hears at the next
    /// turn boundary; a job that calls no model has none to hear from.
    pub fn begin(&mut self, cancel: CancellationToken, steer: Option<Steer>) {
        self.run = Run::Running {
            cancel,
            steer,
            unsend: false,
        };
    }

    /// Ask the run under way to stop, and say whether there was one. `unsend`
    /// also takes the prompt back, which is what `esc` means.
    pub fn stop(&mut self, unsend: bool) -> bool {
        match &mut self.run {
            Run::Running {
                cancel,
                unsend: take_back,
                ..
            } => {
                cancel.cancel();
                *take_back = unsend;
                true
            }
            _ => false,
        }
    }

    /// Stop whatever is running, saying nothing about the prompt. For the ways
    /// out that are leaving anyway.
    pub fn cancel(&self) {
        if let Run::Running { cancel, .. } = &self.run {
            cancel.cancel();
        }
    }

    /// What a run left behind, and whether its prompt was taken back. The other
    /// half of [`Lane::take_ended`], which is the surface collecting it.
    pub fn end(&mut self, out: Result<llm::stream::Usage, agent::AgentError>, unsend: bool) {
        self.run = Run::Ended { out, unsend };
    }

    /// Where this lane's run stands, for a surface that only draws it.
    pub fn run(&self) -> &Run {
        &self.run
    }

    // ---------------------------------------------------- what a surface reads

    /// What a surface draws this lane from: where it is, what it is doing, and
    /// what it is doing it under. One call rather than four reaches into it.
    pub fn snapshot(
        &self,
        model: &str,
        started: Option<std::time::Duration>,
        queued: usize,
    ) -> Snapshot {
        // Both the lines queued on the surface and the ones said mid-run are
        // lines the user has given it that have not reached the model. Which
        // side of the seam one waits on is the loop's business, not the
        // reader's.
        self.tally.snapshot(
            model,
            self.worktree.as_deref(),
            started,
            queued + self.steer().map_or(0, agent::Steer::len),
        )
    }

    /// The checkout this lane works in, when it is not the repository's own.
    pub fn worktree(&self) -> Option<&str> {
        self.worktree.as_deref()
    }

    /// The loop this lane is under, if any.
    pub fn looping(&self) -> Option<&Looping> {
        self.looping.as_ref()
    }

    /// The transcript, for a surface that only reads it.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// The ask nobody has answered, when a run left one open.
    pub fn last_ask(&self) -> Option<EntryId> {
        self.session.as_ref().and_then(Session::last_ask)
    }

    /// The checkout this lane works in.
    pub fn root(&self) -> &Path {
        self.ctx.workspace.root()
    }

    /// The context a job runs with: this lane's, with the job's own way out.
    pub fn ctx_for(&self, cancel: CancellationToken) -> Ctx {
        self.ctx.clone().with_cancel(cancel)
    }

    /// What the views of this lane are keyed by.
    pub fn token(&self) -> u64 {
        self.token
    }

    /// The model this lane's runs ask for.
    pub fn model(&self) -> &str {
        &self.agent.spec.model
    }

    /// The agent these runs go through, for what only it knows.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// What runs of this lane report as they go.
    pub fn sender(&self) -> &UnboundedSender<Event> {
        &self.events
    }

    /// What this lane has been told and not yet heard.
    pub fn inbox(&mut self) -> &mut UnboundedReceiver<Event> {
        &mut self.inbox
    }

    /// The id this lane's session is saved under.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// What arrived while nobody was looking, in order.
    pub fn take_pending(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending)
    }

    // ------------------------------------------------------------- the meter

    /// Fold an event into the meter. The loop's facts arrive here and nowhere
    /// else: what a run has spent is only known from them.
    pub fn note(&mut self, event: &Event) {
        self.tally.on(event, self.agent.spec.pricing);
    }

    /// Start the meter at what the session has already spent, so a resumed
    /// tree does not read as a free run.
    pub fn seed_meter(&mut self) {
        self.tally.seed(self.totals);
    }

    /// Charge what a run spent to the session, once it has reported it, at the
    /// rate this lane's model is priced at.
    pub fn charge(&mut self, spent: &llm::stream::Usage) {
        let cost = self.agent.spec.pricing.cost(spent);
        self.totals.add(spent, cost);
    }

    /// The same, from the run itself: its own word when it has one, and the
    /// meter's reading of the turn in flight when it was cut short before
    /// pricing it.
    pub fn charge_run(&mut self, out: &Result<llm::stream::Usage, agent::AgentError>) {
        let spent = match out {
            Ok(usage) => *usage,
            Err(_) => self.tally.run_spend().usage,
        };
        self.charge(&spent);
    }

    // -------------------------------------------------------------- the loop

    /// Take the loop off this lane: what `/loop` off, a round that ended it,
    /// and a lane being left all mean.
    pub fn take_looping(&mut self) -> Option<Looping> {
        self.looping.take()
    }

    /// Arm the round the loop queued — or none, when the line about to run is
    /// not one of a loop's own.
    pub fn arm_round(&mut self, round: Option<u64>) {
        self.pending_round = round;
    }

    /// Take that round back: a job is starting, or the loop it belonged to is
    /// gone.
    pub fn take_round(&mut self) -> Option<u64> {
        self.pending_round.take()
    }

    // -------------------------------------------------------- the transcript

    /// Lend the transcript to a job for the length of its run.
    pub fn take_session(&mut self) -> Option<Session> {
        self.session.take()
    }

    /// Take it back: the job has finished, or never started.
    pub fn return_session(&mut self, session: Session) {
        self.session = Some(session);
    }

    /// What a run's ending should leave in the transcript.
    pub fn note_outcome(&mut self, out: &Result<llm::stream::Usage, agent::AgentError>) {
        if let Some(session) = self.session.as_mut() {
            session.note_outcome(out);
        }
    }

    /// A line the run filed rather than the model: a command's note, read back
    /// with the rest of the transcript. Nothing to file when there is none.
    pub fn push_note(&mut self, note: &str) {
        if let Some(session) = self.session.as_mut() {
            session.push_note(note);
        }
    }
}
