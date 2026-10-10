//! The interactive surface: one loop holds raw mode for the whole session,
//! servicing the agent's events, the keyboard, and the animation timer.

mod bar;
mod browse;
mod call;
mod clipboard;
mod editor;
mod job;
mod menu;
mod mouse;
mod reply;
mod row;
mod screen;
mod scrollback;
mod select;
mod stream;
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
use pi_core::core::lane::Run;
use pi_core::core::{self, Core};
use pi_core::driver::{Drivers, Origin, Said};
use pi_core::input::{self, Fate, Intent, Rewound, Step};
use pi_store::keys::Keys;
use pi_store::listing::Listing;
use ratatui::text::Line;
use screen::Screen;
use std::sync::Arc;

use job::Done;
use menu::{Lists, MenuEntry};
use term::{HISTORY_KEEP, Hold, drop_shared_history, history_of, reader};
use ui::{Mark, Tab, Ui, following_terminal, lane_name};
use view::{Queued, View, Views, front_view, prune_views, view_at};

// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking…";

// How close the halves of a named or modified pair must be (`esc esc`),
// borrowed from pi. A latching flag would read clear-type-clear as a pair.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

// The shortest a flash or a copy mark stays: long enough to catch a short
// line, short enough that a second try lands after it.
const FLASH: std::time::Duration = std::time::Duration::from_secs(1);
// The longest a flash stays, however much it says: the bar is borrowed.
const FLASH_MAX: std::time::Duration = std::time::Duration::from_secs(4);

// How long a flash of `line` stays: about 16 characters a second of reading.
fn flash_for(line: &str) -> std::time::Duration {
    let reading = std::time::Duration::from_millis(line.chars().count() as u64 * 60);
    reading.clamp(FLASH, FLASH_MAX)
}

// Shared text for several call sites, so a reword can't drift between them.
const NO_TRANSCRIPT: &str = "this checkout has no transcript — /new or /resume first";
const NOTHING_TO_REWIND: &str = "nothing to rewind to";

// What a `Step::Handled` leaves behind: its lines as the reply, config
// changes applied. A free fn since a run in flight calls it mid-borrow.
fn land_handled(ui: &mut Ui, core: &Core, view: &mut View, rows: Listing) {
    ui.open_reply(rows);
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
    // The rewind selector wants everywhere the session can go back to —
    // only the surface can overlay the transcript.
    Rewind,
    // A row chosen from it: the conversation rewinds there, kept or
    // unsent depending on what the row was.
    To(agent::session::EntryId),
    // Stop the run: from esc, or from the phone's `/stop`.
    Interrupt,
    // Esc caught a prompt on its way out: stop the run, then unsend it.
    Unsend,
}

impl Deed {
    // Whether it may happen now. Only the two that rewrite the transcript
    // care: the run in flight is writing it.
    fn fate(&self) -> Fate {
        match self {
            Deed::Rewind | Deed::To(_) => {
                Fate::Refused("rewinding needs the transcript this run is writing — esc first")
            }
            _ => Fate::Now,
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
    // Something to carry out, once the select's borrows are gone.
    Do(Asked),
    // A line pi relays for the model: always a turn, never a command.
    Relay(String, job::Relay),
    // A turn ended and has to be settled, whichever lane it belongs to.
    Turn(Done),
    // Something only the screen cares about.
    Nothing,
    // Leave. Not a `break` at the arm: the way out has lanes to settle.
    Leave,
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
    pub fn new(mut core: Core, keys: Arc<Keys>, drivers: Drivers) -> Result<Self> {
        // The screen first, for raw mode: the answer to the background query
        // carries no newline, so a cooked read would wait for one forever.
        let screen = Screen::new()?;
        let answer = tty::background();
        let paint = Paint::with_theme(
            true,
            Arc::new(following_terminal(&core.config.theme, answer.bg)),
        );
        let mut ui = Ui::new(
            screen,
            keys,
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
        let mut opening = View::opening(core.lane().resolved(), &ui.paint);
        opening.model = core.lane().model().to_string();
        let mut views = Views::new();
        views.insert(core.lane().token(), opening);
        drop_shared_history();
        {
            let lines = history_of(&core.store, core.lane().root());
            ui.editor.seed_history(lines);
        }
        // A resumed session shows its transcript from the start: the whole
        // screen is rebuildable now, so there is no reason to hide it.
        let token = core.lane().token();
        if let Some(session) = core.lane().session().filter(|s| !s.is_empty()) {
            ui.rebuild(view_at(&mut views, token), session);
        }
        let (events, hold) = reader();
        Ok(Self {
            core,
            views,
            ui,
            events,
            hold,
            drivers,
            bar: bar::BarScript::find(),
        })
    }

    // A surface on an in-memory screen, for tests driving the loop's
    // settle side. No reader thread, no history file to touch.
    #[cfg(test)]
    fn on_test_screen(mut core: Core, keys: Arc<Keys>) -> Self {
        let paint = Paint::with_theme(false, Arc::new(core.config.theme.clone()));
        let mut ui = Ui::new(
            screen::Screen::test(80, 24),
            keys,
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
            drivers: Drivers::new(Vec::new()),
            bar: bar::BarScript::at(None),
        }
    }

    // Lands a line (screen + history) from any door, once per line —
    // history is written now, not on exit, which two Ctrl-Cs can skip.
    fn echo_sent(&mut self, line: &str) {
        let view = front_view(&mut self.views, self.core.lane());
        self.ui.submit(view, line);
        view.surface.scroll = 0;
        self.save_history();
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
        self.ui.lists.at(self.core.lane().root());
        self.ui.at_root = self.core.lane().root().to_path_buf();
        self.ui.at_menu = None;
        // Recall follows the checkout like the lists do; the line just
        // typed is already filed (`save_history` ran while still in front).
        let lines = history_of(&self.core.store, self.core.lane().root());
        self.ui.editor.seed_history(lines);
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

    // A handled command's lines are its answer, and the answer goes to the
    // reply — the screen is not a log of what the user typed at the interface.
    fn land_lines(&mut self, rows: Listing) {
        land_handled(
            &mut self.ui,
            &self.core,
            front_view(&mut self.views, self.core.lane()),
            rows,
        );
    }

    fn land_swap(&mut self, said: Listing) {
        // Carries its own transcript: the lane it moves to was opened
        // with one, even if the lane being left has a run writing it.
        self.rebuild_front();
        // `at` forgets the lists: a swap that didn't move repeats the root,
        // dropping them either way.
        self.ui.lists.at(self.core.lane_mut().root());
        self.ui.open_reply(said);
    }

    // Take what every lane's run has posted since the last look into its own
    // view, drawn or not: its meter, filed rows and end happen when they do.
    async fn serve_lanes(&mut self) {
        for at in 0..self.core.lanes.len() {
            while let Ok(event) = self.core.lanes[at].inbox().try_recv() {
                // Every lane, not just the one in front: a turn a channel
                // asked for is still owed its answer after a switch.
                self.drivers.observe(self.core.lanes[at].token(), &event);
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
        self.ui
            .rebuild(front_view(&mut self.views, self.core.lane()), session);
    }

    // A lane's news goes to its own screen, never the one in front; a
    // never-drawn lane gets its banner first so it can't replace the row.
    fn say_of(&mut self, lane: usize, what: impl Into<Line<'static>>) {
        let view = view::opened(&mut self.views, &self.core.lanes[lane], &self.ui.paint);
        self.ui.say_line(view, what.into());
    }

    // Deeds the screen keeps for itself: only the surface can move the
    // screen, keyboard, and process, and knows which lane is in front.
    async fn carry(&mut self, deed: Deed) {
        match deed {
            Deed::Nothing => {}
            Deed::Interrupt => self.stop_current(false),
            Deed::Unsend => self.stop_current(true),
            Deed::Rewind => self.open_rewind(),
            Deed::To(id) => self.rewind_turn(id),
            Deed::External => self.edit_externally().await,
        }
    }

    // The one gate every input passes, so two ways of asking the same
    // thing can't get different answers. `fate` decides only mid-run.
    fn admit(&mut self, asked: Asked, origin: Origin, line: Option<String>) -> Wake {
        if !self.core.lane().is_running() {
            return Wake::Do(asked);
        }
        let intent = match asked {
            // A deed is the screen's own move: it never waits for the run,
            // because waiting is one of the answers about lines.
            Asked::Own(deed) => match deed.fate() {
                // A key, not a command: a deed's refusal is the answer to a
                // press, and the reply is for what a slash command answered.
                Fate::Refused(why) => {
                    self.ui.flash(why);
                    return Wake::Nothing;
                }
                Fate::Now => return Wake::Do(Asked::Own(deed)),
                Fate::Queued => unreachable!("a deed waits for nothing"),
            },
            Asked::Core(intent) => intent,
        };
        match intent.fate() {
            Fate::Now => Wake::Do(Asked::Core(intent)),
            Fate::Queued => {
                front_view(&mut self.views, self.core.lane())
                    .queued
                    .push(Queued {
                        intent,
                        origin,
                        line,
                    });
                Wake::Nothing
            }
            // A command refused because a run is in flight. It was typed, so
            // its refusal is that command's answer.
            Fate::Refused(why) => {
                self.ui.open_reply(Listing::say([why]));
                Wake::Nothing
            }
        }
    }

    // A job asked to stop the turn running in its checkout, as esc would;
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
        front_view(&mut self.views, self.core.lane()).state.stopping = true;
    }

    /// Drives the terminal until the user leaves. Each lane owns the
    /// channel its runs post to, so a lane left behind keeps working.
    pub async fn run(mut self) -> Result<()> {
        // Every lane's runs report here when they end. One channel rather than a
        // handle per lane: the loop waits on it like any other source.
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
        // An MCP prompt's text, back from its server: the lane it was asked
        // on, who asked, the line as typed, and the text or why there is none.
        let (fetched_tx, mut fetched_rx) =
            tokio::sync::mpsc::unbounded_channel::<(u64, Origin, String, Result<String, String>)>();
        let mut tick = tokio::time::interval(status::SPIN);
        let mut looked = std::time::Instant::now();
        // Off while nothing runs, so the interval falls behind; one
        // late tick then ordinary cadence avoids a catch-up spin.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            self.serve_lanes().await;
            // Before the bar rebuilds, so a vanished tab goes with its lane.
            self.drop_vanished_lanes();
            prune_views(&self.core, &mut self.views);
            let core = &self.core;
            for said in self.drivers.retain(|t| core.position_of(t).is_some()) {
                self.ui
                    .say(front_view(&mut self.views, self.core.lane()), said);
            }
            // What changed on disk is taken up within a second: settings,
            // instructions and memory resolve the lane again, a skill's word
            // starts to answer. Not on every frame: each look is a walk.
            if looked.elapsed() >= std::time::Duration::from_secs(1) {
                looked = std::time::Instant::now();
                if let Some(said) = self.core.refresh_config() {
                    let view = front_view(&mut self.views, self.core.lane());
                    for line in said {
                        self.ui.say_muted(view, line);
                    }
                    self.ui.adopt_config(&self.core, view);
                }
                if self.core.refresh_prompts() {
                    self.ui.commands = self.core.commands.clone();
                }
                if let Some(said) = self.core.refresh_skills() {
                    self.ui.commands = self.core.commands.clone();
                    let view = front_view(&mut self.views, self.core.lane());
                    for line in said {
                        self.ui.say_muted(view, line);
                    }
                }
            }
            self.refresh_tabs();
            self.bar.poke(self.core.lane());
            let jobs = self.drivers.jobs();
            for root in jobs.take_interrupts() {
                self.interrupt_at(&root);
            }
            self.ui.jobs = jobs.jobs(self.core.lane().root());
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
            // A driver's line goes only when the lane is free and nothing typed
            // is waiting: what the user says comes first.
            let free = !running && !waiting;
            let driven = free.then(|| self.driver_line()).flatten();
            let lane = self.core.lane();
            let woke = if let Some((from, wake)) = driven {
                origin = from;
                wake
            } else if !waiting || running {
                let bar_due = self.bar.due().map(tokio::time::Instant::from_std);
                // Only while the lane is free: a prompt due under a run would
                // wake this loop again and again with nothing it may do.
                let jobs = self.drivers.jobs();
                let due_back = free.then(|| jobs.next_due(lane.root())).flatten();
                // Every branch must be cancel-safe: a loser is dropped mid-poll.
                // `recv()` and `tick()` are; a blocking read gets its own thread.
                tokio::select! {
                    Some(done) = done_rx.recv() => Wake::Turn(done),
                    Some((token, from, typed, text)) = fetched_rx.recv() => {
                        let front = self.core.lane().token() == token;
                        let started = match text {
                            Err(why) => {
                                self.ui.flash(why);
                                false
                            }
                            Ok(send) if front && !self.core.lane().is_running() => {
                                self.start_turn(send, Some(typed), &done_tx);
                                true
                            }
                            Ok(_) if !front => {
                                self.ui.flash(format!(
                                    "{typed} came back after its lane left the front; run it again"
                                ));
                                false
                            }
                            Ok(_) => {
                                self.ui.flash(format!(
                                    "{typed} came back while another turn ran; run it again"
                                ));
                                false
                            }
                        };
                        // A driver hears of the turn only now that it began.
                        if from != Origin::Typed
                            && let Some(at) = self.core.position_of(token)
                            && let Some(said) = self.drivers.dispatched(from, token, started, true)
                        {
                            self.say_of(at, said);
                        }
                        Wake::Nothing
                    }
                    // Only while something runs, or a flash is up — an idle
                    // loop waking ten times a second has nothing to spin.
                    _ = tick.tick(), if anywhere || jobs.working(lane.root()) || self.ui.flash.is_some() || self.ui.copied.is_some() => {
                        self.ui.spinner += 1;
                        Wake::Nothing
                    }
                    Some(out) = self.bar.rx.recv() => {
                        if let Some(why) = self.bar.land(out, &mut self.ui.layout) {
                            self.ui.say_muted(front_view(&mut self.views, self.core.lane()), why);
                        }
                        Wake::Nothing
                    }
                    _ = tokio::time::sleep_until(bar_due.unwrap_or_else(tokio::time::Instant::now)),
                        if bar_due.is_some() => Wake::Nothing,
                    // Something was left, cancelled or finished: look again.
                    _ = jobs.changed().notified() => Wake::Nothing,
                    _ = tokio::time::sleep_until(due_back.unwrap_or_else(tokio::time::Instant::now)),
                        if due_back.is_some() => Wake::Nothing,
                    key = self.events.recv() => match key {
                        Some(key) => {
                            let lane = self.core.lane();
                            let view = front_view(&mut self.views, lane);
                            let intent = self.ui.key(lane, view, key, running);
                            if self.ui.took_submit() {
                                self.save_history();
                            }
                            let held = self.ui.held.take();
                            self.admit(intent, Origin::Typed, held)
                        }
                        None => Wake::Leave,
                    },
                    msg = self.drivers.inbound() => match msg {
                        // The phone types at the lane in front, like a
                        // hand; its `/stop` is esc — same gate, same intents.
                        Some((from, channel::Inbound::Text { text })) => {
                            origin = from;
                            let intent = input::read(&text, &self.core.commands);
                            if intent.echoed() {
                                self.echo_sent(&text);
                            }
                            self.admit(Asked::Core(intent), origin, None)
                        }
                        Some((_, channel::Inbound::Stop)) => {
                            self.admit(Asked::Own(Deed::Interrupt), Origin::Typed, None)
                        }
                        // The QR, an error, a way out of one: on the lane the
                        // channel follows, where it lasts and can be re-read.
                        Some((_, channel::Inbound::Notice(text))) => {
                            self.ui.say(front_view(&mut self.views, self.core.lane()), text);
                            Wake::Nothing
                        }
                        // A channel saying it is up: one row for a moment.
                        Some((_, channel::Inbound::Flash(text))) => {
                            self.ui.flash(text);
                            Wake::Nothing
                        }
                        None => Wake::Nothing,
                    },
                }
            } else {
                // One at a time, each still the intent it was read as.
                // Joined as lines, a command+prompt would lose its first word.
                let queued = front_view(&mut self.views, self.core.lane())
                    .queued
                    .remove(0);
                origin = queued.origin;
                if let Some(line) = &queued.line {
                    let view = front_view(&mut self.views, self.core.lane());
                    self.ui.submit(view, line);
                    view.surface.scroll = 0;
                }
                Wake::Do(Asked::Core(queued.intent))
            };
            // Out here, where all of `self` is free again.
            let asked = match woke {
                Wake::Turn(done) => {
                    self.settle(done).await;
                    continue;
                }
                Wake::Nothing => continue,
                Wake::Leave => break,
                // Its driver hears nothing of the turn, so nothing follows.
                Wake::Relay(line, relay) => {
                    self.start_relayed(line, relay, &done_tx);
                    continue;
                }
                Wake::Do(asked) => asked,
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
            let was = self.core.current;
            let asked_on = self.core.lane().token();
            let step = self.core.dispatch(intent);
            self.reconcile(was);
            let prompt = matches!(step, Step::Prompt { .. });
            // Its turn starts when the server answers; drivers hear then.
            let deferred = matches!(step, Step::McpPrompt { .. });
            match step {
                Step::Quit => break,
                // A refusal is an answer: it was asked for by a line, so it
                // goes where every other answer does.
                Step::Flash(line) => self.ui.open_reply(Listing::say([line])),
                Step::Bash(command) => self.start_bash(command, &done_tx),
                Step::McpPrompt {
                    prompt,
                    args,
                    typed,
                } => {
                    self.ui.flash(format!("asking {} …", prompt.word()));
                    let (tx, token) = (fetched_tx.clone(), self.core.lane().token());
                    let from = origin;
                    tokio::spawn(async move {
                        const WAIT: std::time::Duration = std::time::Duration::from_secs(30);
                        let text = tokio::time::timeout(WAIT, prompt.text(&args))
                            .await
                            .unwrap_or_else(|_| Err(format!("{} did not answer", prompt.word())));
                        let _ = tx.send((token, from, typed, text));
                    });
                }
                Step::Swap(said) => self.land_swap(said),
                Step::Worktrees(lines) => {
                    self.land_lines(lines);
                    // A checkout went; the cached list would go on offering it.
                    self.ui.lists.forget();
                }
                Step::Edit(file) => {
                    self.edit_file(file).await;
                    // A file saved for the first time is listed as there now.
                    self.ui.lists.forget();
                }
                Step::Handled(lines) => self.land_lines(lines),
                Step::Compact(focus) => self.start_compact(focus, &done_tx),
                Step::Drive(drive) => {
                    let lane = self.core.lane();
                    match self.drivers.command(drive, lane.token(), lane.ctx()) {
                        Said::Nothing => {}
                        Said::Reply(lines) => self.ui.open_reply(Listing::say(lines)),
                        Said::Transcript(line) => self
                            .ui
                            .say(front_view(&mut self.views, self.core.lane()), line),
                    }
                }
                // Queued-during-run entries are taken one at a time at the
                // loop's top; draining here would merge them into one prompt.
                Step::Prompt { send, typed } => self.start_turn(send, typed, &done_tx),
            }
            // The driver that sent this line hears the end of the turn it began.
            // By token: the step may have moved the surface to another lane.
            if origin != Origin::Typed
                && !deferred
                && let Some(at) = self.core.position_of(asked_on)
            {
                let started = self.core.lanes[at].is_running();
                if let Some(said) = self.drivers.dispatched(origin, asked_on, started, prompt) {
                    self.say_of(at, said);
                }
            }
        }
        self.save_history();
        self.settle_all(&mut done_rx).await;
        Ok(())
    }

    // The line a driver sends the front lane next, drawn and its spend
    // charged, with who sent it: a relay opens a turn, anything else is read.
    fn driver_line(&mut self) -> Option<(Origin, Wake)> {
        let lane = self.core.lane();
        let next = self.drivers.next(lane.token(), lane.root())?;
        self.core.lane_mut().charge(&next.spent);
        let view = front_view(&mut self.views, self.core.lane());
        view.surface.scroll = 0;
        // A job's result is relayed under its label; a loop's line is read as
        // typed. A person's line through a job is always a prompt: what they
        // send never runs here as a command, and every one is answered.
        let wake = match next.label {
            Some(label) => {
                self.ui.submit_relayed(view, &label, &next.line);
                let relay = job::Relay {
                    label,
                    note: next.note,
                };
                Wake::Relay(next.line, relay)
            }
            None => {
                let intent = match next.origin {
                    Origin::Input(_) => Intent::Prompt(next.line.clone()),
                    _ => input::read(&next.line, &self.core.commands),
                };
                self.ui.submit(view, &next.line);
                if !next.note.is_empty() {
                    self.core.lane_mut().push_note(&next.note);
                }
                Wake::Do(Asked::Core(intent))
            }
        };
        Some((next.origin, wake))
    }

    // Cut the transcript at an entry — chosen from the selector, or the
    // prompt an Esc took back — and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        match self.core.rewind_to(id) {
            Ok(Rewound::Nothing) => {
                self.ui.flash(NOTHING_TO_REWIND);
            }
            Ok(outcome) => {
                // The transcript is truth again: rebuild the whole view
                // from it, back to the node the conversation returned to.
                self.rebuild_front();
                let said = match outcome {
                    Rewound::Unsent(_) if !self.ui.editor.is_empty() => {
                        "unsent — the line you were typing stands; Up recalls it".to_string()
                    }
                    // Unsent is half-typed, not gone: the text goes back to
                    // the editor, and the cursor on it says what happened.
                    Rewound::Unsent(text) => {
                        self.ui.editor.set_line(&text);
                        return;
                    }
                    Rewound::Kept | Rewound::Nothing => {
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
                self.ui
                    .say_muted(front_view(&mut self.views, self.core.lane()), &said);
            }
            Err(e) => {
                self.ui.say(
                    front_view(&mut self.views, self.core.lane()),
                    core::not_saved(&e),
                );
            }
        }
    }

    // Open the rewind selector on what the user said: a rewind takes a prompt
    // back, and an answer is a place the conversation carries on from.
    fn open_rewind(&mut self) {
        // A lane whose transcript a job took and could not read back has
        // nothing to go back to; said, not panicked.
        let Some(session) = self.core.lane().session() else {
            self.ui.flash(NOTHING_TO_REWIND);
            return;
        };
        let rows: Vec<MenuEntry> = session
            .rewind_nodes()
            .into_iter()
            .map(|node| MenuEntry::Message {
                id: node.id(),
                show: pi_store::text::clip(node.show(), 60),
            })
            .collect();
        if rows.is_empty() {
            self.ui.flash(NOTHING_TO_REWIND);
            return;
        }
        self.ui.open_rewind(rows);
    }
}

#[cfg(test)]
mod tests;
