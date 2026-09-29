//! The interactive surface: one owner of the terminal for the whole session.
//!
//! The line-editing library that used to sit here owned the terminal only while
//! it was reading a line, which is what made a key press during a run
//! unreachable and left the renderer writing into a terminal nobody was
//! managing. Here a single loop holds raw mode from start to finish and
//! services three sources at once — the agent's events, the keyboard, and a
//! timer for the animation — so nothing has to be bolted on beside it.

mod browse;
mod call;
mod editor;
mod job;
mod menu;
mod mouse;
mod reply;
mod row;
mod screen;
mod scrollback;
mod stream;
mod term;
mod ui;
mod view;
mod vim;

use agent::session::EntryId;
use anyhow::Result;
use crossterm::event::Event as TermEvent;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::core::lane::Run;
use crate::core::{self, Core};
use crate::driver::{Drivers, Origin, Said};
use crate::input::{self, Fate, Intent, Rewound, Step};
use crate::store::keys::Keys;
use crate::store::listing::Listing;
use crate::ui::render::Paint;
use crate::ui::status;
use crate::ui::tty;
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

// How close two Ctrl-C presses must be to read as one deliberate quit.
//
// Borrowed from pi, which uses the same 500ms. A latching flag looks simpler
// and is wrong: clear one half-typed line, type another, clear that — and the
// second clear reads as the second half of a double-tap and quits.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

// How long a flash stays on the bar row: long enough to read a short line
// without looking for it, short enough that a second try lands after it.
const FLASH: std::time::Duration = std::time::Duration::from_secs(1);

// The bar's own row: present whatever the bar has to say, because a row that
// came and went would take the transcript above it along on every key that
// missed. Every terminal tall enough to hold it gives it this one.
const BAR_H: usize = 1;

// One text each: three and two call sites had their own copy of these, and a
// reworded one would have drifted.
const NO_TRANSCRIPT: &str = "this checkout has no transcript — /new or /resume first";
const NOTHING_TO_REWIND: &str = "nothing to rewind to";

// What a `Step::Handled` leaves behind: its lines as the reply, and whatever
// the command changed under the surface. A free function because a run in
// flight lands them from inside its own borrow, where `self` is in pieces.
fn land_handled(ui: &mut Ui, core: &Core, view: &mut View, rows: Listing) {
    ui.open_reply(rows);
    ui.adopt_config(core, view);
}

// A line submitted while the lane in front is working. What it may do is
// settled before it runs: `command` cannot be asked and then ignored, because
// asking is doing.
//
// Only queueing and refusing happen here. Anything that may run now goes back
// to the loop's own dispatch, so a command takes the same path whether or not
// a run is under way — and a `/worktree` that opens a lane is landed by the
// same code that lands it from an idle prompt.

// What the screen does to the lane itself, rather than asking the core to
// answer something. A key can mean these and a line cannot: there is no
// `/rewind` word, and `/new` and ctrl+l twice are one intent that goes the
// other way round. They are answered one step early — `admit` says whether the
// run in flight allows them — and then carried out here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Deed {
    // A key that moved the editor and nothing else.
    Nothing,
    // The line being typed wants `$EDITOR`. The surface's own: the editor takes
    // the terminal, which only the surface knows how to give away.
    External,
    // The rewind selector wants everywhere the session can go back to: only
    // the surface can put a screen over the transcript.
    Rewind,
    // A row chosen from it: the conversation rewinds there, and what the row
    // was decides whether it is kept or unsent.
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

// What a key asked for. The core answers an `Intent` however it arrived — the
// keyboard, the phone, a typed line — and `Deed` is what the screen keeps for
// itself, so the two never have to be told apart again downstream.
#[derive(Debug)]
enum Asked {
    Core(Intent),
    Own(Deed),
}

enum Wake {
    // Something to carry out, once the select's borrows are gone.
    Do(Asked),
    // A turn ended and has to be settled, whichever lane it belongs to.
    Turn(Done),
    // Something only the screen cares about.
    Nothing,
    // Leave. Not a `break` at the arm: the way out has lanes to settle.
    Leave,
}

pub struct Tui {
    core: Core,
    // What each lane looks like, keyed by lane token. Held here rather than on
    // the lane because a screen is the surface's: the lane list reorders and
    // drops lanes, and a screen joined to a lane by identity cannot end up
    // drawn for the wrong one when it does.
    views: Views,
    ui: Ui,
    events: UnboundedReceiver<TermEvent>,
    // Stops the reader while a child holds the terminal.
    hold: Hold,
    drivers: Drivers,
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
        })
    }

    // A surface on an in-memory screen, for tests that drive the loop's
    // settle side. No reader thread and no history file: the terminal the
    // test runner owns is not this test's to touch.
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
        }
    }

    // A line whose answer will land under it: onto the screen, at the newest
    // row, and into the history file.
    //
    // One place rather than one per door. A fresh turn starts at the newest
    // row — a view scrolled up to read would otherwise stream output out of
    // sight — and the history is written per line rather than on the way out,
    // because quitting with two Ctrl-Cs skips every tidy exit path there is.
    // Every door a line can be submitted through calls this — the keyboard and
    // the phone alike — because a line the user cannot see they sent is one
    // they send twice.
    //
    // Which lines those are is `Intent::echoed`'s to say: a command answers
    // over the menu in the reply, which is dismissed rather than kept, and a
    // row left above an answer that never comes is the question standing alone.
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

    // Sessions change on the commands that create, delete or switch them;
    // refresh the copy the completion menu reads.
    // A turn or a switch can change what `/resume` would list. Dropped
    // rather than recomputed: whoever asks next pays, and most of the time
    // nobody does.
    fn refresh_sessions(&mut self) {
        self.ui.lists.forget();
    }

    // Bring the screen into step with the lanes after a command. A switch
    // parks the line being typed in the view of the lane it was typed at
    // and takes the lane in front's own parked line back up; the lists
    // follow, and what that lane posted while away is replayed here.
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
        // Recall follows the checkout for the same reason the lists do. The
        // line just typed is already filed: `save_history` runs per line, and
        // ran while this lane was still the one in front.
        let lines = history_of(&self.core.store, self.core.lane().root());
        self.ui.editor.seed_history(lines);
        // A lane opened later has no banner yet, and the files it stands on
        // are its own. Nothing reads the screen here: the events below ask for
        // it again, each in the lane it belongs to.
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
        // A swap lands carrying its own transcript: the lane it moves to was
        // opened with one, so this reads something even when the lane being
        // left has a run writing it.
        self.rebuild_front();
        // `at` forgets both lists, so it stands in for `refresh_sessions`: a
        // swap that did not move repeats the root, and drops them either way.
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

    // Rebuild the bar before every draw: a run that ended out of sight has to
    // reach the screen without anyone asking, and a step walks the order this
    // builds — the bar and the ring are one list.
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

    // Drop lanes whose checkout was deleted outside pi — idle ones only, a
    // running or looping lane still answering to the index it was given.
    //
    // Silent: the lane going off the bar is what says the checkout is gone.
    fn drop_vanished_lanes(&mut self) {
        let mut gone: Vec<usize> = Vec::new();
        // Back to front, stopping at a working lane: removing one before it
        // would shift the index a run in flight reports back by. That lane's
        // turn over, the next pass drops what this one left.
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

    // The front lane's screen out of its transcript: what a swap and a rewind
    // both need, and neither has anything to add to it.
    //
    // A lane whose transcript a job took and could not read back has nothing to
    // rebuild from. That state is named — `NO_TRANSCRIPT` — rather than fatal,
    // so the screen is left as it stands.
    fn rebuild_front(&mut self) {
        let Some(session) = self.core.lane().session() else {
            return;
        };
        self.ui
            .rebuild(front_view(&mut self.views, self.core.lane()), session);
    }

    // A lane's news goes into that lane's own screen, never the one in front:
    // it is read back beside the conversation it happened to. A lane never
    // drawn gets its opening block first, so `reconcile` cannot replace the row.
    fn say_of(&mut self, lane: usize, what: impl Into<Line<'static>>) {
        let view = view::opened(&mut self.views, &self.core.lanes[lane], &self.ui.paint);
        self.ui.say_line(view, what.into());
    }

    // The deeds the screen keeps for itself. They are here rather than in
    // `dispatch` because only the surface can move the screen, the keyboard and
    // the process — and only the surface knows which lane is in front.
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

    // The one gate every input passes: a key, the phone, or an intent coming
    // back off the queue, so two ways of asking the same thing cannot get two
    // different answers.
    //
    // The answer is in two halves and they live apart on purpose: whether a
    // run is in flight is state, and is read here; what a question may do
    // while one is, is a property of the question, and `fate` holds it. An
    // idle lane admits everything, which is why `fate` never has to mention
    // idleness — and why it is asked only once a run is known to be there.
    fn admit(&mut self, asked: Asked, origin: Origin) -> Wake {
        if !self.core.lane().is_running() {
            return Wake::Do(asked);
        }
        let intent = match asked {
            // A deed is the screen's own move: it is never queued and never
            // steered, because waiting is one of the four answers about lines.
            Asked::Own(deed) => match deed.fate() {
                // A key, not a command: a deed's refusal is the answer to a
                // press, and the reply is for what a slash command answered.
                Fate::Refused(why) => {
                    self.ui.flash(why);
                    return Wake::Nothing;
                }
                Fate::Now => return Wake::Do(Asked::Own(deed)),
                Fate::Queued | Fate::Steered(_) => {
                    unreachable!("a deed waits for nothing")
                }
            },
            Asked::Core(intent) => intent,
        };
        match intent.fate() {
            Fate::Now => Wake::Do(Asked::Core(intent)),
            Fate::Queued => {
                front_view(&mut self.views, self.core.lane())
                    .queued
                    .push(Queued { intent, origin });
                Wake::Nothing
            }
            Fate::Steered(text) => {
                // A `!` or a `/compact` holds the lane: nothing is listening,
                // so the line waits for it the way every line used to.
                let Some(steer) = self.core.lane().steer().cloned() else {
                    front_view(&mut self.views, self.core.lane())
                        .queued
                        .push(Queued { intent, origin });
                    return Wake::Nothing;
                };
                // Spends the chance to unsend, exactly as the model's first
                // word does: esc now means stop. Without this, esc after a
                // line was said takes the prompt back and the line — never
                // heard, handed back at `finish` — starts a turn of its own,
                // which is the opposite of what the user just asked for.
                front_view(&mut self.views, self.core.lane())
                    .state
                    .committed = true;
                self.drivers.steered(self.core.lane().token(), origin);
                steer.say(text);
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

    /// Drive the terminal until the user leaves.
    ///
    /// No event channel is handed in any more: each lane owns the one its runs
    /// post to, which is what lets a lane the screen has moved on from keep
    /// working without its output landing on somebody else's view.
    pub async fn run(mut self) -> Result<()> {
        // Every lane's runs report here when they end. One channel rather than a
        // handle per lane: the loop waits on it like any other source.
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
        let mut tick = tokio::time::interval(status::SPIN);
        // The branch is off while nothing runs, so the interval falls behind
        // the clock; bursting to catch up would spin the loop the moment a run
        // starts. One late tick, then the ordinary cadence.
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
            self.refresh_tabs();
            let view = front_view(&mut self.views, self.core.lane());
            self.ui.flush(self.core.lane(), view);
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
            let next = (!running && !waiting)
                .then(|| self.drivers.next(self.core.lane().token()))
                .flatten();
            let woke = if let Some(next) = next {
                origin = next.origin;
                let view = front_view(&mut self.views, self.core.lane());
                self.ui.submit(view, &next.line);
                view.surface.scroll = 0;
                if !next.note.is_empty() {
                    self.core.lane_mut().push_note(&next.note);
                }
                Wake::Do(Asked::Core(input::read(&next.line, &self.core.commands)))
            } else if !waiting || running {
                // Every branch must be cancel-safe: a loser is dropped mid-poll.
                // `recv()` and `tick()` are; a blocking read gets its own thread.
                tokio::select! {
                    Some(done) = done_rx.recv() => Wake::Turn(done),
                    // Only while something runs, or a flash is up — an idle
                    // loop waking ten times a second has nothing to spin.
                    _ = tick.tick(), if anywhere || self.ui.flash.is_some() => {
                        self.ui.spinner += 1;
                        Wake::Nothing
                    }
                    key = self.events.recv() => match key {
                        Some(key) => {
                            let lane = self.core.lane();
                            let view = front_view(&mut self.views, lane);
                            let intent = self.ui.key(lane, view, key, running);
                            if self.ui.took_submit() {
                                self.save_history();
                            }
                            self.admit(intent, Origin::Typed)
                        }
                        None => Wake::Leave,
                    },
                    msg = self.drivers.inbound() => match msg {
                        // The phone types at the lane in front, like a hand,
                        // and its `/stop` is esc. Same intents, same gate, so
                        // they cannot drift apart.
                        Some((from, channel::Inbound::Text { text })) => {
                            origin = from;
                            let intent = input::read(&text, &self.core.commands);
                            if intent.echoed() {
                                self.echo_sent(&text);
                            }
                            self.admit(Asked::Core(intent), origin)
                        }
                        Some((_, channel::Inbound::Stop)) => {
                            self.admit(Asked::Own(Deed::Interrupt), Origin::Typed)
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
                // One at a time, each still the intent it was read as. Joined
                // as lines, a command and a prompt became one line and `read`
                // saw only the first word.
                let queued = front_view(&mut self.views, self.core.lane())
                    .queued
                    .remove(0);
                origin = queued.origin;
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
            match step {
                Step::Quit => break,
                // A refusal is an answer: it was asked for by a line, so it
                // goes where every other answer does.
                Step::Flash(line) => self.ui.open_reply(Listing::say([line])),
                Step::Bash(command) => self.start_bash(command, &done_tx),
                Step::Swap(said) => self.land_swap(said),
                Step::Worktrees(lines) => {
                    self.land_lines(lines);
                    // A checkout went; the cached list would go on offering it.
                    self.ui.lists.forget();
                }
                Step::EditConfig(file) => self.edit_config(file).await,
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
                // What was submitted while the run worked is taken up by the
                // top of this loop, one entry at a time and each read as what
                // it is. Draining it here instead meant everything queued
                // became the next prompt, whatever it had been typed as.
                Step::Prompt { send, typed } => self.start_turn(send, typed, &done_tx),
            }
            // The driver that sent this line hears the end of the turn it began.
            // By token: the step may have moved the surface to another lane.
            if origin != Origin::Typed
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

    // Cut the transcript at an entry — chosen from the selector, or the
    // prompt an Esc took back — and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        match self.core.rewind_to(id) {
            Ok(Rewound::Nothing) => {
                self.ui.flash(NOTHING_TO_REWIND);
            }
            Ok(outcome) => {
                // The transcript is the source of truth again: rebuild the
                // whole view from it, so the screen returns to the node the
                // conversation did instead of keeping the forgotten turns.
                // It clears anything said before it: hence the notice after.
                // A rewind is refused while a run has the transcript.
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
                            .map(|n| crate::text::clip(n.show(), 60))
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
                show: crate::text::clip(node.show(), 60),
                help: "you — unsends it",
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
