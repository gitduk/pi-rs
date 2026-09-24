//! One run's life, from the moment it is armed to the moment it is closed:
//! starting a turn, a `!` line or a `/compact` off the loop, and putting the
//! transcript back when it comes home.
//!
//! The loop in `mod.rs` starts these and stops them; what happens in between —
//! what a lane is charged, what the screen is told, which lane's line is drawn
//! where — is here.

use agent::session::{Entry as LogEntry, Session};
use agent::{AgentError, Event};
use futures::FutureExt;
use ratatui::text::{Line, Span};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::row::Row;
use super::term::{Deafened, EXIT_GRACE, external_editor, scratch_file};
use super::view::{Queued, front_view, tail_of, view_at};
use super::{NO_TRANSCRIPT, Tui};
use crate::app;
use crate::app::looping::Cut;
use crate::input::Intent;
use crate::store::listing::Listing;

// What kind of job a finished `Done` was, carrying what only that kind
// leaves behind — so no arm can be built holding another's.
pub(super) enum Kind {
    // A turn: the agent loop ran, and `ran` says how it went. Its output
    // reached the view through the lane's event channel as it happened.
    Turn,
    // A `!` command, and the lines it printed. They come home whole rather
    // than as events, so settling is the only place they can be shown.
    //
    // Kept apart from a turn's silence because `Bridge` is one accumulator
    // for the whole surface, filled by whichever turn is streaming and
    // emptied only by `Event::TurnStart`; a `!` emits no events, so letting
    // one speak for the bridge flushes another lane's half-written answer to
    // the phone as though it were finished.
    Bash {
        // The output lines, derived by the same function the rebuild draws
        // with — plus the flash for a command that never ran.
        lines: Vec<String>,
    },
    // A `/compact`, and what it shrank and spent. None means the transcript
    // already fit — or, with a cancelled `ran`, that nobody ever looked.
    Compact(Option<(agent::Report, llm::stream::Usage)>),
}

// A job that ran off the loop, reporting back to the loop that started it.
//
// The transcript comes home this way rather than through a `JoinHandle`, so
// one channel serves every lane and nothing has to poll a growing list of
// them. `ran` is None only when the job panicked and took its copy down.
pub(super) struct Done {
    pub(super) token: u64,
    pub(super) kind: Kind,
    // The transcript back, and how the job went. None when it panicked and
    // took its copy down with it — one field, because those two are never
    // separately absent.
    pub(super) ran: Option<(Session, Result<llm::stream::Usage, AgentError>)>,
}

// Run `job` off the loop, turning a panic into a `None` the settle side can
// act on. One guard for every task that carries the transcript, so the
// panic contract is written once: a job that never reports leaves its lane
// looking "working" forever.
pub(super) async fn guard<F, T>(job: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    std::panic::AssertUnwindSafe(job).catch_unwind().await.ok()
}

impl Tui {
    // Start a turn on the lane in front and come straight back.
    //
    // The run keeps the transcript for its length and posts what it is doing
    // to that lane's own channel, so the loop is free to draw, read keys and
    // serve the other lanes — including this one after the screen moves on.
    // Hand the view over to a job about to start: the clock runs, the run's
    // own figures start at nothing, and the session's earlier runs are handed
    // to the tally as the base `/status` reports against.
    // `committed` says whether the prompt behind it can still be taken back.
    pub(super) fn arm_view(&mut self, committed: bool) {
        let view = front_view(&mut self.views, self.core.lane());
        view.state.started = Some(std::time::Instant::now());
        view.state.committed = committed;
        view.state.stopping = false;
        self.core.lane_mut().seed_meter();
    }

    pub(super) fn start_turn(
        &mut self,
        prompt: String,
        typed: Option<String>,
        done: &UnboundedSender<Done>,
    ) {
        // Lent to the run for the length of the turn. A lane with a run under
        // way refuses another, so the only way it is missing here is the lane
        // whose transcript a panic took and whose archive would not read back.
        let Some(mut carried) = self.core.lane_mut().take_session() else {
            self.ui.flash(NO_TRANSCRIPT);
            return;
        };
        carried.send_prompt(prompt, typed);
        // The repair results and the stop note the send filed are entries
        // now: derive their rows like any commit, so they show without a
        // rebuild. The ask itself stays unadopted — the door echoed it.
        let view = front_view(&mut self.views, self.core.lane());
        let tail = view.surface.tail;
        let fresh: Vec<LogEntry> = carried
            .entries()
            .iter()
            .filter(|e| tail.is_none_or(|t| e.id() > t) && !matches!(e, LogEntry::Ask { .. }))
            .cloned()
            .collect();
        self.ui.adopt(view, &fresh);
        let cancel = CancellationToken::new();
        let steer = agent::Steer::default();
        let ctx = self.core.lane_mut().ctx_for(cancel.clone());

        self.arm_view(false);
        // Read while the agent is still reachable: `/model` may replace it
        // while this run works, and the run keeps the one it started on.
        front_view(&mut self.views, self.core.lane()).model =
            self.core.lane_mut().model().to_string();

        let agent = self.core.lane_mut().agent().clone();
        let sent = self.core.lane_mut().sender().clone();
        let token = self.core.lane().token();
        let done = done.clone();
        // The run's own handle on the mailbox; the lane keeps the other.
        let heard = steer.clone();
        tokio::spawn(async move {
            let out = guard(agent.steered(&mut carried, &ctx, &sent, &heard)).await;
            let _ = done.send(Done {
                token,
                kind: Kind::Turn,
                ran: out.map(|out| (carried, out)),
            });
        });
        self.core.lane_mut().begin(cancel, Some(steer));
    }

    pub(super) fn start_bash(&mut self, command: String, done: &UnboundedSender<Done>) {
        let Some(mut carried) = self.core.lane_mut().take_session() else {
            self.ui.flash(NO_TRANSCRIPT);
            return;
        };
        let cancel = CancellationToken::new();
        let ctx = self.core.lane_mut().ctx_for(cancel.clone());
        // Committed forecloses `Deed::Unsend`, the only writer of
        // `Run::Running.unsend`, which a `!` has no prompt to honour.
        self.arm_view(true);

        let token = self.core.lane().token();
        let done = done.clone();
        tokio::spawn(async move {
            let out = guard(async move {
                let out = app::bash::run_bash(&ctx, &command).await;
                // Esc that stopped the `!` is a cancelled run too; `ran` says
                // so instead of a success that spent nothing.
                let ran = if ctx.cancel.is_cancelled() {
                    Err(AgentError::Cancelled)
                } else {
                    Ok(llm::stream::Usage::default())
                };
                app::bash::record_bash(&mut carried, &command, out.text.clone());
                (carried, ran, out.screen())
            })
            .await;
            // A panic printed nothing anyone can still show; the empty lines
            // and the missing transcript say the same thing from both sides.
            let (kind, ran) = match out {
                Some((carried, ran, lines)) => (Kind::Bash { lines }, Some((carried, ran))),
                None => (Kind::Bash { lines: Vec::new() }, None),
            };
            let _ = done.send(Done { token, kind, ran });
        });
        // Not a turn: nothing here calls a model, so there is no boundary at
        // which a line could be heard.
        self.core.lane_mut().begin(cancel, None);
    }

    // Run a `/compact` off the loop. It can spend real time — summarising
    // what it drops is a model call — and the lane must keep drawing and
    // serving the others meanwhile.
    pub(super) fn start_compact(&mut self, focus: Option<String>, done: &UnboundedSender<Done>) {
        let Some(mut carried) = self.core.lane_mut().take_session() else {
            self.ui.flash(NO_TRANSCRIPT);
            return;
        };
        let cancel = CancellationToken::new();
        // Work under way like any turn's, and nothing anyone can take back.
        self.arm_view(true);

        let agent = self.core.lane_mut().agent().clone();
        let token = self.core.lane().token();
        let done = done.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            let out = guard(async move {
                // Dropped mid-flight, not signalled: `compact_now` writes to
                // the transcript only once every await is behind it.
                let ran = tokio::select! {
                    got = agent.compact_now(&mut carried, focus.as_deref()) => Ok(got),
                    _ = stop.cancelled() => Err(AgentError::Cancelled),
                };
                (carried, ran)
            })
            .await;
            // A report only exists where the pass ran to the end: `Ok(None)`
            // is a transcript that already fit, `Err` one nobody looked at.
            let (kind, ran) = match out {
                Some((carried, Ok(got))) => (
                    Kind::Compact(got),
                    Some((carried, Ok(llm::stream::Usage::default()))),
                ),
                Some((carried, Err(e))) => (Kind::Compact(None), Some((carried, Err(e)))),
                None => (Kind::Compact(None), None),
            };
            let _ = done.send(Done { token, kind, ran });
        });
        // Not a turn: nothing here calls a model, so there is no boundary at
        // which a line could be heard.
        self.core.lane_mut().begin(cancel, None);
    }

    // Run a `!` command the way a turn runs: off the loop, so the screen stays
    // live and the lane can be left to it.
    //
    // It borrows the transcript like a turn, and for the same reason: the
    // result is filed in it, and nothing else may replace it meanwhile.
    // Hand the terminal to `$EDITOR` on a copy of the line, and take back what
    // was saved. On its own thread: a run in flight still has a stream to serve.
    pub(super) async fn edit_externally(&mut self) {
        let path = match scratch_file(self.ui.editor.text()) {
            Ok(path) => path,
            Err(e) => {
                self.ui.flash(format!("no scratch file: {e}"));
                return;
            }
        };
        let (program, args) = external_editor();

        // The terminal is gone from here to `resume`, so nothing between them
        // may return early: the surface would be left invisible.
        let _parked = self.hold.park().await;
        let _deaf = Deafened::new();
        self.ui.screen.leave();
        let ran = tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                let mut cmd = std::process::Command::new(program);
                cmd.args(args).arg(&path);
                // SIG_IGN is inherited across exec, and an editor installing
                // no handler would be the one that could not be interrupted.
                #[cfg(unix)]
                unsafe {
                    use std::os::unix::process::CommandExt;
                    cmd.pre_exec(|| {
                        libc::signal(libc::SIGINT, libc::SIG_DFL);
                        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                        Ok(())
                    });
                }
                cmd.status()
            }
        })
        .await;
        let resumed = self.ui.screen.resume();
        self.ui.show_mode();
        // A resize while the child held the terminal raised no event, so the
        // view's measurements are against a width that may no longer exist.
        front_view(&mut self.views, self.core.lane())
            .surface
            .counted = None;

        // Judged on its own: a save that succeeded is still a save when the
        // screen comes back badly, and reading the two together threw it away.
        let (mut keep, mut said) = match ran {
            Err(e) => (false, Some(format!("the editor did not run: {e}"))),
            Ok(Err(e)) => (false, Some(format!("could not run the editor: {e}"))),
            // `:cq` is how vim says "forget it". Git reads a non-zero exit the
            // same way, and the line the user had is worth more than the file.
            Ok(Ok(s)) if !s.success() => (
                false,
                Some(format!("editor exited {s} — the line is unchanged")),
            ),
            Ok(Ok(_)) => match std::fs::read_to_string(&path) {
                Ok(text) => {
                    self.ui.editor.set_line(text.trim_end());
                    (false, None)
                }
                // Kept: what was written is the only copy of it, and naming
                // the file beats deleting it.
                Err(e) => (
                    true,
                    Some(format!(
                        "saved at {} — could not read it back: {e}",
                        path.display()
                    )),
                ),
            },
        };
        // A surface that did not come back cannot show anything, so the file
        // stays as the way out and its name is what the message carries.
        if let Err(e) = resumed {
            keep = true;
            said = Some(format!(
                "the screen did not come back: {e} — line at {}",
                path.display()
            ));
        }
        if !keep {
            let _ = std::fs::remove_file(&path);
        }
        // `keep` is exactly "this message names the file": the two branches
        // that leave one behind are the two the user has to act on, and a row
        // in the transcript is what survives long enough to copy a path out
        // of. The rest are a press's own answer.
        if let Some(line) = said {
            if keep {
                self.say_of(self.core.current, line);
            } else {
                self.ui.flash(line);
            }
        }
    }

    // A job came home. Every kind settles on the lane that lent it the
    // transcript, whichever lane is on screen by the time it lands.
    pub(super) async fn settle(&mut self, done: Done) {
        // The run posts its last events and only then says it is over, so both
        // are in flight at once and the end can win the race. Take what is
        // waiting before closing anything, or a tool row still open is frozen
        // as abandoned and the elapsed figure is read off a cleared clock.
        self.serve_lanes().await;
        let Some(lane) = self.core.lanes.iter().position(|l| l.token == done.token) else {
            return;
        };
        let back = self.core.lanes[lane].finish();
        view_at(&mut self.views, self.core.lanes[lane].token())
            .queued
            .extend(
                back.unheard
                    .into_iter()
                    .map(|said| Queued::Line(Intent::Prompt(said))),
            );
        let unsend = back.unsend;

        match done.kind {
            Kind::Turn | Kind::Bash { .. } => self.settle_run(lane, done, unsend).await,
            Kind::Compact(_) => self.settle_compact(lane, done).await,
        }
    }

    // A turn or a `!` has ended. Put the transcript back and save it,
    // whichever lane it belongs to; show the end of it only when that lane
    // is the one on screen.
    //
    // The saving cannot wait — a lane the user never returns to still has to
    // have its work on disk — but nothing about drawing it does.
    async fn settle_run(&mut self, lane: usize, done: Done, unsend: bool) {
        let Done { ran, kind, .. } = done;
        // Only a turn is a request the model was working on, and only a turn's
        // ending is worth telling it about.
        let was_turn = matches!(kind, Kind::Turn);
        // A `!` brings its lines home to be shown here; a turn's reached the
        // view as events, and a compact never arrives at this function.
        let (was_bash, said) = match kind {
            Kind::Bash { lines } => (true, Some(lines)),
            _ => (false, None),
        };

        // Only a run that came back says why it ended; the archive rebuild
        // below has the same transcript and knows nothing about the run.
        let ran_back = ran.is_some();

        // The task carried the whole transcript, not just this turn, and a
        // panic in it dropped that copy. The archive is the last good one;
        // carrying the empty stand-in forward would save it over the real one
        // at the end of the next turn, which loses the conversation rather
        // than the turn.
        let (recovered, out) = match ran {
            Some((session, out)) => {
                self.core.lanes[lane].return_session(session);
                (true, out)
            }
            None => (
                self.recover_session(lane, "run"),
                Err(AgentError::Cancelled),
            ),
        };

        // The lane in front drew every row the run left behind: its entries as
        // they were committed, and whatever its ending filed as it was worded.
        // So its cursor is the end of what came home — and it has to be, or the
        // next turn's adopt draws those filed rows again, above the prompt it
        // is answering. A lane off screen keeps its cursor: that is what the
        // replay of its `pending` events goes by.
        if ran_back && lane == self.core.current {
            let tail = self.core.lanes[lane].session().and_then(tail_of);
            view_at(&mut self.views, self.core.lanes[lane].token())
                .surface
                .tail = tail;
        }

        // A panic never came back, and Esc that took the prompt back produced
        // nothing to misread as a task: neither has anything to tell. Nor does
        // a `!` the user stopped — the shell command was theirs, and calling
        // it a cancelled run tells the model to abandon a request it never had.
        if ran_back && !unsend && was_turn && self.core.lanes[lane].session().is_some() {
            self.core.lanes[lane].note_outcome(&out);
        }

        // Saved either way: an interrupted turn is exactly the one worth
        // keeping. Not when the transcript never came back, though — the empty
        // one standing in for it would land on top of what is on disk.
        if recovered && let Err(e) = self.core.save_lane(lane) {
            self.say_of(lane, format!("warning: the transcript was not saved: {e}"));
        }
        // The save put the session on disk, the one `/resume` is likeliest to
        // want back; make the completion list see it.
        self.refresh_sessions();

        let cancelled = matches!(&out, Err(AgentError::Cancelled));
        if said.is_none() && lane == self.core.current {
            self.bridge.finish_turn(cancelled).await;
        }
        // The run's totals (subagents' included) land on its lane; an
        // interrupted run lands as the spend the view showed.
        self.core.lanes[lane].charge_run(&out);

        // A `!` draws its own rows as it lands, so its cursor is wherever the
        // transcript that came home ends: the archive's end if it panicked.
        if was_bash && let Some(session) = self.core.lanes[lane].session() {
            view_at(&mut self.views, self.core.lanes[lane].token())
                .surface
                .tail = tail_of(session);
        }
        // A `!` command's output comes home whole rather than as events, so
        // this is the only place it can reach the view that asked for it.
        if let Some(said) = said.filter(|lines| !lines.is_empty()) {
            let rows = said.into_iter().map(Row::notice);
            view_at(&mut self.views, self.core.lanes[lane].token())
                .surface
                .scrollback
                .extend(rows);
        }

        // Before the split below, so a round that ended off-screen still arms
        // the next one — it waits with the lane, like any queued line.
        //
        // A run that came back cancelled is the user's own stop, not a failure
        // to report to the model. One that never came back has no outcome to
        // read, so it is named the failure it is whatever stood in for one:
        // `recover_session` has said what became of the transcript, and this
        // says only what became of the loop.
        let cut = if unsend {
            Some(Cut::Unsent)
        } else if !ran_back {
            Some(Cut::Failed)
        } else if cancelled {
            Some(Cut::Stopped)
        } else if out.is_err() {
            Some(Cut::Failed)
        } else {
            None
        };
        let said = self.step_loop(lane, cut);

        if lane == self.core.current {
            self.close_run(out);
            if unsend && let Some(id) = self.core.lane().last_ask() {
                self.rewind_turn(id);
            }
        } else {
            // Out of sight: what the run left to draw waits with it, and the
            // lane says so in the bar until someone looks.
            self.core.lanes[lane].end(out, unsend);
        }
        // Last, and outside the split: a rewind rebuilds the whole surface, and
        // a row landed before it would go with the old drawing.
        if let Some(said) = said {
            self.say_of(lane, said);
        }
    }

    // A `/compact` has finished: put the transcript back, and show what the
    // pass did — or say there was nothing to shrink.
    async fn settle_compact(&mut self, lane: usize, done: Done) {
        let Done { ran, kind, .. } = done;
        let Kind::Compact(report) = kind else {
            unreachable!("only a compact settles here")
        };
        let stopped = matches!(&ran, Some((_, Err(AgentError::Cancelled))));
        // The lane it was started on may not be the one on screen any more;
        // give it back its transcript either way.
        let back = match ran {
            Some((session, _)) => {
                self.core.lanes[lane].return_session(session);
                true
            }
            None => self.recover_session(lane, "compaction"),
        };

        if let Some((report, spent)) = report {
            self.core.lanes[lane].charge(&spent);
            let _ = self.core.lanes[lane]
                .sender()
                .send(Event::Compacted(report));
            if back && let Err(e) = self.core.save_lane(lane) {
                self.say_of(lane, format!("warning: the transcript was not saved: {e}"));
            }
            self.refresh_sessions();
        } else if stopped {
            // Nothing was written, so there is nothing to report and nothing
            // to save — only the same word a stopped turn ends on.
            self.say_of(
                lane,
                self.ui.paint.span(&self.ui.paint.theme.muted, "stopped"),
            );
        } else if back {
            let held = self.core.lanes[lane].agent().kept_tokens();
            let now = self.core.tokens_now_at(lane);
            // `/compact` answered, and there was nothing to do: the answer to
            // a command, not news about the lane.
            self.ui.open_reply(Listing::say([format!(
                "nothing to compact — {now} tokens, all inside the {held} kept as working context"
            )]));
        }
        // The pass is over, so the clock stops. What it was driving — the
        // live region — already went with the turn.
        view_at(&mut self.views, self.core.lanes[lane].token())
            .state
            .started = None;
    }

    // Put back the archive when the job that borrowed the live transcript
    // never returned it, naming the job as `verb`. Says whether the lane now
    // holds something safe to write over its save — without one it refuses
    // work until `/new`, where exiting would cost every other lane its run.
    fn recover_session(&mut self, lane: usize, verb: &str) -> bool {
        let id = self.core.lanes[lane].id().to_string();
        match self.core.store.load(&id) {
            Ok(stored) => {
                self.core.lanes[lane].return_session(stored.into_session());
                self.say_of(
                    lane,
                    format!("the {verb} did not finish — back to the transcript as last saved"),
                );
                true
            }
            Err(why) => {
                self.say_of(
                    lane,
                    format!(
                        "the {verb} did not finish and its transcript could not be read back \
                         ({why}) — /new or /resume to use this checkout again"
                    ),
                );
                false
            }
        }
    }

    // Draw the end of a run into the view that is on screen.
    pub(super) fn close_run(&mut self, out: Result<llm::stream::Usage, AgentError>) {
        let view = front_view(&mut self.views, self.core.lane());
        self.ui.close(view);
        // A cancelled run's calls got no `ToolEnd`; their animated rows have to
        // reach scrollback some other way before the next flush draws them as a
        // frozen spinner.
        self.ui.abandon_tools(view);
        view.state.started = None;
        match out {
            Ok(_) => {}
            Err(AgentError::Cancelled) => {
                let stopped = Line::from(self.ui.paint.span(&self.ui.paint.theme.muted, "stopped"));
                self.ui.say_line(view, stopped);
            }
            Err(e) => {
                let mark = self.ui.paint.span(&self.ui.paint.theme.status.err, "error");
                let rest = Span::from(format!(" {e}"));
                self.ui.say_line(view, Line::from(vec![mark, rest]));
            }
        }
    }

    // Stop every lane still working and settle it, so leaving cannot drop a
    // transcript that lives in a task.
    //
    // Cancelling, not waiting for the work to finish: a run stops at its next
    // cancellation point, which is the wait Esc already asks of anyone. The
    // deadline is for the run that will not stop — what is on disk is then the
    // last save, which is what leaving without this gave every time.
    pub(super) async fn settle_all(&mut self, done: &mut UnboundedReceiver<Done>) {
        let mut left = 0;
        for lane in &self.core.lanes {
            if lane.is_running() {
                lane.cancel();
                left += 1;
            }
        }
        if left == 0 {
            return;
        }
        self.ui.say(
            front_view(&mut self.views, self.core.lane()),
            "stopping — saving what the runs have written",
        );
        self.ui.flush(
            self.core.lane(),
            front_view(&mut self.views, self.core.lane()),
        );
        let waited = tokio::time::timeout(EXIT_GRACE, async {
            while left > 0 {
                match done.recv().await {
                    Some(ended) => {
                        self.settle(ended).await;
                        left -= 1;
                    }
                    None => break,
                }
            }
        })
        .await;
        if waited.is_err() {
            // On the terminal we are about to give back: a transcript that did
            // not come home is one the user should know is short.
            self.ui.screen.leave();
            eprintln!("{left} run(s) did not stop in time; their last save is what is on disk");
        }
    }
}
