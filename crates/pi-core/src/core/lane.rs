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
use std::sync::{Arc, Mutex};

use agent::session::{EntryId, Session};
use agent::{Agent, Archive, Event, Totals};
use subagent::Subagent;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use tool::Ctx;

use super::Core;
use crate::core::meter::{Snapshot, Tally};
use crate::core::resolve::Resolved;
use crate::input::{Rewound, Step, refused};
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
    read_at: Option<ReadAt>,
}

// The session, and its newest compaction, as the prompt's files were read.
pub(super) type ReadAt = (String, Option<EntryId>);

// One session: replaced whole by `/new` and `/resume`, so nothing of the one
// being left survives into the next.
struct Conversation {
    transcript: Transcript,
    id: String,
    // When this session began. Held rather than read back: going to disk for
    // it made every save parse the whole transcript to recover one integer.
    created: u64,
    // Keeps another pi from saving over this session while it is open here.
    _claim: Option<session::Claim>,
    // What the user calls this session, if anything.
    name: Option<String>,
    // What the run in flight has cost so far, seeded when a run
    // arms; the status lines read this run alone, `/status` the session.
    tally: Tally,
    // Why the last save failed; cleared by the next one that lands.
    unsaved: Option<String>,
    // Rows and notes filed since the last save: turns save themselves, but
    // what lands between them waits for the next turn, or for leaving.
    filed: bool,
}

// Where the transcript is: lent through `take_session`, given back through
// `return_session`.
enum Transcript {
    Home(Session),
    // A run has it. Rows and notes filed meanwhile land when it comes home,
    // after everything the run committed; a note the run takes on its way
    // to a request lands there instead.
    Lent(Arc<Mutex<Vec<Held>>>),
}

impl Transcript {
    fn home(&self) -> Option<&Session> {
        match self {
            Transcript::Home(session) => Some(session),
            Transcript::Lent(_) => None,
        }
    }

    fn home_mut(&mut self) -> Option<&mut Session> {
        match self {
            Transcript::Home(session) => Some(session),
            Transcript::Lent(_) => None,
        }
    }
}

enum Held {
    Plain(String),
    Notice(String),
    Tally(String),
    Note(String),
}

impl Held {
    fn file(self, session: &mut Session) -> EntryId {
        match self {
            Self::Plain(text) => session.push_plain(text),
            Self::Notice(text) => session.push_notice(text),
            Self::Tally(text) => session.push_tally(text),
            Self::Note(text) => session.push_note(text),
        }
    }
}

/// The notes filed while a run has the transcript, for that run to take
/// before its next request rather than leave until it is over.
#[derive(Clone)]
pub struct Notes(Arc<Mutex<Vec<Held>>>);

impl Notes {
    /// The oldest note not yet taken.
    pub fn take(&self) -> Option<String> {
        let mut held = self.0.lock().unwrap();
        let at = held.iter().position(|h| matches!(h, Held::Note(_)))?;
        let Held::Note(text) = held.remove(at) else {
            unreachable!()
        };
        Some(text)
    }
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
            transcript: session.map_or_else(|| Transcript::Lent(Arc::default()), Transcript::Home),
            id,
            created,
            _claim: claim,
            name,
            tally: Tally::default(),
            unsaved: None,
            filed: false,
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
    executions: Option<Arc<crate::driver::background::Table>>,
) -> Arc<Resolved> {
    let mut resolved = resolved;
    // The tools hung on below were not there when `resolve` read the script
    // directories, so a name one of them holds is a script's: it keeps it and
    // the built-in waits behind it, returning when the script releases it.
    let mut said = Vec::new();
    let executions =
        executions.map(|table| Arc::new(crate::driver::background::Commands::new(table)));
    // Hooks ask where executions are kept, a child's gates as the lane's do.
    let ask: Arc<dyn toolbox::channel::Ask> = match &executions {
        Some(executions) => executions.clone(),
        None => Arc::new(toolbox::channel::NoOne),
    };
    let mut brief = resolved.brief.clone();
    Arc::make_mut(&mut brief).hooks = resolved.hooks.arm(ask.clone());
    let mut subagent = Subagent::prompted(
        agent,
        brief.clone(),
        archive,
        &resolved.prompt_files.prompt("subagent"),
        &resolved.standing,
        retry,
    )
    .with_tools(Arc::new(|_, registry| super::shells::foreground(registry)));
    if let Some(deadline) = resolved.subagent_deadline {
        subagent = subagent.with_deadline(deadline);
    }
    // Changed from here on a fresh `Arc`, since the child holds this one: it
    // runs on the registry it came from, and only the lane's copy carries the tool.
    offer_builtin(Arc::make_mut(&mut brief), Arc::new(subagent), &mut said);
    // The lane's copy only: a child never hears the user, whose words are
    // the only ones memory keeps.
    if resolved.remembers {
        offer_builtin(
            Arc::make_mut(&mut brief),
            Arc::new(crate::core::memory::Remember::new(
                pi_store::memory::Memory::default(),
            )),
            &mut said,
        );
    }
    // The lane's copy only, as with the subagent: a child cannot reach the
    // lane its prompt would come back to.
    if let Some(executions) = executions
        && tool::Tier::Exec.under(resolved.ceiling)
    {
        let patch = Arc::make_mut(&mut brief);
        let shell = Arc::new(
            toolbox::bash::Bash::new(executions.clone()).with_controls(executions.clone()),
        );
        offer_builtin(patch, shell.clone(), &mut said);
        for dir in &resolved.scripts {
            let held: Arc<dyn tool::Source> = dir.clone();
            let swapped = (patch.registry)
                .swap_source(&held, Arc::new(dir.with_shell(shell.clone(), ask.clone())));
            debug_assert!(swapped, "a script dir the brief never read");
        }
    }
    agent.apply(brief);
    // Re-armed as often as the config or the execution table moves; the same
    // script is skipped every time, so the note is kept once.
    for line in said {
        if !resolved.notes.contains(&line) {
            Arc::make_mut(&mut resolved).notes.push(line);
        }
    }
    resolved
}

// Offered after the scripts are read, yet it keeps its name over them. Say
// so here, where the name is known, rather than in resolve.
fn offer_builtin(brief: &mut agent::Briefing, tool: Arc<dyn tool::Tool>, said: &mut Vec<String>) {
    let name = tool.name().to_string();
    let shadows = brief.registry.sourced_holds(&name);
    if brief.registry.offer(tool) && shadows {
        said.push(format!(
            "tool skipped — script {name}: the name is taken by the built-in"
        ));
    }
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
        prompt_blocks: Vec::new(),
        hooks: Default::default(),
        subagent_deadline: None,
        standing: standing.into(),
        ceiling: tool::Tier::Exec,
        commands: Arc::new(Vec::new()),
        watched: Vec::new(),
        catalog_generation: Default::default(),
        scripts: Vec::new(),
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

    /// A command has given this lane's transcript back. The one place `Running`
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
        executions: Option<Arc<crate::driver::background::Table>>,
        change: impl FnOnce(&mut Agent),
    ) {
        let agent = Arc::make_mut(&mut self.checkout.agent);
        change(agent);
        self.checkout.resolved = arm(agent, resolved, archive, retry, executions);
    }

    /// Replace what was decided where the agent holds none of it — the
    /// command table — without re-arming anything.
    pub(super) fn keep(&mut self, resolved: Arc<Resolved>) {
        self.checkout.resolved = resolved;
    }

    /// Where the session stands when it moved or was compacted since the
    /// prompt's files were last read; the first look only marks where things
    /// stand. Marked again by `prompt_files_read`, once they were read.
    pub(super) fn prompt_files_due(&mut self) -> Option<ReadAt> {
        let session = self.talk.transcript.home()?;
        let compacted = session.entries().iter().rev().find_map(|e| match e {
            agent::session::Entry::Compaction { id, .. } => Some(*id),
            _ => None,
        });
        let now = (self.talk.id.clone(), compacted);
        match &self.checkout.read_at {
            None => {
                self.checkout.read_at = Some(now);
                None
            }
            Some(was) if *was == now => None,
            Some(_) => Some(now),
        }
    }

    /// The prompt's files were read with the session standing `at`.
    pub(super) fn prompt_files_read(&mut self, at: ReadAt) {
        self.checkout.read_at = Some(at);
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
        self.talk.transcript.home()
    }

    /// The ask nobody has answered, when a run left one open.
    pub fn last_ask(&self) -> Option<EntryId> {
        self.talk.transcript.home().and_then(Session::last_ask)
    }

    /// The checkout this lane works in.
    pub fn root(&self) -> &Path {
        self.checkout.ctx.workspace.root()
    }

    /// The checkout's context, for what measures the tree between turns.
    pub fn ctx(&self) -> &Ctx {
        &self.checkout.ctx
    }

    /// The diff of each file the model has seen that changed on disk since,
    /// told once; see `toolbox::drift`.
    pub fn drift(&self) -> Option<String> {
        toolbox::drift::drift(&self.checkout.ctx)
    }

    /// The context runs start from. A run already going keeps its own copy.
    pub fn ctx_mut(&mut self) -> &mut Ctx {
        &mut self.checkout.ctx
    }

    /// The context an execution runs with: this lane's, with the execution's own way out.
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

    /// What the run in flight has cost, for the lines that say the run alone.
    pub fn tally(&self) -> &Tally {
        &self.talk.tally
    }

    /// What this session has spent, counted at the wire: its runs, its
    /// compactions and the children and executions they started alike.
    pub fn spent(&self) -> Totals {
        self.checkout.agent.meter.read()
    }

    /// Fold an event into the meter. The loop's facts arrive here and nowhere
    /// else: what a run has spent is only known from them.
    pub fn note(&mut self, event: &Event) {
        self.talk.tally.on(event);
    }

    /// Start the run's count afresh and pin the rate it is priced at: a
    /// `/model` answered mid-run does not reprice the turn in flight.
    pub fn seed_meter(&mut self) {
        self.talk.tally.seed(self.checkout.agent.spec().pricing);
    }

    /// Lend the transcript to an execution for the length of its run.
    pub fn take_session(&mut self) -> Option<Session> {
        match std::mem::replace(&mut self.talk.transcript, Transcript::Lent(Arc::default())) {
            Transcript::Home(session) => Some(session),
            lent @ Transcript::Lent(_) => {
                self.talk.transcript = lent;
                None
            }
        }
    }

    /// Take it back: the execution has finished, or never started. Whatever was
    /// filed for the screen while it was away lands here, in the order it was
    /// drawn — the transcript a rebuild reads has to hold the rows the screen
    /// was shown.
    pub fn return_session(&mut self, mut session: Session) {
        let Transcript::Lent(held) = &mut self.talk.transcript else {
            panic!("a lane holds one transcript: a second would drop the first")
        };
        let held = std::mem::take(&mut *held.lock().unwrap());
        for held in held {
            self.talk.filed = true;
            held.file(&mut session);
        }
        self.talk.transcript = Transcript::Home(session);
    }

    /// The notes held for the run that has the transcript; `None` while it
    /// is home, where a note is filed at once.
    pub fn notes(&self) -> Option<Notes> {
        match &self.talk.transcript {
            Transcript::Home(_) => None,
            Transcript::Lent(held) => Some(Notes(held.clone())),
        }
    }

    /// What a run's ending should leave in the transcript.
    pub fn note_ended(&mut self, ended: &crate::driver::Ended) {
        if let Some(session) = self.talk.transcript.home_mut() {
            match ended {
                crate::driver::Ended::Done | crate::driver::Ended::Unsent => {}
                crate::driver::Ended::Stopped => session.note_user_stop(),
                crate::driver::Ended::Failed(why) => session.note_failure(why.clone()),
            }
        }
    }

    /// A line pi filed rather than the model: a command's note, read back
    /// with the rest of the transcript; held when a run has the transcript.
    ///
    /// Returns the id as `push_plain` does; `None` for a held note.
    pub fn push_note(&mut self, note: &str) -> Option<EntryId> {
        self.file(Held::Note(note.to_string()))
    }

    /// Plain text retained for the user, held when a run owns the transcript.
    ///
    /// Returns the id so a surface can tell it from what it has yet to show;
    /// `None` for a held row, filed later.
    pub fn push_plain(&mut self, text: &str) -> Option<EntryId> {
        self.file(Held::Plain(text.to_string()))
    }

    /// Keeps an agent report for the user without presentation markup.
    pub fn push_notice(&mut self, text: &str) -> Option<EntryId> {
        self.file(Held::Notice(text.to_string()))
    }

    /// Keeps a run's figures separately from plain text and reports.
    pub fn push_tally(&mut self, text: &str) -> Option<EntryId> {
        self.file(Held::Tally(text.to_string()))
    }

    fn file(&mut self, entry: Held) -> Option<EntryId> {
        match &mut self.talk.transcript {
            Transcript::Home(session) => {
                self.talk.filed = true;
                Some(entry.file(session))
            }
            Transcript::Lent(held) => {
                held.lock().unwrap().push(entry);
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

    pub fn held(&self) -> usize {
        match &self.talk.transcript {
            Transcript::Home(_) => 0,
            Transcript::Lent(held) => held.lock().unwrap().len(),
        }
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
        let Some(session) = lane.talk.transcript.home() else {
            return Ok(());
        };
        let header = session::Header {
            id: &lane.talk.id,
            model: &lane.checkout.agent.spec().model,
            name: lane.talk.name.as_deref(),
            created: lane.talk.created,
            spent: lane.spent(),
        };
        let saved = self
            .store
            .save(lane.checkout.ctx.workspace.root(), header, session);
        lane.talk.unsaved = saved.as_ref().err().map(|e| format!("{e:#}"));
        saved?;
        lane.talk.filed = false;
        Ok(())
    }

    /// Save every lane holding rows filed since its last save; one nothing
    /// was filed in stays as it is on disk, or off it.
    pub fn save_filed(&mut self) {
        for at in 0..self.lanes.len() {
            if self.lanes[at].talk.filed {
                let _ = self.save_lane(at);
            }
        }
    }
    /// Rewind the conversation to an entry and write the shorter transcript
    /// back.
    ///
    /// The entry decides which of the two it is; a stale pick falls
    /// through to removing nothing. A failed write still rewound: the
    /// outcome comes back beside the error, so an unsent prompt is not lost.
    pub fn rewind_to(&mut self, entry: agent::session::EntryId) -> (Rewound, anyhow::Result<()>) {
        // A rewind is refused while a run has the transcript, so this is the
        // idle path; without it there is nothing to go back through.
        let Some(session) = self.lane_mut().talk.transcript.home_mut() else {
            return (Rewound::Nothing, Ok(()));
        };
        let unsent = session.unsent_text(entry);
        let removed = match unsent {
            Some(_) => session.rollback_before(entry),
            None => session.rollback_to(entry),
        };
        if removed == 0 {
            return (Rewound::Nothing, Ok(()));
        }
        let saved = self.save();
        let outcome = match unsent {
            Some(text) => Rewound::Unsent(text),
            None => Rewound::Kept,
        };
        (outcome, saved)
    }
    // Become the session `talk` holds: the journal and spill namespace
    // follow it, replaced whole — nothing of the old session carries over.
    fn becomes(&mut self, talk: Conversation) {
        let lane = self.lane_mut();
        lane.talk = talk;
        // In place, not a new `Arc`: the subagent tool holds this one. A command
        // of the old session still out charges the new one from here.
        lane.checkout.agent.meter.reset(Totals::default());
        lane.checkout.ctx = lane
            .checkout
            .ctx
            .clone()
            .with_session(&lane.talk.id, pi_store::spill_root());
        // What the old session's model saw, this one never did.
        lane.checkout.ctx.viewed = Default::default();
        let path = self
            .store
            .journal_path(self.lane().root(), &self.lane().talk.id);
        journal::switched(&path, &self.lane().talk.id);
    }
    // The session being left has to survive the switch, or it is thrown
    // away: saved first, and the switch refused when that fails. An empty
    // one with nothing filed since its last save has nothing to keep; a run
    // in flight saves its own.
    pub(super) fn leave(&mut self, what: &str) -> Result<(), String> {
        let talk = &self.lane().talk;
        let keep = talk
            .transcript
            .home()
            .is_some_and(|s| talk.filed || !s.is_empty());
        if !keep {
            return Ok(());
        }
        self.save().map_err(|e| {
            let why = refused(what, e);
            format!("still on this session — {}", super::not_saved(&why))
        })
    }
    /// `/new`: leave the session in front for a fresh one, refused when the
    /// one being left cannot be saved.
    pub(super) fn new_session(&mut self) -> Result<(), String> {
        self.leave("new")?;
        self.fresh_session();
        Ok(())
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
        let spent = stored.spent;
        self.becomes(Conversation::new(
            id,
            created,
            Some(stored.into_session()),
            name,
            Some(claim),
        ));
        self.lane().checkout.agent.meter.reset(spent);
        // The id (timestamp+pid) is nothing to read; only a name — what the
        // user called it — is worth a line.
        match self.lane().talk.name.as_deref() {
            Some(name) => vec![format!("resumed “{name}”")],
            None => Vec::new(),
        }
    }
    /// `/mv`: the conversation in front carries on in `dir`, and all the tree
    /// decides is decided again there; whole or not at all.
    pub(super) fn move_to(&mut self, dir: &str) -> Result<Step, String> {
        let from = self.lane().root().to_path_buf();
        let failed = |e: anyhow::Error| format!("not moved — {}", refused("mv", e));
        let target = match dir.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                let home = crate::core::instructions::home()
                    .ok_or_else(|| failed(anyhow::anyhow!("$HOME is not set")))?;
                home.join(rest.trim_start_matches('/'))
            }
            _ => from.join(dir),
        };
        let settings = Settings::load(self.pinned.config.as_deref(), &target).map_err(failed)?;
        let config = settings.config().map_err(failed)?;
        let ws = tool::Workspace::new(&target)
            .and_then(|ws| crate::core::resolve::fence(ws, &config, &self.pinned))
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
            self.save().map_err(failed)?;
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
        let claim = if self.store.filed(&from, &id) {
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
        if let Some(executions) = &self.executions {
            executions.rehome(&from, &to);
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
    // Open a checkout as a new lane in front, on lane `like`'s agent; whole
    // or not at all. Which session it takes up is the caller's.
    pub(super) fn open_lane(
        &mut self,
        ws: tool::Workspace,
        worktree: Option<String>,
        like: usize,
    ) -> Result<(), String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let mut resolved = crate::core::resolve::resolve(
            &self.pinned,
            &ws,
            &self.config,
            &self.settings,
            None,
            self.surface,
        )
        .map_err(failed)?;
        // Built from the config in force, which already carries the facts
        // discovery last returned; only the project file can still differ.
        resolved.catalog_generation = self.catalog.generation();

        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let agent = &self.lanes[like].checkout.agent;
        let archive = self.archive(root, agent.spec().model.clone());
        let mut ag = (**agent).clone();
        // Before `arm`, which hands the meter on to the subagent it builds.
        ag.meter = Arc::default();
        let resolved = arm(
            &mut ag,
            Arc::new(resolved),
            archive,
            crate::core::retry(&self.config),
            self.executions.clone(),
        );

        // Built, not cloned from the lane being left: a `Ctx`'s tables key on
        // absolute paths in one tree, and none of that lane's describe this.
        let ctx = tool::Ctx::new(ws);
        self.lanes.push(Lane::opened(Opening {
            worktree,
            ..Opening::new(Arc::new(ag), resolved, ctx)
        }));
        self.current = self.lanes.len() - 1;
        if let Some(notes) = self.refresh_model_config() {
            Arc::make_mut(&mut self.lane_mut().checkout.resolved)
                .notes
                .extend(notes);
        }
        self.in_force();
        Ok(())
    }
    // The newest session filed for the checkout in front, or a fresh one.
    pub(super) fn take_up_latest(&mut self) -> Vec<String> {
        // Asked with the root the next save will file under, so a tree is found
        // by the same key it was stored by.
        // One open in another pi stays there; this lane starts its own.
        match self.store.take_latest(self.lane().root()) {
            Ok((stored, claim)) => self.adopt_session(stored, claim),
            Err(e) => {
                // Nothing recorded for this tree is the ordinary case; an
                // archive that will not load is not, and says so only here.
                tracing::debug!(target: "pi::session", error = %e, "no session to resume in this worktree");
                self.fresh_session();
                Vec::new()
            }
        }
    }
    /// The checkouts a restart left open, `(root, filed session)` in the
    /// order they were in, opened again around the one in front, each on its
    /// own session and model. What does not come back is said under the
    /// front one's banner, not fatal.
    pub fn reopen(&mut self, lanes: &[(std::path::PathBuf, Option<String>)]) {
        let front = self.current;
        let mut order = Vec::new();
        let mut said = Vec::new();
        for (root, id) in lanes {
            let ws = match tool::Workspace::new(root)
                .and_then(|ws| crate::core::resolve::fence(ws, &self.config, &self.pinned))
            {
                Ok(ws) => ws,
                Err(e) => {
                    said.push(format!("{}: not reopened — {e}", root.display()));
                    continue;
                }
            };
            order.push(ws.root().to_path_buf());
            if self.lane_at(ws.root()).is_some() {
                continue;
            }
            let worktree = crate::core::worktree::current(ws.root());
            if let Err(why) = self.open_lane(ws, worktree, front) {
                said.push(format!("{}: {why}", root.display()));
                continue;
            }
            let Some(id) = id else {
                self.fresh_session();
                continue;
            };
            // Carried over, not chosen: nothing to say about which one it is.
            match self.store.take(self.lane().root(), id) {
                Ok((stored, claim)) => {
                    let model = stored.model.clone();
                    self.adopt_session(stored, claim);
                    said.extend(self.rejoin(&model));
                }
                Err(e) => {
                    self.fresh_session();
                    said.push(format!("{}: started afresh — {e:#}", root.display()));
                }
            }
        }
        let front = self.lanes[front].token();
        self.lanes.sort_by_key(|lane| {
            order
                .iter()
                .position(|root| root == lane.root())
                .unwrap_or(usize::MAX)
        });
        self.current = self.position_of(front).unwrap_or(0);
        said.extend(self.refresh_model_config().unwrap_or_default());
        self.in_force();
        for line in &said {
            tracing::warn!(target: "pi::session", said = %line, "reopen");
        }
        if !said.is_empty() {
            let lane = self.lane_mut();
            Arc::make_mut(&mut lane.checkout.resolved)
                .notes
                .extend(said);
        }
    }
    // Back on the model the session last ran, as a `--resume` start is; says
    // only what that start would.
    fn rejoin(&mut self, model: &str) -> Vec<String> {
        let root = self.lane().root().display().to_string();
        match self.dial_for(model, pi_store::config::ModelOrigin::Resumed) {
            Ok(None) => Vec::new(),
            Ok(Some(dialled)) => {
                let said = dialled.warning.iter().chain(&dialled.assumed);
                let said = said.map(|line| format!("{root}: {line}")).collect();
                self.retarget(dialled.transport, dialled.spec);
                said
            }
            Err(why) => vec![format!("{root}: {why}")],
        }
    }
    // Switches transcript, name and id; the model does not change — reasoning
    // from a different model is demoted the same way as after `/model`.
    pub(super) fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
        // Same id is normally a no-op (else totals would zero); unless the
        // transcript itself is gone (a panicked execution), which must be reported.
        if id == self.lane().talk.id {
            if self.lane().session().is_none() {
                return Err(
                    "this checkout's transcript is gone — /new starts a session here".to_string(),
                );
            }
            return Ok(Vec::new());
        }
        self.leave("resume")?;
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

    // Lanes come back in the order given, the front one stays in front, and
    // what could not come back as asked is said rather than dropped.
    #[test]
    fn reopened_lanes_keep_their_order_and_say_what_failed() {
        let dir = tempfile::tempdir().unwrap();
        let (front, other) = (dir.path().join("front"), dir.path().join("other"));
        std::fs::create_dir_all(&front).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut core = a_repl(&front, std::sync::Arc::new(Recording::default()), "m");
        let gone = dir.path().join("gone");
        core.reopen(&[
            (other.clone(), Some("no-such-session".into())),
            (front.clone(), None),
            (gone.clone(), None),
            (other.clone(), None),
        ]);

        let roots: Vec<_> = core.lanes.iter().map(|l| l.root().to_path_buf()).collect();
        let real = |p: &std::path::Path| p.canonicalize().unwrap();
        assert_eq!(roots, [real(&other), real(&front)]);
        assert_eq!(core.lane().root(), real(&front));
        let notes = &core.lane().resolved().notes;
        let said = |what: &str, root: &std::path::Path| {
            notes
                .iter()
                .any(|n| n.starts_with(&root.display().to_string()) && n.contains(what))
        };
        assert!(said("started afresh", &other), "{notes:?}");
        assert!(said("not reopened", &gone), "{notes:?}");
    }

    // An opened lane is cloned from another's agent, so it would share that
    // meter unless given its own; a session taken up counts from its own file.
    #[test]
    fn spend_is_one_lanes_and_one_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        let spent = agent::Totals {
            cost: 1.0,
            ..Default::default()
        };
        core.lane().agent().meter.reset(spent);

        let ws = tool::Workspace::new(dir.path()).unwrap();
        core.open_lane(ws, None, 0).unwrap();
        assert_eq!(core.lane().spent(), agent::Totals::default());
        assert_eq!(core.lanes[0].spent(), spent);

        core.current = 0;
        core.save().unwrap();
        let left = core.lane().talk.id.clone();
        core.fresh_session();
        assert_eq!(core.lane().spent(), agent::Totals::default());

        core.resume(&left).unwrap();
        assert_eq!(core.lane().spent(), spent, "the saved figure comes back");
    }

    // Every model call names the conversation the lane is on, and a new one
    // is named afresh: a host that routes by it sees one id per conversation.
    #[test]
    fn the_context_names_the_conversation_the_lane_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        let first = core.lane().id().to_string();
        assert_eq!(core.lane().ctx().session(), Some(first.as_str()));

        core.fresh_session();
        let fresh = core.lane().id().to_string();
        assert_ne!(fresh, first);
        assert_eq!(core.lane().ctx().session(), Some(fresh.as_str()));
    }

    // A view carried over would let the next session edit unread, and tell
    // it of changes to a file it never saw.
    #[test]
    fn a_new_session_has_seen_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        let seen = dir.path().join("a.txt");
        core.lane().ctx().note_view(&seen, "old");
        std::fs::write(&seen, "new").unwrap();
        core.fresh_session();
        assert!(core.lane().ctx().viewed(&seen).is_none());
        assert_eq!(core.lane().drift(), None);
    }

    // A run takes the notes filed while it works; it leaves the rest, and
    // the rows, to land on its return.
    #[test]
    fn a_run_takes_its_notes_and_the_rest_land_on_return() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        assert!(
            core.lane().notes().is_none(),
            "a note at home is filed at once"
        );
        let session = core.lane_mut().take_session().unwrap();
        let notes = core.lane().notes().unwrap();
        core.lane_mut().push_plain("a row");
        core.lane_mut().push_note("taken");
        core.lane_mut().push_note("left");
        assert_eq!(notes.take().as_deref(), Some("taken"));
        core.lane_mut().return_session(session);
        let filed: Vec<_> = (core.lane().session().unwrap().entries().iter())
            .filter_map(|e| match e {
                agent::session::Entry::Note { note, .. } => Some(note.as_str()),
                agent::session::Entry::Plain { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(filed, ["a row", "left"]);
    }

    // `/new` leaves the session in front as `/resume` does: what was filed in
    // it while idle reaches disk first, and a session that cannot is kept.
    #[test]
    fn a_new_session_saves_the_one_left_or_refuses() {
        use crate::input::{Builtin, Intent, Step};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let mut core = a_repl(&root, std::sync::Arc::new(Recording::default()), "m");
        core.lane_mut().push_note("noted while idle");
        assert!(matches!(
            core.dispatch(Intent::Builtin(Builtin::New)),
            Step::Swap(_)
        ));
        assert!(core.store.filed(&root, "s1"));

        let left = core.lane().id().to_string();
        core.lane_mut().push_note("noted again");
        std::fs::write(root.join("blocker"), "").unwrap();
        core.store = pi_store::session::Store::new(root.join("blocker/state"));
        match core.dispatch(Intent::Builtin(Builtin::New)) {
            Step::Notice(why) => assert!(why.contains("not saved"), "{why}"),
            step => panic!("a session that cannot be saved is not left: {step:?}"),
        }
        assert_eq!(core.lane().id(), left);
    }

    // A rebuild from the config in force must not take the files' stamps as
    // read, or an edit made meanwhile is never applied.
    #[test]
    fn an_effort_change_leaves_a_settings_edit_to_the_reload() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.toml");
        std::fs::write(&settings, "").unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        core.pinned.config = Some(settings.display().to_string());
        let was = core.lane().resolved().watched.clone();
        core.effort("high");
        assert_eq!(core.lane().agent().brief.effort.name(), "high");
        assert_eq!(core.lane().resolved().watched, was);
        assert!(core.refresh_config().is_some());
    }

    // A skill found when the prompt's files are read again is a command at
    // once, as when the shelf itself is looked at.
    #[test]
    fn a_skill_read_with_the_prompt_files_is_put_in_force() {
        let repo = crate::core::worktree::test_repo();
        let mut core = a_repl(repo.path(), std::sync::Arc::new(Recording::default()), "m");
        assert!(
            core.refresh_prompt_files().is_none(),
            "the first look marks"
        );
        let skill = repo.path().join(".pi/skills/haiku");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\ndescription: d\n---\n").unwrap();
        core.fresh_session();
        core.refresh_prompt_files();
        assert!(core.commands.iter().any(|c| c.word == "/haiku"));
    }

    // The test lane's resolution with one script, `name`, written into `dir`
    // and read into its registry.
    fn with_script(dir: &std::path::Path, name: &str, script: &str) -> super::Resolved {
        use std::sync::Arc;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), script).unwrap();
        let scripts = Arc::new(toolbox::scripts::Dir::new(dir));
        let mut resolved = Arc::try_unwrap(super::a_resolved("standing")).ok().unwrap();
        Arc::make_mut(&mut resolved.brief)
            .registry
            .read(scripts.clone());
        resolved.scripts = vec![scripts];
        resolved
    }

    // An agent armed with `resolved`, and what its children are given.
    fn armed(
        resolved: std::sync::Arc<super::Resolved>,
        executions: Option<std::sync::Arc<crate::driver::background::Table>>,
    ) -> (agent::Agent, std::sync::Arc<super::Resolved>) {
        let mut agent = agent::Agent::new(
            std::sync::Arc::new(Recording::default()),
            crate::core::tests::test_spec("m"),
        );
        let resolved = super::arm(
            &mut agent,
            resolved,
            crate::core::archive::nowhere(),
            agent::Retry::default(),
            executions,
        );
        (agent, resolved)
    }

    // Only the lane offers wrappers that can start background commands.
    #[tokio::test]
    async fn only_the_lane_offers_background_script_wrappers() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let resolved = with_script(
            &dir.path().join("tools"),
            "off",
            "#!/bin/sh\n# ---\n# description = \"Go off\"\n# background = true\n# ---\necho '{\"status\":\"x\"}' >&3\n",
        );
        let (agent, resolved) = armed(Arc::new(resolved), Some(Arc::default()));

        let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let off = |registry: &tool::Registry| registry.get("off").unwrap();
        let args = serde_json::json!({});
        let lane = off(&agent.brief.registry).execute(args.clone(), &ctx).await;
        assert!(lane.unwrap().flatten().contains("command #1"));
        assert!(resolved.brief.registry.get("off").is_none());
    }

    #[test]
    fn a_lanes_shell_exposes_background_controls_without_a_separate_command_tool() {
        use std::sync::Arc;

        let (agent, _) = armed(super::a_resolved(""), Some(Arc::default()));
        assert!(agent.brief.registry.get("job").is_none());
        let shell = agent.brief.registry.get(toolbox::bash::Bash::NAME).unwrap();
        let schema = shell.schema();
        for control in ["stop", "restart", "output"] {
            assert!(schema["properties"].get(control).is_some(), "{control}");
        }
        assert!(schema["properties"].get("stop_all").is_none());
        assert!(schema["properties"].get("join").is_none());
        assert!(
            schema.get("required").is_none(),
            "an empty call lists background commands"
        );
    }

    // A script cannot stand in for a tool the lane hands out: the built-in
    // keeps the name and says the script was skipped, rather than leaving
    // it silently gone.
    #[test]
    fn a_built_in_keeps_its_name_over_a_script() {
        use std::sync::Arc;

        for name in [
            subagent::Subagent::NAME,
            crate::core::memory::Remember::NAME,
            toolbox::bash::Bash::NAME,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut resolved = with_script(
                &dir.path().join("tools"),
                name,
                "#!/bin/sh\n# ---\n# description = \"my own tool\"\n# ---\necho hi\n",
            );
            resolved.remembers = true;
            let (agent, resolved) = armed(Arc::new(resolved), Some(Arc::default()));

            let kept = agent.brief.registry.get(name).unwrap();
            assert_ne!(kept.description(), "my own tool", "a script took {name}");
            assert!(
                resolved.notes.iter().any(|n| n.contains(name)),
                "the script was skipped without saying so: {:?}",
                resolved.notes
            );
            assert!(
                (resolved.brief.registry.get(name))
                    .is_none_or(|t| t.description() == "my own tool"),
                "the child gained a lane's tool"
            );
        }
    }

    #[test]
    fn a_script_above_the_ceiling_does_not_hide_memory() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let name = crate::core::memory::Remember::NAME;
        let mut resolved = with_script(
            dir.path(),
            name,
            "#!/bin/sh\n# ---\n# description = \"custom memory\"\n# ---\necho hi\n",
        );
        resolved.remembers = true;
        resolved.ceiling = tool::Tier::Write;
        let brief = Arc::make_mut(&mut resolved.brief);
        brief.registry = std::mem::take(&mut brief.registry).within(tool::Tier::Write);
        let (agent, resolved) = armed(Arc::new(resolved), None);
        let memory = agent
            .brief
            .registry
            .get(name)
            .expect("memory is still available");
        assert_eq!(memory.tier(), tool::Tier::Write);
        assert!(
            resolved.notes.is_empty(),
            "a filtered script was reported as a conflict"
        );
    }

    // Reports must survive while a run owns the transcript.
    #[test]
    fn a_row_filed_while_a_run_holds_the_transcript_lands_with_it() {
        let mut lane = a_lane("s");
        // Test lanes start without a transcript.
        lane.return_session(agent::session::Session::default());
        let held = lane.take_session().expect("the run has it");

        lane.push_tally("3s · 1.2k/340 · $0.01");
        lane.push_plain("! the reply came back short");
        lane.push_notice("settled");
        lane.return_session(held);

        let filed: Vec<_> = lane
            .session()
            .expect("the run gave it back")
            .entries()
            .iter()
            .filter_map(|e| match e {
                agent::session::Entry::Tally { text, .. } => Some((text.as_str(), "tally")),
                agent::session::Entry::Plain { text, .. } => Some((text.as_str(), "plain")),
                agent::session::Entry::Notice { text, .. } => Some((text.as_str(), "notice")),
                _ => None,
            })
            .collect();
        assert_eq!(
            filed,
            [
                ("3s · 1.2k/340 · $0.01", "tally"),
                ("! the reply came back short", "plain"),
                ("settled", "notice")
            ],
            "in the order they were drawn, each still knowing what it is"
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

    // What the user said is handed back and goes with everything after it;
    // anything else stays and only what follows goes. Either way the shorter
    // transcript is what is on disk.
    #[test]
    fn a_rewind_unsends_what_was_said_and_keeps_anything_else() {
        use crate::input::{Builtin, Intent, Rewound, Step};
        use llm::message::{AssistantContent, Text};
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        let session = core.lane_mut().talk.transcript.home_mut().unwrap();
        let first = session.prompt("first");
        let reply =
            session.push_assistant(vec![AssistantContent::Text(Text { text: "ok".into() })]);
        session.prompt("second");
        let rewind = |core: &mut crate::core::Core, id: String| {
            core.dispatch(Intent::Builtin(Builtin::Rewind(id)))
        };
        let on_disk = |core: &crate::core::Core| {
            let path = core.store.path_of(core.lane().root(), core.lane().id());
            pi_store::session::Store::read(&path)
                .unwrap()
                .session
                .entries()
                .len()
        };

        let step = rewind(&mut core, reply.0.to_string());
        assert!(
            matches!(step, Step::Rewound(Rewound::Kept, None)),
            "{step:?}"
        );
        assert_eq!(on_disk(&core), 2);

        match rewind(&mut core, format!(" {} ", first.0)) {
            Step::Rewound(Rewound::Unsent(text), None) => assert_eq!(text, "first"),
            step => panic!("what was said comes back: {step:?}"),
        }
        assert_eq!(on_disk(&core), 0);

        let step = rewind(&mut core, first.0.to_string());
        assert!(
            matches!(step, Step::Rewound(Rewound::Nothing, None)),
            "{step:?}"
        );
        let step = rewind(&mut core, "last".into());
        assert!(matches!(step, Step::Notice(_)), "{step:?}");
    }

    // `/resume` reads a session's row off its file, so a name lands there at
    // once; a bare `/name` takes it away again.
    #[test]
    fn a_name_is_filed_as_it_is_given_and_taken_back() {
        use crate::input::{Builtin, Intent, Step};
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        let filed = |core: &crate::core::Core| {
            let path = core.store.path_of(core.lane().root(), core.lane().id());
            pi_store::session::Store::read(&path).unwrap().name
        };

        let step = core.dispatch(Intent::Builtin(Builtin::Name("bug hunt".into())));
        assert!(
            matches!(&step, Step::Notice(said) if said.contains("bug hunt")),
            "{step:?}"
        );
        assert_eq!(filed(&core).as_deref(), Some("bug hunt"));

        let step = core.dispatch(Intent::Builtin(Builtin::Name(String::new())));
        assert!(
            matches!(&step, Step::Notice(said) if said.contains("unnamed")),
            "{step:?}"
        );
        assert_eq!(filed(&core), None);
    }
}
