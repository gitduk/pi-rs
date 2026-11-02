//! The interactive surface: one loop holds raw mode for the whole session,
//! servicing the agent's events, the keyboard, and the animation timer.

mod background;
mod bar;
mod browse;
mod call;
mod clipboard;
mod editor;
mod execute;
mod frame;
mod keys;
pub use keys::WORDS;
mod menu;
mod mouse;
mod row;
mod run;
mod screen;
mod scrollback;
mod search;
mod select;
mod slash;
mod stream;
use stream::by_pi;
mod term;
mod ui;
mod view;
mod vim;

use agent::session::EntryId;
use anyhow::Result;
use crossterm::event::Event as TermEvent;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::render::Paint;
use crate::status;
use crate::tty;
use pi_core::core::lane::{Lane, Run};
use pi_core::core::{self, Core};
use pi_core::driver::{Drivers, Origin, Said};
use pi_core::input::commands::Choice;
use pi_core::input::{self, Fate, Intent, Rewound};
use pi_store::listing::Listing;
use ratatui::text::Text;
use screen::Screen;
use std::sync::Arc;

use crate::theme::following_terminal;
use menu::Lists;
use run::Done;
use term::{HISTORY_KEEP, Hold, drop_shared_history, history_of, reader};
use ui::{Mark, Tab, Ui, lane_name};
use view::{Pending, Queued, View, Views, front_view, prune_views, view_at};

// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking…";

// How close the halves of a named or modified pair must be (`esc esc`),
// borrowed from pi. A latching flag would read clear-type-clear as a pair.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

// The shortest a flash or a copy mark stays: long enough to catch a short
// line, short enough that a second try lands after it.
const FLASH: std::time::Duration = std::time::Duration::from_secs(1);

// How long a flash of `line` stays: each character adds less than the last,
// so a long error stays longer but not in proportion to its length.
fn flash_for(line: &str) -> std::time::Duration {
    let n = line.chars().count() as f64;
    std::time::Duration::from_secs_f64(2.0 * (1.0 + n / 20.0).ln()).max(FLASH)
}

// Shared text for several call sites, so a reword can't drift between them.
const NO_TRANSCRIPT: &str = "this checkout has no transcript — /new or /resume first";
const NOTHING_TO_REWIND: &str = "nothing to rewind to";
// The line `esc esc` starts, whose completions are what the user said.
const REWIND: &str = "/rewind ";
// The line a bare `/resume` becomes, whose completions are the sessions.
const RESUME: &str = "/resume ";
const NO_SESSIONS: &str = "no sessions recorded for this workspace";

// What a `Step::Handled` leaves behind: its lines as the page, config
// changes applied. A free fn since a run in flight calls it mid-borrow.
fn land_handled(ui: &mut Ui, core: &Core, view: &mut View, rows: Listing) {
    ui.open_answer(rows);
    ui.adopt_config(core, view);
}

// What the screen does to the lane itself, not the core. Admitted by
// `admit` (which says whether a run allows them), then carried out here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Deed {
    // A key that moved the editor and nothing else.
    Nothing,
    // The line being typed wants `$EDITOR`. The surface's own — only it
    // knows how to give the terminal away.
    External,
    // `/rewind` on the line, what the user said offered under it.
    Rewind,
    // Stop the run: from esc, or from the phone's `/stop`.
    Interrupt,
    // Esc caught a prompt on its way out: stop the run, then unsend it.
    Unsend,
    RecallQueued(Vec<pi_core::execution::Request>),
    // A command's row clicked: what it said that is not back yet, on a page.
    CommandOutput(u64),
    // Show or hide the completed executions waiting for delivery.
    BackgroundResults(bool),
    // An empty list stops every running execution in the current checkout.
    StopCommands(Vec<u64>),
    RestartCommands(Vec<u64>),
    // Drop the asides: the rows only drawn, never filed.
    Tidy,
    // The person's answer to a question its asker waits on, or none.
    Answer {
        question: u64,
        picked: Option<String>,
    },
}

impl Deed {
    // Whether it may happen now. Only the two that rewrite the transcript
    // care: the run in flight is writing it.
    fn fate(&self) -> Fate {
        match self {
            Deed::Rewind => {
                Fate::Refused("rewinding needs the transcript this run is writing — esc first")
            }
            Deed::Nothing
            | Deed::External
            | Deed::Interrupt
            | Deed::Unsend
            | Deed::RecallQueued(_)
            | Deed::CommandOutput(_)
            | Deed::BackgroundResults(_)
            | Deed::StopCommands(_)
            | Deed::RestartCommands(_)
            | Deed::Tidy
            | Deed::Answer { .. } => Fate::Now,
        }
    }

    // Whether the journal keeps it. Clearing the screen is a press made in
    // passing — the same key clears the line — and explains nothing later.
    fn journaled(&self) -> bool {
        match self {
            // Answers and queue withdrawals are journaled where they take effect.
            Deed::Nothing | Deed::Tidy | Deed::Answer { .. } | Deed::RecallQueued(_) => false,
            Deed::External
            | Deed::Rewind
            | Deed::Interrupt
            | Deed::Unsend
            | Deed::CommandOutput(_)
            | Deed::BackgroundResults(_)
            | Deed::StopCommands(_)
            | Deed::RestartCommands(_) => true,
        }
    }
}

// What a key asked for: `Intent` is what the core answers regardless of
// origin; `Deed` is what the screen keeps for itself.
#[derive(Debug)]
enum Asked {
    Core(Intent),
    Own(Deed),
}

enum Wake {
    // Something to carry out, once the select's borrows are gone, with the
    // line it was read from: echoed if the answer lands under it.
    Do(Asked, Option<String>),
    // A waiting operation, with the identity assigned when it entered the queue.
    Queued(Queued),
    // A line pi relays for the model: always a turn, never a command.
    Relay(agent::session::Prompt),
    // A turn ended and has to be settled, whichever lane it belongs to.
    Turn(Done),
    // Something only the screen cares about.
    Nothing,
    // Leave. Not a `break` at the arm: the way out has lanes to settle.
    Leave(&'static str),
}

/// How the surface was left.
pub enum Ended {
    Quit,
    /// A different pi installed: run it on every lane.
    Restart(core::Restart),
}

pub struct Tui {
    core: Core,
    // What each lane looks like, keyed by token — held here, not on the
    // lane, since the lane list reorders/drops lanes by identity.
    views: Views,
    ui: Ui,
    events: UnboundedReceiver<TermEvent>,
    // Stops the reader while a child holds the terminal.
    hold: Hold,
    drivers: Drivers,
    bar: bar::BarScript,
}

impl Tui {
    /// `last` is kept as a screen row at the end of the resumed transcript.
    pub fn new(mut core: Core, drivers: Drivers, last: Option<String>) -> Result<Self> {
        // The screen first, for raw mode: the answer to the background query
        // carries no newline, so a cooked read would wait for one forever.
        let screen = Screen::new()?;
        let answer = tty::background();
        let paint = Paint::with_theme(
            true,
            Arc::new(following_terminal(&core.config.theme, answer.bg)),
        )
        .with_background(answer.bg);
        let mut ui = Ui::new(
            screen,
            core.config.key_table.clone(),
            core.choices(),
            core.commands.clone(),
            Lists::new(core.store.clone(), core.lane_mut().root().to_path_buf()),
            paint,
        );
        ui.lists.pinned = core.pinned.clone();
        ui.tty_bg = answer.bg;
        // Asking the terminal read it, so it is typed in here: the keyboard
        // reader would never see those bytes again.
        ui.editor.insert_str(&answer.typed);
        ui.at_root = core.lane().root().to_path_buf();
        ui.status = core.config.status.to_vec();
        ui.set_vim(&core.config.vim);
        drop_shared_history();
        {
            let lines = history_of(&core.store, core.lane().root());
            ui.editor.seed_history(lines);
        }
        // Every lane, not just the front: a restart carries them all over.
        let mut views = Views::new();
        for lane in &core.lanes {
            let view = view::opened(&mut views, lane, &ui.paint);
            view.model = lane.model().to_string();
            // A resumed session shows its transcript from the start: the whole
            // screen is rebuildable now, so there is no reason to hide it.
            if let Some(session) = lane.session().filter(|s| !s.is_empty()) {
                ui.rebuild(view, session, lane.root());
            }
        }
        let (events, hold) = reader();
        let mut tui = Self {
            core,
            views,
            ui,
            events,
            hold,
            drivers,
            bar: bar::BarScript::find(),
        };
        if let Some(line) = last {
            tui.restarted(&line);
        }
        Ok(tui)
    }

    fn restarted(&mut self, line: &str) {
        let lane = self.core.lane_mut();
        let view = view::opened(&mut self.views, lane, &self.ui.paint);
        self.ui.say_kept(lane, view, &by_pi(line));
    }

    // A surface on an in-memory screen, for tests driving the loop's
    // settle side. No reader thread, no history file to touch.
    #[cfg(test)]
    fn on_test_screen(mut core: Core) -> Self {
        let paint = Paint::with_theme(false, Arc::new(core.config.theme.clone()));
        let mut ui = Ui::new(
            screen::Screen::test(80, 24),
            core.config.key_table.clone(),
            core.choices(),
            core.commands.clone(),
            Lists::new(core.store.clone(), core.lane_mut().root().to_path_buf()),
            paint,
        );
        ui.at_root = core.lane_mut().root().to_path_buf();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            core,
            views: Views::new(),
            ui,
            events: rx,
            hold: Hold::default(),
            drivers: Drivers::default(),
            bar: bar::BarScript::at(None),
        }
    }

    // Best effort: losing a recall list is not worth a message on the way out.
    fn save_history(&self) {
        let path = self.core.store.history_path(self.core.lane().root());
        let all = self.ui.editor.history();
        let keep = &all[all.len().saturating_sub(HISTORY_KEEP)..];
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = tool::state::write_private(&path, editor::encode(keep).as_bytes());
    }

    // What the input keys on the root goes where the lane in front is now.
    fn follow_root(&mut self) {
        let root = self.core.lane().root().to_path_buf();
        self.ui.lists.at(&root);
        self.ui.at_menu = None;
        // Recall follows the checkout like the lists do; the line just
        // typed is already filed (`save_history` ran while still in front).
        self.ui
            .editor
            .seed_history(history_of(&self.core.store, &root));
        self.ui.at_root = root;
    }

    // Brings the screen into step after a lane switch: parks the old
    // lane's draft, restores the new one's, follows lists, replays news.
    fn reconcile(&mut self, was: usize) {
        if was == self.core.current {
            return;
        }
        self.ui
            .leave_lane(view_at(&mut self.views, self.core.lanes[was].token()));
        let parked = std::mem::take(&mut front_view(&mut self.views, self.core.lane()).draft);
        self.ui.editor.set_line(&parked);
        self.follow_root();
        // A newly opened lane has no banner yet. Nothing reads the screen
        // here — the events below ask for it again, in their own lane.
        view::opened(&mut self.views, self.core.lane(), &self.ui.paint);
        // Esc asked the prompt back before the screen moved on and the run ended
        // out of sight: a rewind wants the screen, so it waited for it.
        if self.core.lane_mut().take_ended() == Some(true)
            && let Some(id) = self.core.lane().last_ask()
        {
            self.rewind_turn(id);
        }
    }

    // The line a turn or a `!` answers, onto the screen as it starts.
    fn echo(&mut self, line: &str) {
        let view = front_view(&mut self.views, self.core.lane());
        self.ui.submit(view, line);
        view.surface.follow();
    }

    // A handled command's lines are its answer, and the answer goes to the
    // slash page — the screen is not a log of what the user typed at the interface.
    fn land_lines(&mut self, rows: Listing) {
        land_handled(
            &mut self.ui,
            &self.core,
            front_view(&mut self.views, self.core.lane()),
            rows,
        );
    }

    // A notice goes to the bar; the slash page is for answers asked for.
    pub(super) fn notice(&mut self, line: String) {
        if !line.is_empty() {
            self.ui.flash(line);
        }
    }

    // A notice for a change that may have moved config: applied as
    // `land_lines` does, without touching the slash page.
    pub(super) fn land_notice(&mut self, said: String) {
        let view = front_view(&mut self.views, self.core.lane());
        self.ui.adopt_config(&self.core, view);
        self.notice(said);
    }

    // A skill warning must not hide the reload that made it visible.
    fn refresh_files(&mut self) {
        let mut notes = Vec::new();
        if let Some(said) = self.core.refresh_config() {
            notes.extend(said);
            let view = front_view(&mut self.views, self.core.lane());
            self.ui.adopt_config(&self.core, view);
        }
        if self.core.refresh_prompts() {
            self.ui.commands = self.core.commands.clone();
        }
        if let Some(said) = self.core.refresh_skills() {
            self.ui.commands = self.core.commands.clone();
            notes.extend(said);
        }
        self.notice(pi_core::input::notice(notes));
    }

    // Take what every lane's run has posted since the last look into its own
    // view, drawn or not: its meter, filed rows and end happen when they do.
    async fn serve_lanes(&mut self) {
        for at in 0..self.core.lanes.len() {
            while let Ok(event) = self.core.lanes[at].inbox().try_recv() {
                // Every lane, not just the one in front: a turn a command's input
                // opened is still owed its answer after a switch.
                let lane = &self.core.lanes[at];
                self.drivers.observe(lane.token(), lane.root(), &event);
                // Opened first: a banner drawn later would replace the rows.
                let view = view::opened(&mut self.views, &self.core.lanes[at], &self.ui.paint);
                self.ui.on_event(&mut self.core.lanes[at], view, event);
            }
        }
    }

    // Rebuilds the bar before every draw: an off-screen run must reach
    // the screen unasked, and a step walks this same order (bar = ring).
    fn refresh_tabs(&mut self) {
        let current = self.core.current;
        let mut tabs: Vec<Tab> = self
            .core
            .lanes
            .iter()
            .enumerate()
            .map(|(at, lane)| Tab {
                // In front is in front, whatever it is doing: the run row above
                // already says whether this one is working.
                mark: if at == current {
                    Mark::Front
                } else {
                    match lane.run() {
                        // A loop between rounds: the lane has not finished.
                        Run::Ended { ok: true, .. } if self.drivers.holds(lane.token()) => {
                            Mark::Plain
                        }
                        Run::Ended { ok: true, .. } => Mark::Done,
                        Run::Ended { ok: false, .. } => Mark::Failed,
                        Run::Running { .. } | Run::Idle => Mark::Plain,
                    }
                },
                name: lane_name(lane),
            })
            .collect();
        // The checkouts no lane has open, in git's order: the ring is not the
        // lanes alone. `Lists` holds the read, one fork per list.
        for tree in self.ui.lists.worktrees_read().unwrap_or_default() {
            if tabs.iter().any(|t| t.name == tree.name) {
                continue;
            }
            tabs.push(Tab {
                mark: Mark::Unopened,
                name: tree.name.clone(),
            });
        }
        self.ui.tabs = tabs;
    }

    // Drops idle lanes whose checkout was deleted outside pi (a running
    // or looping one keeps its index). Silent — the bar says it's gone.
    fn drop_vanished_lanes(&mut self) {
        let mut gone: Vec<usize> = Vec::new();
        // Back to front, stopping at a working lane: removing one before
        // it would shift the index a run reports back by.
        for (at, lane) in self.core.lanes.iter().enumerate().rev() {
            if lane.is_running() || self.drivers.holds(lane.token()) {
                break;
            }
            if at == self.core.current {
                continue;
            }
            // Only a lane with a checkout of its own can go this way: the one
            // pi was started in is no worktree, and one still on disk stands.
            if lane.worktree().is_none() || lane.root().exists() {
                continue;
            }
            gone.push(at);
        }
        if gone.is_empty() {
            return;
        }
        // Already highest first, so earlier indices stay put while they go.
        for at in gone {
            self.core.remove_lane(at);
        }
        // The ring's list is cached; a vanished checkout must not stay in it
        // for a later step to offer — and re-create — by its stale name.
        self.ui.lists.forget();
    }

    // The front lane's screen rebuilt from its transcript, for a swap or
    // rewind. `NO_TRANSCRIPT` names a dead session rather than panicking.
    fn rebuild_front(&mut self) {
        let Some(session) = self.core.lane().session() else {
            return;
        };
        self.ui.rebuild(
            front_view(&mut self.views, self.core.lane()),
            session,
            self.core.lane().root(),
        );
    }

    // A lane's news goes to its own screen, never the one in front; a
    // never-drawn lane gets its banner first so it can't replace the row.
    fn say_of(&mut self, lane: usize, what: impl Into<Text<'static>>) {
        let view = view::opened(&mut self.views, &self.core.lanes[lane], &self.ui.paint);
        self.ui.say(view, what);
    }

    // Deeds the screen keeps for itself: only the surface can move the
    // screen, keyboard, and process, and knows which lane is in front.
    async fn carry(&mut self, deed: Deed) {
        if deed.journaled() {
            tracing::info!(target: "pi::session", deed = ?deed, "deed");
        }
        match deed {
            Deed::Nothing => {}
            Deed::Interrupt => self.stop_current(false),
            Deed::Unsend => self.stop_current(true),
            Deed::RecallQueued(requests) => {
                for request in requests {
                    self.cancel_queued(request, "recalled for editing");
                }
            }
            Deed::Rewind => self.open_rewind(),
            Deed::External => self.edit_externally().await,
            Deed::BackgroundResults(shown) => self.ui.background_results_shown = shown,
            Deed::CommandOutput(id) => {
                if let Some(execution) = self
                    .ui
                    .every_command
                    .iter()
                    .find(|j| j.id == id && j.state == pi_core::driver::background::State::Back)
                {
                    if let Some(body) = self.drivers.background().body(self.core.lane().root(), id)
                    {
                        self.ui
                            .open_page(call::background_page(execution, &body, &self.ui.paint));
                    }
                    return;
                }
                self.ui.background_open = match &self.ui.background_open {
                    Some(open) if open.id == id => None,
                    _ => Some(ui::OpenCommand {
                        id,
                        body: String::new(),
                    }),
                };
            }
            Deed::StopCommands(ids) => {
                let said = self.drivers.stop_commands(self.core.lane().root(), &ids);
                self.land_driven(said);
            }
            Deed::RestartCommands(ids) => {
                let said = self.drivers.restart_commands(self.core.lane().root(), &ids);
                self.land_driven(said);
            }
            Deed::Tidy => self.ui.tidy(front_view(&mut self.views, self.core.lane())),
            Deed::Answer { question, picked } => self.drivers.background().answer(question, picked),
        }
    }

    fn land_driven(&mut self, said: Said) {
        match said {
            Said::Nothing => {}
            Said::Notice(line) => self.notice(line),
            Said::Noted(line, note) => {
                self.notice(line);
                let lane = self.core.lane_mut();
                let view = front_view(&mut self.views, lane);
                self.ui.say_note(lane, view, &note);
            }
            Said::Transcript(line) => self
                .ui
                .say(front_view(&mut self.views, self.core.lane()), by_pi(&line)),
        }
    }

    // The one gate every input passes, so two ways of asking the same
    // thing can't get different answers. `fate` decides only mid-run.
    fn admit(&mut self, asked: Asked, origin: Origin, line: Option<String>) -> Wake {
        if !self.core.lane().is_running()
            || matches!(&asked, Asked::Core(Intent::Prompt(_))) && origin == Origin::Typed
        {
            return Wake::Do(asked, line);
        }
        let intent = match asked {
            // A deed is the screen's own move: it never waits for the run,
            // because waiting is one of the answers about lines.
            Asked::Own(deed) => match deed.fate() {
                // A key, not a command: a deed's refusal is the answer to a
                // press, and the slash page is for what a slash command output.
                Fate::Refused(why) => {
                    self.ui.flash(why);
                    return Wake::Nothing;
                }
                Fate::Now => return Wake::Do(Asked::Own(deed), None),
                Fate::Queued => unreachable!("a deed waits for nothing"),
            },
            Asked::Core(intent) => intent,
        };
        match intent.fate() {
            Fate::Now => Wake::Do(Asked::Core(intent), line),
            Fate::Queued => {
                self.enqueue(Queued {
                    input: Pending::Intent(intent),
                    request: pi_core::execution::Request::new(origin, self.core.lane().token()),
                    line,
                });
                Wake::Nothing
            }
            // A command refused because a run is in flight.
            Fate::Refused(why) => {
                let request = pi_core::execution::Request::new(origin, self.core.lane().token());
                tracing::info!(target: "pi::session", request = request.id, ?origin,
                    lane = request.lane, ?intent, why, "dispatch refused");
                self.drivers.started(request, false);
                self.complete(
                    request,
                    pi_core::execution::Output::Notice(String::new()),
                    toolbox::channel::Outcome::Failed(why.to_string()),
                );
                Wake::Nothing
            }
        }
    }

    // A command asked to stop the turn running in its checkout, as esc would;
    // with none running there is nothing to say.
    fn interrupt_at(&mut self, root: &std::path::Path) {
        let Some(at) = self.core.lane_at(root) else {
            return;
        };
        if !self.core.lanes[at].is_running() {
            return;
        }
        if at == self.core.current {
            self.stop_current(false);
        } else {
            self.core.lanes[at].stop(false);
        }
    }

    // Stop the run in front, if there is one. `unsend` also takes the prompt
    // back once it has stopped.
    fn stop_current(&mut self, unsend: bool) {
        // Said here rather than at the callers: the state is the only thing
        // that knows, and every way of asking to stop arrives through it.
        if !self.core.lane_mut().stop(unsend) {
            self.ui.flash("nothing running to stop");
            return;
        }
        if let Some(work) = &mut front_view(&mut self.views, self.core.lane()).state.work {
            work.stopping = true;
        }
    }

    // A different pi installed: become it once nothing on screen is half
    // done. Asked every pass; `restart` says when not yet.
    fn restart_into_update(&self) -> Option<Ended> {
        let version = core::update::newer()?;
        let settled = self.ui.read.is_none()
            && self.ui.editor.is_empty()
            && self.ui.background_open.is_none()
            && (self.views.values()).all(|v| v.draft.is_empty() && v.queued.is_empty())
            && !(self.core.lanes.iter()).any(|lane| self.drivers.holds(lane.token()));
        if !settled {
            return None;
        }
        let restart = self.core.restart()?;
        tracing::info!(target: "pi::update", version = %version, "restarting into it");
        Some(Ended::Restart(restart))
    }

    /// Drives the terminal until the user leaves. Each lane owns the
    /// channel its runs post to, so a lane left behind keeps working.
    pub async fn run(mut self) -> Result<Ended> {
        let mut ended = Ended::Quit;
        // Every lane's runs report here when they end. One channel rather than a
        // handle per lane: the loop waits on it like any other source.
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
        let (fetched_tx, mut fetched_rx) = tokio::sync::mpsc::unbounded_channel::<run::Fetched>();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<run::Started>();
        let mut tick = tokio::time::interval(status::SPIN);
        let mut hangup = term::Hangup::new()?;
        let mut looked = std::time::Instant::now();
        // Off while nothing runs, so the interval falls behind; one
        // late tick then ordinary cadence avoids a catch-up spin.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let update = core::update::changed();
        let leaving = loop {
            self.serve_lanes().await;
            // Before the bar rebuilds, so a vanished tab goes with its lane.
            self.drop_vanished_lanes();
            for request in prune_views(&self.core, &mut self.views) {
                self.cancel_queued(request, "its session was removed");
            }
            let core = &self.core;
            for said in self.drivers.retain(|t| core.position_of(t).is_some()) {
                self.ui
                    .say(front_view(&mut self.views, self.core.lane()), by_pi(&said));
            }
            // What changed on disk is taken up within a second: settings
            // resolve the lane again, a skill's word starts to answer. Not on
            // every frame: each look is a walk.
            if looked.elapsed() >= std::time::Duration::from_secs(1) {
                looked = std::time::Instant::now();
                self.refresh_files();
            }
            self.refresh_tabs();
            self.bar.poke(self.core.lane());
            let executions = self.drivers.background();
            executions.relay_progress();
            for root in executions.take_interrupts() {
                self.interrupt_at(&root);
            }
            // Taken down first, or the copy would put them back up.
            for question in std::mem::take(&mut self.ui.answered) {
                executions.answer(question, None);
            }
            self.ui
                .sync_questions(executions.questions(self.core.lane().root()));
            let root = self.core.lane().root();
            self.ui.sync_background(executions.background(root));
            self.ui.tools = Some(self.core.lane().agent().brief.clone());
            // Walked once per line completing it, not on every pass: the
            // transcript holds still while `/rewind` is on the line.
            if !self.ui.editor.text().starts_with(REWIND) {
                self.ui.said.clear();
            } else if self.ui.said.is_empty() {
                self.ui.said = said_of(self.core.lane());
            }
            let open = self.ui.background_open.take();
            let shown = |id: u64| self.ui.executions.iter().any(|j| j.id == id);
            self.ui.background_open = open.filter(|o| shown(o.id)).and_then(|o| {
                let body = executions.body(root, o.id)?;
                Some(ui::OpenCommand { id: o.id, body })
            });
            let view = front_view(&mut self.views, self.core.lane());
            self.ui.flush(self.core.lane(), view);
            if std::mem::take(&mut self.ui.redraw) {
                continue;
            }
            // After the frame, not before it: a fork here would hold the
            // screen blank. Read again whenever something dropped the list.
            if self.ui.lists.worktrees_read().is_none() {
                let _ = self.ui.lists.worktrees();
                continue;
            }
            let running = self.core.lane().is_running();
            let anywhere = self.core.lanes.iter().any(|lane| lane.is_running());
            let waiting = !view.queued.is_empty();
            // Who sent what is carried out this pass.
            let mut origin = Origin::Typed;
            // Results and loops wait for a free lane. User submissions also
            // reach admission during a run, so immediate commands can answer.
            let free = !running && !waiting;
            let driven = (free || running).then(|| self.driver_line()).flatten();
            if free
                && driven.is_none()
                && let Some(restart) = self.restart_into_update()
            {
                ended = restart;
                break "pi restarted into an update.";
            }
            let lane = self.core.lane();
            let woke = if let Some((from, wake)) = driven {
                origin = from;
                wake
            } else if !waiting || running {
                let bar_due = self.bar.due().map(tokio::time::Instant::from_std);
                // A character held off the list shows in it once no pair took it back.
                let held = self.ui.held().map(tokio::time::Instant::from_std);
                // Only while the lane is free: a prompt due under a run would
                // wake this loop again and again with nothing it may do.
                let executions = self.drivers.background();
                let due_back = free.then(|| executions.next_due(lane.root())).flatten();
                // Every branch must be cancel-safe: a loser is dropped mid-poll.
                // `recv()` and `tick()` are; a blocking read gets its own thread.
                tokio::select! {
                    Some(done) = done_rx.recv() => Wake::Turn(done),
                    Some(started) = started_rx.recv() => {
                        self.land_started(started);
                        Wake::Nothing
                    }
                    Some(fetched) = fetched_rx.recv() => {
                        self.land_fetched(fetched, &done_tx);
                        Wake::Nothing
                    }
                    // Only while something runs, or a flash is up — an idle
                    // loop waking ten times a second has nothing to spin.
                    _ = tick.tick(), if anywhere || executions.working(lane.root()) || self.ui.flash.is_some() || self.ui.copied.is_some() => {
                        self.ui.spinner += 1;
                        Wake::Nothing
                    }
                    Some(out) = self.bar.rx.recv() => {
                        if let Some(why) = self.bar.land(out, &mut self.ui.layout) {
                            self.notice(why);
                        }
                        Wake::Nothing
                    }
                    _ = tokio::time::sleep_until(bar_due.unwrap_or_else(tokio::time::Instant::now)),
                        if bar_due.is_some() => Wake::Nothing,
                    _ = tokio::time::sleep_until(held.unwrap_or_else(tokio::time::Instant::now)),
                        if held.is_some() => Wake::Nothing,
                    // Something was left, cancelled or finished: look again.
                    _ = executions.changed().notified() => Wake::Nothing,
                    // A different pi installed: the bar says so.
                    _ = update.notified() => Wake::Nothing,
                    // Left the way `/exit` leaves: runs settled, executions taken down.
                    () = hangup.recv() => {
                        tracing::info!(target: "pi::session", "hung up");
                        Wake::Leave("The terminal hung up; pi exited.")
                    }
                    _ = tokio::time::sleep_until(due_back.unwrap_or_else(tokio::time::Instant::now)),
                        if due_back.is_some() => Wake::Nothing,
                    key = self.events.recv() => match key {
                        Some(key) => {
                            let lane = self.core.lane();
                            let view = front_view(&mut self.views, lane);
                            let intent = self.ui.key(lane, view, key, running);
                            let line = self.ui.took_submit();
                            if line.is_some() {
                                self.save_history();
                            }
                            self.admit(intent, Origin::Typed, line)
                        }
                        None => Wake::Leave("The terminal input closed; pi exited."),
                    },
                }
            } else {
                // One at a time, each still the intent it was read as.
                // Joined as lines, a command+prompt would lose its first word.
                let queued = front_view(&mut self.views, self.core.lane())
                    .queued
                    .pop_front()
                    .expect("something is waiting");
                Wake::Queued(queued)
            };
            // Out here, where all of `self` is free again.
            let (asked, line) = match woke {
                Wake::Turn(done) => {
                    self.settle(done).await;
                    continue;
                }
                Wake::Nothing => continue,
                Wake::Leave(why) => break why,
                // Its driver hears nothing of the turn, so nothing follows.
                Wake::Relay(ask) => {
                    self.deliver_prompt(ask, Origin::Background, None, &done_tx);
                    continue;
                }
                Wake::Queued(queued) => {
                    let origin = queued.request.origin;
                    if self
                        .resume_queued(queued, &done_tx, &fetched_tx, &started_tx)
                        .await
                    {
                        break match origin {
                            Origin::Typed => "The user exited pi.",
                            Origin::Loop | Origin::Background | Origin::Input(_) => {
                                "pi exited in response to a background command."
                            }
                        };
                    }
                    continue;
                }
                Wake::Do(asked, line) => (asked, line),
            };
            // What the surface answers for itself: the screen, the keyboard
            // and the process are not `Core`'s to move.
            let intent = match asked {
                Asked::Own(deed) => {
                    self.carry(deed).await;
                    continue;
                }
                // A key that means a command — `ctrl+l` twice is `/new` —
                // arrives already read.
                Asked::Core(ready) => ready,
            };
            if self
                .execute(intent, origin, line, &done_tx, &fetched_tx, &started_tx)
                .await
            {
                break match origin {
                    Origin::Typed => "The user exited pi.",
                    Origin::Loop | Origin::Background | Origin::Input(_) => {
                        "pi exited in response to a background command."
                    }
                };
            }
        };
        self.save_history();
        self.settle_all(&mut done_rx, leaving).await;
        self.core.hand_over_background();
        Ok(ended)
    }

    // The line a driver sends the front lane next, drawn, with who sent it:
    // a relay opens a turn, anything else is read.
    fn driver_line(&mut self) -> Option<(Origin, Wake)> {
        let lane = self.core.lane();
        let next = if lane.is_running() {
            self.drivers.next_input(lane.root())?
        } else {
            self.drivers.next(lane.token(), lane.root(), lane.ctx())?
        };
        let view = front_view(&mut self.views, self.core.lane());
        view.surface.follow();
        // Results remain model input; a person's submission uses the same
        // parser and admission rules as a submitted local line.
        let wake = match next.label {
            Some(label) => {
                tracing::info!(
                    target: "pi::session", origin = ?next.origin, lane = self.core.lane().token(), label = %label, "relay"
                );
                self.ui.submit_relayed(view, &label, &next.line);
                Wake::Relay(agent::session::Prompt {
                    text: next.line,
                    shown: Some(label),
                    relayed: Some(next.note),
                    images: Vec::new(),
                })
            }
            None => {
                let intent = input::read(
                    &next.line,
                    &self.core.commands,
                    Some(&self.core.lane().agent().brief.registry),
                );
                if !next.note.is_empty() {
                    self.core.lane_mut().push_note(&next.note);
                }
                self.admit(Asked::Core(intent), next.origin, Some(next.line))
            }
        };
        Some((next.origin, wake))
    }

    // Cut the transcript at the prompt an Esc took back, and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        let (outcome, saved) = self.core.rewind_to(id);
        self.land_rewound(outcome, saved.err().map(|e| core::not_saved(&e)));
    }

    // A save failure must remain visible beside the rewind confirmation.
    fn land_rewound(&mut self, outcome: Rewound, unsaved: Option<String>) {
        let mut notes: Vec<String> = unsaved.into_iter().collect();
        // Past a rewind the transcript is truth again: the whole view is
        // rebuilt from it, back to the node the conversation returned to.
        let said = match outcome {
            Rewound::Nothing => {
                notes.push(NOTHING_TO_REWIND.to_string());
                self.notice(input::notice(notes));
                return;
            }
            Rewound::Unsent(_) if !self.ui.editor.is_empty() => {
                "unsent — the line you were typing stands; Up recalls it".to_string()
            }
            // Unsent is half-typed, not gone: the text goes back to
            // the editor, and the cursor on it says what happened.
            Rewound::Unsent(text) => {
                self.rebuild_front();
                self.ui.editor.set_line(&text);
                self.notice(input::notice(notes));
                return;
            }
            Rewound::Kept => {
                let at = self
                    .core
                    .lane()
                    .session()
                    .and_then(|s| s.last_node())
                    .map(|n| pi_store::text::clip(n.show(), 60))
                    .filter(|t| !t.is_empty())
                    .map(|t| format!(" — the transcript now ends at {t}"))
                    .unwrap_or_default();
                format!("rewound{at}")
            }
        };
        self.rebuild_front();
        notes.insert(0, said);
        self.notice(input::notice(notes));
    }

    // `/rewind ` on the line, what the user said completing it.
    fn open_rewind(&mut self) {
        self.ui.said = said_of(self.core.lane());
        if self.ui.said.is_empty() {
            self.ui.flash(NOTHING_TO_REWIND);
            return;
        }
        self.ui.seed(REWIND);
    }
}

// What the user said, for `/rewind`: a rewind takes a prompt back, and an
// answer is a place the conversation carries on from.
fn said_of(lane: &Lane) -> Vec<Choice> {
    lane.session().map_or_else(Vec::new, |session| {
        (session.rewind_nodes().into_iter())
            .map(|node| Choice {
                name: node.id().0.to_string(),
                note: pi_store::text::clip(node.show(), 60),
            })
            .collect()
    })
}

#[cfg(test)]
mod tests;
