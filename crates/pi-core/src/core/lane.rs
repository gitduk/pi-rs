//! One checkout being worked in: its run state machine, its transcript, and
//! what the Core does to the lanes it holds.
//!
//! The verbs stay on `Core` since most read the store, config and settings
//! it holds.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_token() -> u64 {
    NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
}

use std::path::Path;
use std::sync::Arc;

use agent::session::{EntryId, Session};
use agent::{Agent, Archive, Event, NoteOutcome as _, Totals};
use subagent::Subagent;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use tool::{Ctx, Tool};

use super::Core;
use crate::core::meter::{Snapshot, Tally};
use crate::core::resolve::Resolved;
use crate::input::commands::session_facts;
use crate::input::{Rewound, Step, refused};
use pi_store::icons;
use pi_store::journal;
use pi_store::session::{self, Stored};
use pi_store::settings::Settings;

/// Where this lane's run stands.
///
/// One value rather than a flag beside an outcome: a lane is idle, or running,
/// or holding the end of a run nobody has seen — never two of those, and the
/// compiler is what should say so.
pub enum Run {
    // Nothing running, nothing waiting to be looked at.
    Idle,
    Running {
        // What a stop cancels, and only for the lane in front.
        cancel: CancellationToken,
        // A stop caught the prompt on its way out: end the run, then unsend it.
        // Set while the run works, acted on when it ends.
        unsend: bool,
    },
    // Ended out of sight: marked until someone looks, and a rewind asked
    // with the stop that waits for the lane to be in front.
    Ended {
        ok: bool,
        // The stop asked for the prompt back while this was still running.
        unsend: bool,
    },
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
    // The agent's brief, key map, command table and prompt files: one value,
    // swapped whole by a reload, an opened checkout and a prompt re-read.
    resolved: Arc<Resolved>,
    // Carried across turns: the file locks and edit shifts outlive any one run.
    ctx: Ctx,
    // Which worktree this is, or None in the repository's own checkout.
    worktree: Option<String>,
    // The session and its newest compaction when the prompt's files were
    // last looked at: either moving lets them be read again unnoticed.
    read_at: Option<(String, Option<EntryId>)>,
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
    // Keeps another pi from saving over this session while it is open here.
    _claim: Option<session::Claim>,
    // What the user calls this session, if anything.
    name: Option<String>,
    // What this session's finished runs have cost, in and out and in money.
    totals: Totals,
    // What the run in flight has cost so far, seeded from `totals` when a run
    // arms; the status lines read this run alone, `/status` the session.
    tally: Tally,
    // Rows and notes filed while a run had the transcript out. They land
    // when it comes home, after everything the run committed.
    held: Vec<Held>,
    // Why the last save failed; cleared by the next one that lands.
    unsaved: Option<String>,
}

enum Held {
    Screen(String),
    Note(String),
}

impl Conversation {
    // Held under another root now; the transcript and its figures stay.
    fn reclaim(&mut self, claim: Option<session::Claim>) {
        self._claim = claim;
    }

    fn new(
        id: String,
        created: u64,
        session: Option<Session>,
        name: Option<String>,
        claim: Option<session::Claim>,
    ) -> Self {
        Self {
            session,
            id,
            created,
            _claim: claim,
            name,
            totals: Totals::default(),
            tally: Tally::default(),
            held: Vec::new(),
            unsaved: None,
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

/// Arm an agent for one checkout: hang the subagent tool on the brief, put
/// that brief on the agent, and hand back the bundle the lane keeps.
///
/// One place a brief is assembled, so startup, a reload and an opened
/// checkout arm an agent the same way.
pub fn arm(
    agent: &mut Agent,
    resolved: Arc<Resolved>,
    archive: Arc<dyn Archive>,
    retry: agent::Retry,
    jobs: Option<Arc<crate::driver::jobs::Table>>,
) -> Arc<Resolved> {
    let mut subagent = Subagent::prompted(
        agent,
        resolved.brief.clone(),
        archive,
        resolved.prompt_files.prompt("subagent"),
        &resolved.standing,
        retry,
    );
    if let Some(deadline) = resolved.subagent_deadline {
        subagent = subagent.with_deadline(deadline);
    }
    // Its answer comes back as a job's does, so only with somewhere to keep it.
    if jobs.is_some() {
        subagent = subagent.with_background();
    }
    // A fresh `Arc`, since the child holds the old one: it runs on the
    // registry it came from, and only the lane's copy carries the tool.
    let mut brief = resolved.brief.clone();
    if subagent.tier().under(resolved.ceiling) {
        Arc::make_mut(&mut brief).registry.offer(Arc::new(subagent));
    }
    // The lane's copy only: a child never hears the user, whose words are
    // the only ones memory keeps.
    if resolved.remembers && tool::Tier::Write.under(resolved.ceiling) {
        Arc::make_mut(&mut brief)
            .registry
            .offer(Arc::new(crate::core::memory::Remember::new(
                pi_store::memory::Memory::default(),
            )));
    }
    // The lane's copy only, as with the subagent: a child cannot reach the
    // lane its prompt would come back to.
    if let Some(jobs) = jobs
        && tool::Tier::Exec.under(resolved.ceiling)
    {
        let patch = Arc::make_mut(&mut brief);
        patch
            .registry
            .offer(Arc::new(crate::driver::jobs::Jobs::new(jobs)));
        // The same shell, now able to leave its turn; the child keeps the
        // plain one, having nowhere to keep a job.
        if patch.registry.get(toolbox::bash::Bash::NAME).is_some() {
            let registry = std::mem::take(&mut patch.registry).without(toolbox::bash::Bash::NAME);
            patch.registry = registry;
            patch
                .registry
                .offer(Arc::new(toolbox::bash::WithBackground));
        }
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
            summarize: agent::compaction::SUMMARIZE.to_string(),
            effort: llm::request::Effort::Off,
            approver: Arc::new(agent::Ceiling(tool::Tier::Exec)),
            hooks: Arc::new(agent::NoHooks),
        }),
        subagent_deadline: None,
        standing: standing.into(),
        ceiling: tool::Tier::Exec,
        keys: Arc::new(pi_store::keys::Keys::default()),
        commands: Arc::new(Vec::new()),
        watched: Vec::new(),
        prompt_files: Default::default(),
        shelf: None,
        shelf_seen: 0,
        notes: Vec::new(),
        instructions: Vec::new(),
        memory: Vec::new(),
        remembers: false,
        system: None,
        prompts: Vec::new(),
        endpoint: None,
        mcp: Vec::new(),
        project: Vec::new(),
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
    /// The hold on `id`, when the opener already took it.
    pub claim: Option<session::Claim>,
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
            claim: None,
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
                read_at: None,
            },
            talk: Conversation::new(parts.id, parts.created, None, parts.name, parts.claim),
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
    /// token a stop reaches and the request to unsend go with it.
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
    /// Says whether the stop asked for the prompt back.
    pub fn finish(&mut self) -> bool {
        match std::mem::replace(&mut self.runner.run, Run::Idle) {
            Run::Running { unsend, .. } => unsend,
            _ => false,
        }
    }

    /// Whether a run has this lane's transcript right now.
    pub fn is_running(&self) -> bool {
        matches!(self.runner.run, Run::Running { .. })
    }

    /// A run has taken this lane.
    pub fn begin(&mut self, cancel: CancellationToken) {
        self.runner.run = Run::Running {
            cancel,
            unsend: false,
        };
    }

    /// Ask the run under way to stop, and say whether there was one. `unsend`
    /// also takes the prompt back, as the usual stop does.
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

    /// Change the agent, then put the brief back on it under `resolved`. One
    /// call: a brief armed on an agent other than the one in force is stale.
    pub fn rearm(
        &mut self,
        resolved: Arc<Resolved>,
        archive: Arc<dyn Archive>,
        retry: agent::Retry,
        jobs: Option<Arc<crate::driver::jobs::Table>>,
        change: impl FnOnce(&mut Agent),
    ) {
        let agent = Arc::make_mut(&mut self.checkout.agent);
        change(agent);
        self.checkout.resolved = arm(agent, resolved, archive, retry, jobs);
    }

    /// Replace what was decided where the agent holds none of it — the
    /// command table — without re-arming anything.
    pub(super) fn keep(&mut self, resolved: Arc<Resolved>) {
        self.checkout.resolved = resolved;
    }

    /// Whether the session moved or was compacted since the last look; the
    /// first look only marks where things stand.
    pub(super) fn prompt_files_due(&mut self) -> bool {
        let Some(session) = &self.talk.session else {
            return false;
        };
        let compacted = session.entries().iter().rev().find_map(|e| match e {
            agent::session::Entry::Compaction { id, .. } => Some(*id),
            _ => None,
        });
        let now = Some((self.talk.id.clone(), compacted));
        let was = std::mem::replace(&mut self.checkout.read_at, now.clone());
        was.is_some() && was != now
    }

    /// What this checkout and the config decide.
    pub fn resolved(&self) -> &Arc<Resolved> {
        &self.checkout.resolved
    }

    /// The tree this lane's tools are confined to.
    pub fn workspace(&self) -> &tool::Workspace {
        &self.checkout.ctx.workspace
    }

    /// What a surface draws this lane from: where it is, what it is doing, and
    /// what it is doing it under. One call rather than four reaches into it.
    pub fn snapshot(
        &self,
        model: &str,
        started: Option<std::time::Duration>,
        queued: usize,
    ) -> Snapshot {
        self.talk
            .tally
            .snapshot(model, self.checkout.worktree.as_deref(), started, queued)
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

    /// The context runs start from. A run already going keeps its own copy.
    pub fn ctx_mut(&mut self) -> &mut Ctx {
        &mut self.checkout.ctx
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

    /// Why the transcript on disk is behind this one, while it is.
    pub fn unsaved(&self) -> Option<&str> {
        self.talk.unsaved.as_deref()
    }

    /// What the run in flight has cost, over the session's settled totals.
    pub fn tally(&self) -> &Tally {
        &self.talk.tally
    }

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
        for held in self.talk.held.drain(..) {
            match held {
                Held::Screen(text) => session.push_screen(text),
                Held::Note(text) => session.push_note(text),
            };
        }
        self.talk.session = Some(session);
    }

    /// What a run's ending should leave in the transcript.
    pub fn note_outcome(&mut self, out: &Result<llm::stream::Usage, agent::AgentError>) {
        if let Some(session) = self.talk.session.as_mut() {
            session.note_outcome(out);
        }
    }

    /// A line pi filed rather than the model: a command's note, read back
    /// with the rest of the transcript; held when a run has the transcript.
    ///
    /// Returns the id as `push_screen` does; `None` for a held note.
    pub fn push_note(&mut self, note: &str) -> Option<EntryId> {
        match self.talk.session.as_mut() {
            Some(session) => Some(session.push_note(note)),
            None => {
                self.talk.held.push(Held::Note(note.to_string()));
                None
            }
        }
    }

    /// A row for this lane's screen and nothing else, filed so a rebuild
    /// from the transcript draws it too; held when a run has the transcript.
    ///
    /// Returns the id so a surface can tell it from what it has yet to show;
    /// `None` for a held row, filed later.
    pub fn push_screen(&mut self, text: &str) -> Option<EntryId> {
        match self.talk.session.as_mut() {
            Some(session) => Some(session.push_screen(text)),
            None => {
                self.talk.held.push(Held::Screen(text.to_string()));
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

    pub fn totals(&self) -> &Totals {
        &self.talk.totals
    }

    pub fn held(&self) -> usize {
        self.talk.held.len()
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
    pub fn save(&mut self) -> anyhow::Result<()> {
        self.save_lane(self.current)
    }
    /// The same for a lane that is not in front: a run that ended out of sight
    /// still has to reach disk, and it is not the screen's turn that decides.
    pub fn save_lane(&mut self, at: usize) -> anyhow::Result<()> {
        // Away with a run, which saves it itself on the way back.
        let Some(lane) = self.lanes.get_mut(at) else {
            return Ok(());
        };
        let Some(session) = &lane.talk.session else {
            return Ok(());
        };
        let saved = self.store.save(
            &lane.talk.id,
            lane.checkout.ctx.workspace.root(),
            &lane.checkout.agent.spec().model,
            lane.talk.name.as_deref(),
            lane.talk.created,
            session,
        );
        lane.talk.unsaved = saved.as_ref().err().map(|e| format!("{e:#}"));
        saved?;
        Ok(())
    }
    /// Rewind the conversation to an entry and write the shorter transcript
    /// back.
    ///
    /// The entry decides which of the two it is; a stale pick falls
    /// through to removing nothing.
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
    // Become the session `talk` holds: the journal and spill namespace
    // follow it, replaced whole — nothing of the old session carries over.
    fn becomes(&mut self, talk: Conversation) {
        let lane = self.lane_mut();
        lane.talk = talk;
        lane.checkout.ctx = lane
            .checkout
            .ctx
            .clone()
            .with_session(&lane.talk.id, pi_store::spill_root());
        let path = self
            .store
            .journal_path(self.lane().root(), &self.lane().talk.id);
        journal::switched(&path, &self.lane().talk.id);
    }
    // Fresh, unnamed session; the old transcript stays on disk. Says nothing:
    // the rebuilt screen is already empty and the id is the surface's business.
    pub(super) fn fresh_session(&mut self) {
        let id = session::new_id();
        let claim = self.store.claim(self.lane().root(), &id).ok();
        self.becomes(Conversation::new(
            id,
            session::now(),
            Some(Session::default()),
            None,
            claim,
        ));
    }
    // Take a stored transcript as the running one — entries, name and id.
    // Parting with what is being left is the caller's; they differ on when.
    fn adopt_session(&mut self, stored: Stored, claim: session::Claim) -> Vec<String> {
        let (id, name, created) = (stored.id.clone(), stored.name.clone(), stored.created);
        self.becomes(Conversation::new(
            id,
            created,
            Some(stored.into_session()),
            name,
            Some(claim),
        ));
        // The id (timestamp+pid) is nothing to read; only a name — what the
        // user called it — is worth a line.
        match self.lane().talk.name.as_deref() {
            Some(name) => vec![format!("resumed “{name}”")],
            None => Vec::new(),
        }
    }
    /// `/cd`: the conversation in front carries on in `dir`, and all the tree
    /// decides is decided again there; whole or not at all.
    pub(super) fn move_to(&mut self, dir: &str) -> Result<Step, String> {
        let from = self.lane().root().to_path_buf();
        let failed = |e: anyhow::Error| format!("not moved — {}", refused("cd", e));
        let target = match dir.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                let home = crate::core::instructions::home()
                    .ok_or_else(|| failed(anyhow::anyhow!("$HOME is not set")))?;
                home.join(rest.trim_start_matches('/'))
            }
            _ => from.join(dir),
        };
        let settings = Settings::load(self.pinned.config.as_deref(), &target).map_err(&failed)?;
        let config = settings.config().map_err(&failed)?;
        let ws = tool::Workspace::new(&target)
            .and_then(|ws| ws.with_write_roots(&config.writable()))
            .map_err(|e| failed(anyhow::anyhow!("{}: {e}", target.display())))?;
        let to = ws.root().to_path_buf();
        if to == from {
            return Ok(Step::Notice(format!("already in {}", to.display())));
        }
        if self.lane_at(&to).is_some() {
            return Err(format!("{} is open in another lane already", to.display()));
        }
        let Some(empty) = self.lane().session().map(|s| s.is_empty()) else {
            return Err("a run has the transcript — wait for it to end".into());
        };
        // Filed under the root it was written in before anything moves.
        if !empty {
            self.save().map_err(&failed)?;
        }

        let worktree = crate::core::worktree::list(&to).ok().and_then(|trees| {
            let tree = trees
                .into_iter()
                .find(|t| !t.main && t.path.canonicalize().ok().as_ref() == Some(&to))?;
            Some(tree.name)
        });
        let mut said = self.adopt(config, ws, Some(settings), None)?;
        // Built, not kept: a `Ctx`'s tables key on absolute paths in one tree.
        let id = self.lane().talk.id.clone();
        let ws = self.lane().ctx().workspace.clone();
        *self.lane_mut().ctx_mut() = Ctx::new(ws).with_session(&id, pi_store::spill_root());
        self.lane_mut().checkout.worktree = worktree;

        // The record follows under the same id: one rename takes the
        // transcript, its journal and its subagents along.
        self.lane_mut().talk.reclaim(None);
        let claim = if self.store.path_of(&from, &id).is_file() {
            match self.store.relocate(&to, &id) {
                Ok(claim) => Some(claim),
                Err(e) => {
                    tracing::warn!(target: "pi::session", error = %e, "the session record did not move");
                    said.push(format!("its record stays under {} — {e:#}", from.display()));
                    self.store.claim(&to, &id).ok()
                }
            }
        } else {
            self.store.claim(&to, &id).ok()
        };
        self.lane_mut().talk.reclaim(claim);
        journal::switched(&self.store.journal_path(&to, &id), &id);
        if let Some(jobs) = &self.jobs {
            jobs.rehome(&from, &to);
        }
        // Earlier calls name paths against the old root; nothing rewrites them.
        self.lane_mut().push_note(&format!(
            "The workspace moved from {} to {}: relative paths earlier in this \
             conversation are relative to {}.",
            from.display(),
            to.display(),
            from.display()
        ));
        said.insert(0, format!("now in {}", to.display()));
        Ok(Step::Moved(crate::input::notice(said)))
    }
    // Open a checkout as a new lane, in front; whole or not at all — a tree
    // whose skills won't resolve leaves the run where it was.
    pub(super) fn open_lane(
        &mut self,
        ws: tool::Workspace,
        worktree: Option<String>,
    ) -> Result<Vec<String>, String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let resolved =
            crate::core::resolve::resolve(&self.pinned, &ws, &self.config, &self.settings, None)
                .map_err(failed)?;

        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let archive = self.archive(
            root.clone(),
            self.lane().checkout.agent.spec().model.clone(),
        );
        let mut ag = (*self.lane().checkout.agent).clone();
        let resolved = arm(
            &mut ag,
            Arc::new(resolved),
            archive,
            crate::core::retry(&self.config),
            self.jobs.clone(),
        );

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
        // One open in another pi stays there; this lane starts its own.
        let found = self.store.take_latest(&root);
        Ok(match found {
            Ok((stored, claim)) => self.adopt_session(stored, claim),
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
        let width = list
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.label().as_str()))
            .max()
            .unwrap_or(0);
        let facts = session_facts(&list.iter().collect::<Vec<_>>());
        let mut out: Vec<String> = list
            .iter()
            .zip(facts)
            .map(|(s, facts)| {
                let mark = if s.id == self.lane().talk.id {
                    icons::CURRENT_ITEM
                } else {
                    " "
                };
                format!("{mark} {}  {facts}", pi_store::text::pad(&s.label(), width))
            })
            .collect();
        out.push("/resume <id> switches to one".into());
        out
    }
    // Switches transcript, name and id; the model does not change — reasoning
    // from a different model is demoted the same way as after `/model`.
    pub(super) fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
        // Same id is normally a no-op (else totals would zero); unless the
        // transcript itself is gone (a panicked job), which must be reported.
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
        let (stored, claim) = self
            .store
            .take(self.lane().root(), id)
            .map_err(|e| refused("resume", e))?;
        Ok(self.adopt_session(stored, claim))
    }
}

#[cfg(test)]
mod tests {
    use crate::core::tests::{Recording, a_lane, a_repl};
    use llm::model::Pricing;
    use llm::stream::Usage;

    // A lane's model is one value behind two `Arc`s; a test that reprices it
    // takes the copies the same way `/model` does.
    fn price(lane: &mut crate::core::lane::Lane, pricing: Pricing) {
        let agent = std::sync::Arc::make_mut(&mut lane.checkout.agent);
        std::sync::Arc::make_mut(&mut agent.model).spec.pricing = pricing;
    }

    // `/model` applies at once, but a run in flight stays charged at the
    // rate it started — else `/status` prices tokens the new model never ran.
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

    // Screen rows filed while a run holds the transcript wait rather than
    // drop — a rebuild reads only the transcript; `/resume` would lose them.
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

    // The conversation and its record land in the new tree;
    // the model is told the old paths were the old root's.
    #[tokio::test]
    async fn a_move_carries_the_conversation_and_its_record_to_the_new_root() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().canonicalize().unwrap();
        let to = from.join("new_project");
        std::fs::create_dir(&to).unwrap();
        let mut core = a_repl(&from, std::sync::Arc::new(Recording::default()), "model-a");
        core.lane_mut().push_note("said before the move");
        core.save().unwrap();

        let step = core.move_to("new_project").unwrap();
        assert!(matches!(step, crate::input::Step::Moved(_)));
        assert_eq!(core.lane().root(), to);
        assert!(core.store.path_of(&to, "s1").is_file(), "record moved");
        assert!(!core.store.path_of(&from, "s1").exists(), "kept once");
        let session = core.lane().session().unwrap();
        let text = serde_json::to_string(session.entries()).unwrap();
        assert!(text.contains("said before the move"), "transcript kept");
        assert!(text.contains("moved from"), "the model is told");
        assert!(core.store.claim(&to, "s1").is_err(), "still held here");

        assert!(core.move_to("nowhere").is_err(), "a missing dir is refused");
        assert_eq!(core.lane().root(), to, "and changes nothing");
    }
}
