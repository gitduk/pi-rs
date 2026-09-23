//! One checkout being worked in: its run state machine, its transcript, and
//! what the App does to the lanes it holds — opening one, removing one,
//! starting a session in it, saving it, resuming another.
//!
//! The verbs stay on `App` because most of them read the store, the config and
//! the settings it holds; a `Lanes` type would be those four passed down one
//! at a time.

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

use super::App;
use crate::app::looping::{Cut, Looping, Round};
use crate::app::meter::{Snapshot, Tally};
use crate::input::commands::ago;
use crate::input::{Rewound, refused};
use crate::store::config;
use crate::store::icons;
use crate::store::journal;
use crate::store::session::{self, Stored};

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
    /// Rows filed for the screen while a run had the transcript out. They wait
    /// with it and land when it comes home: a run posts its last events just
    /// before it ends, so the rows that outlive it are filed exactly while
    /// there is nowhere to put them. They land after everything the run
    /// committed — the surface drew them where they happened, and a rebuild
    /// draws them at the end of the turn, which is the one place the two differ.
    pub held_screens: Vec<String>,

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
    pub fn loop_step(&mut self, cut: Option<Cut>, cap: Option<usize>) -> Option<Round> {
        let mut looping = self.looping.take()?;
        if !looping.is_running() {
            self.looping = Some(looping);
            return None;
        }
        let out = looping.step(&self.ctx, cut, cap);
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
        self.tally.on(event);
    }

    /// Start the meter at what the session has already spent, so a resumed
    /// tree does not read as a free run, and pin the rate this run is priced
    /// at: a `/model` answered mid-run does not reprice the turn in flight.
    pub fn seed_meter(&mut self) {
        self.tally.seed(self.totals, self.agent.spec.pricing);
    }

    /// Charge what a run spent to the session, once it has reported it, at the
    /// rate that run was started on — the meter's own, pinned at its seed.
    pub fn charge(&mut self, spent: &llm::stream::Usage) {
        let cost = self.tally.pricing().cost(spent);
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

    /// Take the loop off this lane: what bare `/loop`, a round that ended it,
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

    /// Take it back: the job has finished, or never started. Whatever was
    /// filed for the screen while it was away lands here, in the order it was
    /// drawn — the transcript a rebuild reads has to hold the rows the screen
    /// was shown.
    pub fn return_session(&mut self, mut session: Session) {
        for text in self.held_screens.drain(..) {
            session.push_screen(&text);
        }
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

    /// A row for this lane's screen and nothing else: the tally line a run
    /// ends on, a warning about it. Filed rather than only drawn, so that
    /// rebuilding the screen from the transcript draws it too — and held when
    /// a run has the transcript, since that is when those rows are filed.
    ///
    /// The id comes back for the surface that drew the row: its cursor for what
    /// it has drawn has to move past the entry, or the next adopt draws it a
    /// second time. Nothing comes back for a held row, which is filed later.
    pub fn push_screen(&mut self, text: &str) -> Option<EntryId> {
        match self.session.as_mut() {
            Some(session) => Some(session.push_screen(text)),
            None => {
                self.held_screens.push(text.to_string());
                None
            }
        }
    }
}

impl App {
    pub fn remove_lane(&mut self, at: usize) -> Lane {
        let lane = self.lanes.remove(at);
        if at < self.current || self.current >= self.lanes.len() {
            self.current = self.current.saturating_sub(1);
        }
        lane
    }
    /// Save the transcript. Called after every turn: an interrupted one is
    /// exactly the one worth keeping.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_lane(self.current)
    }
    /// The same for a lane that is not in front: a run that ended out of sight
    /// still has to reach disk, and it is not the screen's turn that decides.
    pub fn save_lane(&self, at: usize) -> anyhow::Result<()> {
        // Away with a run, which saves it itself on the way back.
        let Some(lane) = self.lanes.get(at) else {
            return Ok(());
        };
        let Some(session) = &lane.session else {
            return Ok(());
        };
        self.store.save(
            &lane.id,
            lane.ctx.workspace.root(),
            &lane.agent.spec.model,
            lane.name.as_deref(),
            lane.created,
            session,
        )?;
        Ok(())
    }
    /// Shrink the transcript, or None when there was nothing to shrink — and
    /// likewise when a run has it, which is why `/compact` is refused then.
    ///
    /// Here rather than at each surface: both asked the agent directly, and
    /// both had to reach past the lane for the session to do it.
    pub async fn compact_now(
        &mut self,
        focus: Option<&str>,
    ) -> Option<(agent::Report, llm::stream::Usage)> {
        // One borrow of the lane, two of its fields: they are disjoint, and
        // asking twice would not be.
        let lane = self.lane_mut();
        let session = lane.session.as_mut()?;
        lane.agent.compact_now(session, focus).await
    }
    /// Rewind the conversation to an entry and write the shorter transcript
    /// back.
    ///
    /// The entry decides which of the two it is — the caller cannot get the
    /// pairing wrong, and a menu row that went stale against the transcript
    /// falls through to removing nothing.
    pub fn rewind_to(&mut self, entry: agent::session::EntryId) -> anyhow::Result<Rewound> {
        // A rewind is refused while a run has the transcript, so this is the
        // idle path; without it there is nothing to go back through.
        let Some(session) = &mut self.lane_mut().session else {
            return Ok(Rewound::Nothing);
        };
        let unsent = session.unsent_text(entry);
        let removed = match unsent {
            Some(_) => session.rollback_before(entry),
            None => session.rollback_to(entry),
        };
        if removed == 0 {
            return Ok(Rewound::Nothing);
        }
        self.save()?;
        Ok(match unsent {
            Some(text) => Rewound::Unsent(text),
            None => Rewound::Kept,
        })
    }
    // Become the session this id names: the stamp that dates it, the journal
    // it writes to, and the namespace its spills are filed under.
    //
    // One place because the id and the stamp always travel together and the
    // two callers each set what the other did not — `created` was the one
    // that got missed, and a resumed session was then re-dated on its next
    // save with the stamp of the session it had just left.
    fn becomes(&mut self, id: String, created: u64) {
        self.lane_mut().id = id;
        self.lane_mut().created = created;
        // What `/status` reports is the lane's tally over its settled totals;
        // a new session starts both at nothing rather than the one just left.
        self.lane_mut().totals = Totals::default();
        self.lane_mut().tally = Tally::default();
        // Rows filed for the screen of the session being left, still waiting on
        // its run: they are about a run this one never had.
        self.lane_mut().held_screens.clear();
        let id = self.lane().id.clone();
        let path = self
            .store
            .journal_path(self.lane().ctx.workspace.root(), &id);
        journal::switched(&path, &id);
        // Spills are filed under the session id; a session has to own its own
        // namespace or the one before it keeps swallowing them.
        self.lane_mut().ctx = self
            .lane_mut()
            .ctx
            .clone()
            .with_session(&self.lane_mut().id);
    }
    // Drop the in-memory conversation and open a fresh session under a new
    // id. The old transcript stays on disk.
    //
    // Says nothing: the screen it is rebuilt into is empty, which is the
    // whole of the news, and the id it opened under is the surface's own
    // business — as with a resumed one.
    pub(super) fn fresh_session(&mut self) {
        self.lane_mut().session = Some(Session::default());
        // A name identifies one session; carried over it would name two, which
        // is what `/name` exists to prevent.
        self.lane_mut().name = None;
        self.becomes(session::new_id(), session::now());
    }
    // Take a stored transcript as the running one — entries, name and id.
    // Parting with what is being left is the caller's; they differ on when.
    fn adopt_session(&mut self, stored: Stored) -> Vec<String> {
        let (id, name, created) = (stored.id.clone(), stored.name.clone(), stored.created);
        let session = stored.into_session();
        self.lane_mut().name = name;
        self.lane_mut().session = Some(session);
        self.becomes(id, created);
        // The id is a timestamp with a pid in it — nothing to read, and the
        // transcript coming back on screen already says what was resumed. A
        // name is worth a line, being what the user called it.
        match self.lane_mut().name.as_deref() {
            Some(name) => vec![format!("resumed “{name}”")],
            None => Vec::new(),
        }
    }
    // Open a checkout as a lane of its own, and put it in front.
    //
    // Whole or not at all, like every other path that reads a config: a tree
    // whose config or skills will not resolve leaves the run where it was.
    pub(super) fn open_lane(
        &mut self,
        ws: tools::Workspace,
        worktree: Option<String>,
    ) -> Result<Vec<String>, String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let project = config::load_project(&root).map_err(failed)?;
        let mut resolved = crate::resolve(
            &self.args,
            &ws,
            &self.config,
            &project,
            self.settings.claimed(),
        )
        .map_err(failed)?;

        let (events, inbox) = Lane::channel();
        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let home = self.home(root.clone(), self.lane().agent.spec.model.clone());
        let mut ag = (*self.lane().agent).clone();
        ag.apply(agent::Setup {
            registry: std::mem::take(&mut resolved.registry),
            system: std::mem::take(&mut resolved.system),
            tier: resolved.tier,
            effort: resolved.effort,
            subagent_deadline: resolved.subagent_deadline,
        });
        crate::app::subagent::hang(&mut ag, home, &resolved.standing);

        // Built, not cloned from the lane being left: a `Ctx`'s tables key on
        // absolute paths in one tree, and none of that lane's describe this.
        self.lanes.push(Lane {
            token: crate::app::lane::next_token(),
            agent: std::sync::Arc::new(ag),
            session: Some(Session::default()),
            id: String::new(),
            created: 0,
            name: None,
            totals: Totals::default(),
            tally: Tally::default(),
            held_screens: Vec::new(),

            context: resolved.context,
            standing: resolved.standing,
            ctx: tools::Ctx::new(ws),
            keys: std::sync::Arc::new(resolved.keys),
            commands: std::sync::Arc::new(resolved.commands),
            worktree,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            run: crate::app::lane::Run::Idle,
        });
        self.current = self.lanes.len() - 1;
        self.in_force();

        // Asked with the root the next save will file under, so a tree is found
        // by the same key it was stored by.
        let found = self.store.latest(&root);
        Ok(match found {
            Ok(stored) => self.adopt_session(stored),
            Err(e) => {
                // Nothing recorded for this tree is the ordinary case; an
                // archive that will not load is not, and says so only here.
                tracing::debug!(target: "pi::session", error = %e, "no session to resume in this worktree");
                self.fresh_session();
                Vec::new()
            }
        })
    }
    // The sessions `/resume` can switch to, newest first, the one running
    // now marked.
    pub(super) fn resume_listing(&self) -> Vec<String> {
        let list = self.store.choices(self.lane().ctx.workspace.root());
        if list.is_empty() {
            return vec![
                "no sessions recorded for this workspace".into(),
                "one is saved here at the end of every turn".into(),
            ];
        }
        // What a session is known by is its own label — the name the user gave
        // it, then its first question — never the id.
        let shown: Vec<(bool, String, u64)> = list
            .iter()
            .map(|s| (s.id == self.lane().id, s.label(), s.created))
            .collect();
        let width = shown
            .iter()
            .map(|(_, t, _)| unicode_width::UnicodeWidthStr::width(t.as_str()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = shown
            .iter()
            .map(|(mark, text, created)| {
                format!(
                    "{} {}  {:>10}",
                    if *mark { icons::CURRENT_ITEM } else { " " },
                    crate::store::text::pad(text, width),
                    ago(*created)
                )
            })
            .collect();
        out.push("resume one by typing /resume and Tab".into());
        out
    }
    // Switch to a saved session: its transcript, name and id become this
    // one's, and every further turn extends it. The session being left is
    // saved first, so nothing is lost on the way out.
    //
    // The model in charge does not change — that stays the prompt's decision
    // — so reasoning a different model wrote is demoted the way it is after
    // a `/model` switch.
    pub(super) fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
        // Resuming the session already running is no switch; going through
        // would only zero the totals the bar is mid-way through showing.
        if id == self.lane().id {
            return Ok(Vec::new());
        }
        // The session being left has to survive too, or /resume throws it
        // away. An empty one — just opened, nothing said — has nothing to keep.
        if self
            .lane_mut()
            .session
            .as_ref()
            .is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "resume could not save the leaving session");
        }
        let stored = self.store.load(id).map_err(|e| refused("resume", e))?;
        Ok(self.adopt_session(stored))
    }
}

#[cfg(test)]
mod tests {
    use crate::app::tests::a_lane;
    use llm::model::Pricing;
    use llm::stream::Usage;

    // `/model` is answered at once, a run included. The run it cuts across is
    // still charged at the rate it started on, or the figure `/status` reports
    // would be the price of a model that never ran those tokens.
    #[test]
    fn a_switch_mid_run_does_not_reprice_the_run_in_flight() {
        let cheap = Pricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            ..Default::default()
        };
        let dear = Pricing {
            input_per_mtok: 30.0,
            output_per_mtok: 150.0,
            ..Default::default()
        };

        let mut lane = a_lane("s");
        std::sync::Arc::make_mut(&mut lane.agent).spec.pricing = cheap;
        lane.seed_meter();
        // `/model`: the lane's own rate moves, this run's does not.
        std::sync::Arc::make_mut(&mut lane.agent).spec.pricing = dear;
        lane.charge(&Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        });

        assert_eq!(lane.totals.cost, 3.0 + 15.0);
    }

    // A run holds the transcript for as long as it works, and the rows that
    // outlive a run — the tally line, a warning about it — are filed exactly
    // then. They wait for the transcript rather than being dropped: a rebuild
    // draws what the transcript holds, so a row filed into nothing is a row
    // `/resume` loses.
    #[test]
    fn a_row_filed_while_a_run_holds_the_transcript_lands_with_it() {
        let mut lane = a_lane("s");
        // A lane with a transcript at all: the test lane is built for
        // `/settings` and carries none.
        lane.return_session(agent::session::Session::default());
        let held = lane.take_session().expect("the run has it");

        lane.push_screen("3s · 1.2k/340 · $0.01");
        lane.push_screen("! the reply came back short");
        lane.push_screen("! settled");
        lane.return_session(held);

        let filed: Vec<&str> = lane
            .session()
            .expect("the run gave it back")
            .entries()
            .iter()
            .filter_map(|e| match e {
                agent::session::Entry::Screen { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            filed,
            [
                "3s · 1.2k/340 · $0.01",
                "! the reply came back short",
                "! settled"
            ],
            "in the order they were drawn"
        );
    }
}
