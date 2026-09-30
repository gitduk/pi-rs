//! One checkout being worked in: its run state machine, its transcript, and
//! what the Core does to the lanes it holds — opening one, removing one,
//! starting a session in it, saving it, resuming another.
//!
//! The verbs stay on `Core` because most of them read the store, the config and
//! the settings it holds; a `Lanes` type would be those four passed down one
//! at a time.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

use std::path::Path;
use std::sync::Arc;

use agent::session::{EntryId, Session};
use agent::{Agent, Archive, Event, Steer, Totals};
use subagent::Subagent;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use tool::{Ctx, Tool};

use super::Core;
use crate::core::meter::{Snapshot, Tally};
use crate::core::resolve::Resolved;
use crate::input::commands::ago;
use crate::input::{Rewound, refused};
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
    // Ended out of sight, view already closed: the bar's mark until someone
    // looks, and an Esc-asked rewind that waits for the screen.
    Ended {
        ok: bool,
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

/// One checkout being worked in, in three parts that change at different
/// times: the conversation is swapped whole on `/new` and `/resume`; the
/// checkout and the runner change only through this type's own methods.
pub struct Lane {
    token: u64,
    checkout: Checkout,
    talk: Conversation,
    runner: Runner,
}

// What the tree and the config decide; lives as long as the lane.
struct Checkout {
    // Shared so a run can take it along; `rearm` goes through `make_mut`, so
    // a run in flight keeps the agent it started on.
    agent: Arc<Agent>,
    // The agent's brief, key map, command table and instruction files: one
    // value, swapped whole by `/reload` and by an opened checkout.
    resolved: Arc<Resolved>,
    // Carried across turns: the file locks and edit shifts outlive any one run.
    ctx: Ctx,
    // Which worktree this is, or None in the repository's own checkout.
    worktree: Option<String>,
}

// One session: replaced whole by `/new` and `/resume`, so nothing of the one
// being left survives into the next.
struct Conversation {
    // The transcript, or None while a run has it: lent through `take_session`
    // and given back through `return_session`.
    session: Option<Session>,
    id: String,
    // When this session began. Held rather than read back: going to disk for
    // it made every save parse the whole transcript to recover one integer.
    created: u64,
    // What the user calls this session, if anything.
    name: Option<String>,
    // What this session's finished runs have cost, in and out and in money.
    totals: Totals,
    // What the run in flight has cost so far, seeded from `totals` when a run
    // arms; the status lines read this run alone, `/status` the session.
    tally: Tally,
    // Rows filed for the screen while a run had the transcript out. They land
    // when it comes home, after everything the run committed.
    held_screens: Vec<String>,
}

impl Conversation {
    fn new(id: String, created: u64, session: Option<Session>, name: Option<String>) -> Self {
        Self {
            session,
            id,
            created,
            name,
            totals: Totals::default(),
            tally: Tally::default(),
            held_screens: Vec::new(),
        }
    }
}

// What is running on the lane, and the channel its events arrive on.
struct Runner {
    run: Run,
    // Where this lane's runs post what they are doing. One channel per lane,
    // so an event needs no label to say which screen it belongs on.
    events: UnboundedSender<Event>,
    // The other end, drained by the surface into this lane's view whether or
    // not it is in front.
    inbox: UnboundedReceiver<Event>,
}

/// Arm an agent for one checkout: hang the subagent tool on the brief, put that
/// brief on the agent, and hand back the bundle the lane keeps.
///
/// The one place a lane's brief is assembled, which is what keeps startup,
/// `/reload` and an opened checkout from arming an agent differently. The
/// compactor is not in here: it holds the summarizer's own connection, so the
/// two callers that own one install it on the agent first, and a checkout
/// opened later inherits it by cloning that agent.
///
/// The bundle keeps the brief without the subagent: the tool is offered, not
/// forced, so a re-arm after `/model` must not find the old one holding the name.
pub fn arm(
    agent: &mut Agent,
    resolved: Arc<Resolved>,
    archive: Arc<dyn Archive>,
    retry: agent::Retry,
) -> Arc<Resolved> {
    let subagent = Subagent::new(
        agent,
        resolved.brief.clone(),
        archive,
        &resolved.standing,
        retry,
    );
    // A fresh `Arc`, since the child holds the old one: it runs on the
    // registry it came from, and only the lane's copy carries the tool.
    let mut brief = resolved.brief.clone();
    if subagent.tier().under(resolved.ceiling) {
        Arc::make_mut(&mut brief).registry.offer(Arc::new(subagent));
    }
    agent.apply(brief);
    resolved
}

/// The half of a lane the config decides, for tests that build one directly —
/// the one fixture, since a lane cannot be opened without it.
#[cfg(any(test, feature = "testing"))]
pub fn a_resolved(standing: &str) -> Arc<Resolved> {
    Arc::new(Resolved {
        brief: Arc::new(agent::Briefing {
            registry: toolbox::builtin(),
            system: String::new(),
            effort: llm::request::Effort::Off,
            approver: Arc::new(agent::Ceiling(tool::Tier::Exec)),
            subagent_deadline: None,
        }),
        standing: standing.into(),
        ceiling: tool::Tier::Exec,
        keys: Arc::new(crate::store::keys::Keys::default()),
        commands: Arc::new(Vec::new()),
        notes: Vec::new(),
        context: Vec::new(),
        endpoint: None,
    })
}

/// What a lane is opened with. The rest is the empty state every lane starts
/// in, transcript included: `return_session` is what installs one.
pub struct Opening {
    pub agent: std::sync::Arc<Agent>,
    pub resolved: Arc<Resolved>,
    pub ctx: Ctx,
    pub id: String,
    pub created: u64,
    pub name: Option<String>,
    pub worktree: Option<String>,
}

impl Opening {
    /// The three a lane cannot be without; the rest is left undecided.
    pub fn new(agent: std::sync::Arc<Agent>, resolved: Arc<Resolved>, ctx: Ctx) -> Self {
        Self {
            agent,
            resolved,
            ctx,
            id: String::new(),
            created: 0,
            name: None,
            worktree: None,
        }
    }
}
impl Lane {
    /// A lane before anything has run in it: its own channel, and no transcript
    /// until one is handed back with `return_session`.
    pub fn opened(parts: Opening) -> Self {
        let (events, inbox) = Self::channel();
        Self {
            token: next_token(),
            checkout: Checkout {
                agent: parts.agent,
                resolved: parts.resolved,
                ctx: parts.ctx,
                worktree: parts.worktree,
            },
            talk: Conversation::new(parts.id, parts.created, None, parts.name),
            runner: Runner {
                run: Run::Idle,
                events,
                inbox,
            },
        }
    }

    /// A lane is born with its own channel: nothing else can post to it, and
    /// nothing it posts can land on another screen.
    fn channel() -> (UnboundedSender<Event>, UnboundedReceiver<Event>) {
        unbounded_channel()
    }

    /// The screen is on this lane again: drop the mark of a run that ended out
    /// of sight, and say whether its prompt was asked back.
    ///
    /// Only an ended one: a lane still working must keep its `Running`, or the
    /// token `esc` reaches and the request to unsend go with it.
    pub fn take_ended(&mut self) -> Option<bool> {
        match self.runner.run {
            Run::Ended { unsend, .. } => {
                self.runner.run = Run::Idle;
                Some(unsend)
            }
            _ => None,
        }
    }

    /// A job has given this lane's transcript back. The one place `Running`
    /// ends: a lane left in it queues every later prompt and never drains.
    pub fn finish(&mut self) -> Handback {
        match std::mem::replace(&mut self.runner.run, Run::Idle) {
            Run::Running { unsend, steer, .. } => Handback {
                unsend,
                unheard: steer.map(|s| s.take()).unwrap_or_default(),
            },
            _ => Handback::default(),
        }
    }

    /// Where a line typed mid-run goes, while a run is there to hear one.
    pub fn steer(&self) -> Option<&Steer> {
        match &self.runner.run {
            Run::Running { steer, .. } => steer.as_ref(),
            _ => None,
        }
    }

    /// Whether a run has this lane's transcript right now.
    pub fn is_running(&self) -> bool {
        matches!(self.runner.run, Run::Running { .. })
    }

    // ------------------------------------------------------- what a run does

    /// A run has taken this lane. `steer` is the mailbox it hears at the next
    /// turn boundary; a job that calls no model has none to hear from.
    pub fn begin(&mut self, cancel: CancellationToken, steer: Option<Steer>) {
        self.runner.run = Run::Running {
            cancel,
            steer,
            unsend: false,
        };
    }

    /// Ask the run under way to stop, and say whether there was one. `unsend`
    /// also takes the prompt back, which is what `esc` means.
    pub fn stop(&mut self, unsend: bool) -> bool {
        match &mut self.runner.run {
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
        if let Run::Running { cancel, .. } = &self.runner.run {
            cancel.cancel();
        }
    }

    /// A run ended out of sight: whether it went well, and whether its prompt
    /// was asked back. The other half of [`Lane::take_ended`].
    pub fn end(&mut self, ok: bool, unsend: bool) {
        self.runner.run = Run::Ended { ok, unsend };
    }

    /// Where this lane's run stands, for a surface that only draws it.
    pub fn run(&self) -> &Run {
        &self.runner.run
    }

    // ------------------------------------------------------- the checkout

    /// Change the agent, then put the brief back on it under `resolved`. One
    /// call: a brief armed on an agent other than the one in force is stale.
    pub fn rearm(
        &mut self,
        resolved: Arc<Resolved>,
        archive: Arc<dyn Archive>,
        retry: agent::Retry,
        change: impl FnOnce(&mut Agent),
    ) {
        let agent = Arc::make_mut(&mut self.checkout.agent);
        change(agent);
        self.checkout.resolved = arm(agent, resolved, archive, retry);
    }

    /// What this checkout and the config decide.
    pub fn resolved(&self) -> &Arc<Resolved> {
        &self.checkout.resolved
    }

    /// The tree this lane's tools are confined to.
    pub fn workspace(&self) -> &tool::Workspace {
        &self.checkout.ctx.workspace
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
        self.talk.tally.snapshot(
            model,
            self.checkout.worktree.as_deref(),
            started,
            queued + self.steer().map_or(0, agent::Steer::len),
        )
    }

    /// The checkout this lane works in, when it is not the repository's own.
    pub fn worktree(&self) -> Option<&str> {
        self.checkout.worktree.as_deref()
    }

    /// The transcript, or `None` while a run has it. Both readers answer that
    /// way: the states a lane can be in are named (`NO_TRANSCRIPT`,
    /// `NOTHING_TO_REWIND`) rather than one of them being a panic.
    pub fn session(&self) -> Option<&Session> {
        self.talk.session.as_ref()
    }

    /// The ask nobody has answered, when a run left one open.
    pub fn last_ask(&self) -> Option<EntryId> {
        self.talk.session.as_ref().and_then(Session::last_ask)
    }

    /// The checkout this lane works in.
    pub fn root(&self) -> &Path {
        self.checkout.ctx.workspace.root()
    }

    /// The checkout's context, for what measures the tree between turns.
    pub fn ctx(&self) -> &Ctx {
        &self.checkout.ctx
    }

    /// The context a job runs with: this lane's, with the job's own way out.
    pub fn ctx_for(&self, cancel: CancellationToken) -> Ctx {
        self.checkout.ctx.clone().with_cancel(cancel)
    }

    /// What the views of this lane are keyed by.
    pub fn token(&self) -> u64 {
        self.token
    }

    /// The model this lane's runs ask for.
    pub fn model(&self) -> &str {
        &self.checkout.agent.spec().model
    }

    /// The agent these runs go through, for what only it knows.
    pub fn agent(&self) -> &Agent {
        &self.checkout.agent
    }

    /// What runs of this lane report as they go.
    pub fn sender(&self) -> &UnboundedSender<Event> {
        &self.runner.events
    }

    /// What this lane has been told and not yet heard.
    pub fn inbox(&mut self) -> &mut UnboundedReceiver<Event> {
        &mut self.runner.inbox
    }

    /// The id this lane's session is saved under.
    pub fn id(&self) -> &str {
        &self.talk.id
    }

    pub fn set_name(&mut self, name: Option<String>) {
        self.talk.name = name;
    }

    /// What the run in flight has cost, over the session's settled totals.
    pub fn tally(&self) -> &Tally {
        &self.talk.tally
    }

    // ------------------------------------------------------------- the meter

    /// Fold an event into the meter. The loop's facts arrive here and nowhere
    /// else: what a run has spent is only known from them.
    pub fn note(&mut self, event: &Event) {
        self.talk.tally.on(event);
    }

    /// Start the meter at what the session has already spent, so a resumed
    /// tree does not read as a free run, and pin the rate this run is priced
    /// at: a `/model` answered mid-run does not reprice the turn in flight.
    pub fn seed_meter(&mut self) {
        self.talk
            .tally
            .seed(self.talk.totals, self.checkout.agent.spec().pricing);
    }

    /// Charge what a run spent to the session, once it has reported it, at the
    /// rate that run was started on — the meter's own, pinned at its seed.
    pub fn charge(&mut self, spent: &llm::stream::Usage) {
        let cost = self.talk.tally.pricing().cost(spent);
        self.talk.totals.add(spent, cost);
    }

    /// The same, from the run itself: its own word when it has one, and the
    /// meter's reading of the turn in flight when it was cut short before
    /// pricing it.
    pub fn charge_run(&mut self, out: &Result<llm::stream::Usage, agent::AgentError>) {
        let spent = match out {
            Ok(usage) => *usage,
            Err(_) => self.talk.tally.run_spend().usage,
        };
        self.charge(&spent);
    }

    // -------------------------------------------------------- the transcript

    /// Lend the transcript to a job for the length of its run.
    pub fn take_session(&mut self) -> Option<Session> {
        self.talk.session.take()
    }

    /// Take it back: the job has finished, or never started. Whatever was
    /// filed for the screen while it was away lands here, in the order it was
    /// drawn — the transcript a rebuild reads has to hold the rows the screen
    /// was shown.
    pub fn return_session(&mut self, mut session: Session) {
        assert!(
            self.talk.session.is_none(),
            "a lane holds one transcript: a second would drop the first"
        );
        for text in self.talk.held_screens.drain(..) {
            session.push_screen(&text);
        }
        self.talk.session = Some(session);
    }

    /// What a run's ending should leave in the transcript.
    pub fn note_outcome(&mut self, out: &Result<llm::stream::Usage, agent::AgentError>) {
        if let Some(session) = self.talk.session.as_mut() {
            session.note_outcome(out);
        }
    }

    /// A line the run filed rather than the model: a command's note, read back
    /// with the rest of the transcript. Nothing to file when there is none.
    pub fn push_note(&mut self, note: &str) {
        if let Some(session) = self.talk.session.as_mut() {
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
        match self.talk.session.as_mut() {
            Some(session) => Some(session.push_screen(text)),
            None => {
                self.talk.held_screens.push(text.to_string());
                None
            }
        }
    }
}

// What tests stage a lane with, or read back off it, that nothing else may.
#[cfg(any(test, feature = "testing"))]
impl Lane {
    pub fn set_worktree(&mut self, name: Option<String>) {
        self.checkout.worktree = name;
    }

    pub fn ctx_mut(&mut self) -> &mut Ctx {
        &mut self.checkout.ctx
    }

    pub fn totals(&self) -> &Totals {
        &self.talk.totals
    }

    pub fn held_screens(&self) -> &[String] {
        &self.talk.held_screens
    }
}

impl Core {
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
        let Some(session) = &lane.talk.session else {
            return Ok(());
        };
        self.store.save(
            &lane.talk.id,
            lane.checkout.ctx.workspace.root(),
            &lane.checkout.agent.spec().model,
            lane.talk.name.as_deref(),
            lane.talk.created,
            session,
        )?;
        Ok(())
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
        let Some(session) = &mut self.lane_mut().talk.session else {
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
    // Become the session `talk` holds: the journal it writes to and the
    // namespace its spills are filed under follow it. Replaced whole, so no
    // part of the session being left — its bill, its held rows — carries over.
    fn becomes(&mut self, talk: Conversation) {
        let lane = self.lane_mut();
        lane.talk = talk;
        lane.checkout.ctx = lane
            .checkout
            .ctx
            .clone()
            .with_session(&lane.talk.id, crate::store::spill_root());
        let path = self
            .store
            .journal_path(self.lane().root(), &self.lane().talk.id);
        journal::switched(&path, &self.lane().talk.id);
    }
    // Drop the in-memory conversation and open a fresh session under a new
    // id, unnamed: a name identifies one session. The old transcript stays on
    // disk.
    //
    // Says nothing: the screen it is rebuilt into is empty, which is the
    // whole of the news, and the id it opened under is the surface's own
    // business — as with a resumed one.
    pub(super) fn fresh_session(&mut self) {
        self.becomes(Conversation::new(
            session::new_id(),
            session::now(),
            Some(Session::default()),
            None,
        ));
    }
    // Take a stored transcript as the running one — entries, name and id.
    // Parting with what is being left is the caller's; they differ on when.
    fn adopt_session(&mut self, stored: Stored) -> Vec<String> {
        let (id, name, created) = (stored.id.clone(), stored.name.clone(), stored.created);
        self.becomes(Conversation::new(
            id,
            created,
            Some(stored.into_session()),
            name,
        ));
        // The id is a timestamp with a pid in it — nothing to read, and the
        // transcript coming back on screen already says what was resumed. A
        // name is worth a line, being what the user called it.
        match self.lane().talk.name.as_deref() {
            Some(name) => vec![format!("resumed “{name}”")],
            None => Vec::new(),
        }
    }
    // Open a checkout as a lane of its own, and put it in front.
    //
    // Whole or not at all, like every other path that reads a config: a tree
    // whose skills will not resolve leaves the run where it was. The config is
    // the one in force, not re-read for this tree: `/reload` does that.
    pub(super) fn open_lane(
        &mut self,
        ws: tool::Workspace,
        worktree: Option<String>,
    ) -> Result<Vec<String>, String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let resolved =
            crate::core::resolve::resolve(&self.pinned, &ws, &self.config, &self.settings)
                .map_err(failed)?;

        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let archive = self.archive(
            root.clone(),
            self.lane().checkout.agent.spec().model.clone(),
        );
        let mut ag = (*self.lane().checkout.agent).clone();
        let resolved = arm(&mut ag, Arc::new(resolved), archive, self.config.retry());

        // Built, not cloned from the lane being left: a `Ctx`'s tables key on
        // absolute paths in one tree, and none of that lane's describe this.
        let ctx = tool::Ctx::new(ws);
        self.lanes.push(Lane::opened(Opening {
            worktree,
            ..Opening::new(Arc::new(ag), resolved, ctx)
        }));
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
        let list = self
            .store
            .choices(self.lane().checkout.ctx.workspace.root());
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
            .map(|s| (s.id == self.lane().talk.id, s.label(), s.created))
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
                    crate::text::pad(text, width),
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
        //
        // Unless there is nothing there to resume: a job that panicked and could
        // not read its transcript back leaves the lane's own id naming a session
        // the archive does not hold, and "no switch" would answer that with a
        // screen that never comes back.
        if id == self.lane().talk.id {
            if self.lane().session().is_none() {
                return Err(
                    "this checkout's transcript is gone — /new starts a session here".to_string(),
                );
            }
            return Ok(Vec::new());
        }
        // The session being left has to survive too, or /resume throws it
        // away. An empty one — just opened, nothing said — has nothing to keep.
        if self
            .lane()
            .talk
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
    use crate::core::tests::a_lane;
    use llm::model::Pricing;
    use llm::stream::Usage;

    // A lane's model is one value behind two `Arc`s; a test that reprices it
    // takes the copies the same way `/model` does.
    fn price(lane: &mut crate::core::lane::Lane, pricing: Pricing) {
        let agent = std::sync::Arc::make_mut(&mut lane.checkout.agent);
        std::sync::Arc::make_mut(&mut agent.model).spec.pricing = pricing;
    }

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
        price(&mut lane, cheap);
        lane.seed_meter();
        // `/model`: the lane's own rate moves, this run's does not.
        price(&mut lane, dear);
        lane.charge(&Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        });

        assert_eq!(lane.talk.totals.cost, 3.0 + 15.0);
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
