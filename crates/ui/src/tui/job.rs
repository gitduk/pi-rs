//! One run's life, armed to closed: starting a turn, a `!` line, or a
//! `/compact` off the loop, and putting the transcript back when it returns.

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
use pi_core::core;
use pi_core::driver::Ended;
use pi_core::input::Intent;
use pi_store::listing::Listing;

// What kind of job a finished `Done` was, carrying what only that kind
// leaves behind — so no arm can be built holding another's.
pub(super) enum Kind {
    // A turn: the agent loop ran, and `ran` says how it went. Its output
    // reached the view through the lane's event channel as it happened.
    Turn,
    // A `!` command and the lines it printed, home whole rather than as
    // events — kept apart from Turn since a `!` answers no channel.
    Bash {
        // The output lines, derived by the same function the rebuild draws
        // with — plus the flash for a command that never ran.
        lines: Vec<String>,
    },
    // A `/compact`, and what it shrank and spent. None means the transcript
    // already fit — or, with a cancelled `ran`, that nobody ever looked.
    Compact(Option<(agent::Report, llm::stream::Usage)>),
}

// A job that ran off the loop, reporting back via one channel (not a
// `JoinHandle`) so nothing polls a growing list of lanes.
pub(super) struct Done {
    pub(super) token: u64,
    pub(super) kind: Kind,
    // The transcript back, and how the job went. None only if it
    // panicked — the two are never separately absent.
    pub(super) ran: Option<(Session, Result<llm::stream::Usage, AgentError>)>,
}

// Runs `job` off the loop, turning a panic into `None` — without this
// a job that never reports leaves its lane looking "working" forever.
pub(super) async fn guard<F, T>(job: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    std::panic::AssertUnwindSafe(job).catch_unwind().await.ok()
}

impl Tui {
    // The front lane's transcript, for a job about to run on it.
    fn take_carried(&mut self) -> Option<Session> {
        let carried = self.core.lane_mut().take_session();
        if carried.is_none() {
            self.ui.flash(NO_TRANSCRIPT);
        }
        carried
    }

    // Hands the view to a job about to start: clock running, run figures
    // at zero. `committed` says whether the prompt can still be taken back.
    pub(super) fn arm_view(&mut self, committed: bool) {
        let view = front_view(&mut self.views, self.core.lane());
        view.state.started = Some(std::time::Instant::now());
        view.state.committed = committed;
        view.state.stopping = false;
        view.state.retry = None;
        self.core.lane_mut().seed_meter();
    }

    // Starts a turn on the lane in front and returns immediately: the run
    // posts to that lane's channel, freeing the loop to draw and serve others.
    pub(super) fn start_turn(
        &mut self,
        prompt: String,
        typed: Option<String>,
        done: &UnboundedSender<Done>,
    ) {
        // Lent to the run for the turn's length. Missing only when a panic
        // took the transcript and the archive won't read back.
        let Some(mut carried) = self.take_carried() else {
            return;
        };
        // The line keeps `[Image #n …]`; the model is told where each one is.
        let (prompt, typed, images) = match super::clipboard::with_paths(&prompt, &self.ui.images) {
            Some((sent, named)) => (
                sent,
                typed.or(Some(prompt)),
                super::clipboard::attached(&named),
            ),
            None => (prompt, typed, Vec::new()),
        };
        carried.send_prompt_with(prompt, typed, images);
        // The repair results and stop note the send filed are entries now:
        // derive rows like any commit so they show without a rebuild.
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
        // Read where the run starts, not carried on the agent: a reload
        // between two turns reaches the next one this way.
        let retry = self.core.config.retry();
        // The run's own handle on the mailbox; the lane keeps the other.
        let heard = steer.clone();
        tokio::spawn(async move {
            let out = guard(agent.steered(&mut carried, &ctx, &sent, &heard, &retry)).await;
            let _ = done.send(Done {
                token,
                kind: Kind::Turn,
                ran: out.map(|out| (carried, out)),
            });
        });
        self.core.lane_mut().begin(cancel, Some(steer));
    }

    // Runs a `!` command off the loop, so the screen stays live. Borrows
    // the transcript like a turn — the result is filed in it meanwhile.
    pub(super) fn start_bash(&mut self, command: String, done: &UnboundedSender<Done>) {
        let Some(mut carried) = self.take_carried() else {
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
                let out = core::bash::run_bash(&ctx, &command).await;
                // Esc that stopped the `!` is a cancelled run too; `ran` says
                // so instead of a success that spent nothing.
                let ran = if ctx.cancel.is_cancelled() {
                    Err(AgentError::Cancelled)
                } else {
                    Ok(llm::stream::Usage::default())
                };
                core::bash::record_bash(&mut carried, &command, out.text.clone());
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

    // Runs `/compact` off the loop — summarizing what it drops is a model
    // call — while the lane keeps drawing and serving the others.
    pub(super) fn start_compact(&mut self, focus: Option<String>, done: &UnboundedSender<Done>) {
        let Some(mut carried) = self.take_carried() else {
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

    // Hands the terminal to `$EDITOR` on `path`, on its own thread (a
    // stream may still need serving), and takes it back.
    async fn in_editor(
        &mut self,
        path: &std::path::Path,
    ) -> (
        Result<std::process::ExitStatus, String>,
        std::io::Result<()>,
    ) {
        let (program, args) = external_editor();
        let _parked = self.hold.park().await;
        let _deaf = Deafened::new();
        self.ui.screen.leave();
        let ran = tokio::task::spawn_blocking({
            let path = path.to_path_buf();
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
        let ran = match ran {
            Err(e) => Err(format!("the editor did not run: {e}")),
            Ok(Err(e)) => Err(format!("could not run the editor: {e}")),
            Ok(Ok(status)) => Ok(status),
        };
        (ran, resumed)
    }

    // The project's config in `$EDITOR`, then the reload that makes it count.
    // A non-zero exit (`:cq`) is "forget it": nothing is reloaded.
    pub(super) async fn edit_config(&mut self, path: std::path::PathBuf) {
        let (ran, resumed) = self.in_editor(&path).await;
        if let Err(e) = resumed {
            self.say_of(
                self.core.current,
                format!("the screen did not come back: {e}"),
            );
        }
        let said = match ran {
            Err(why) => vec![why],
            Ok(s) if !s.success() => vec![format!("editor exited {s} — nothing reloaded")],
            Ok(_) => self.core.config_edited(),
        };
        self.land_lines(Listing::say(said));
    }

    // Hand the terminal to `$EDITOR` on a copy of the line, and take back what
    // was saved.
    pub(super) async fn edit_externally(&mut self) {
        let path = match scratch_file(self.ui.editor.text()) {
            Ok(path) => path,
            Err(e) => {
                self.say_of(self.core.current, format!("no scratch file: {e}"));
                return;
            }
        };
        let (ran, resumed) = self.in_editor(&path).await;

        // Judged on its own: a save that succeeded is still a save when the
        // screen comes back badly, and reading the two together threw it away.
        let quit = matches!(&ran, Ok(s) if !s.success());
        let (mut keep, mut said) = match ran {
            Err(why) => (false, Some(why)),
            // `:cq` is how vim says "forget it". Git reads a non-zero exit the
            // same way, and the line the user had is worth more than the file.
            Ok(s) if !s.success() => (
                false,
                Some(format!("editor exited {s} — the line is unchanged")),
            ),
            Ok(_) => match std::fs::read_to_string(&path) {
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
        // Only a plain `:cq` is a press's own answer; the rest carry an error
        // or name the file left behind, and belong where they can be re-read.
        if let Some(line) = said {
            if keep || !quit {
                self.say_of(self.core.current, line);
            } else {
                self.ui.flash(line);
            }
        }
    }

    // A job came home. Every kind settles on the lane that lent it the
    // transcript, whichever lane is on screen by the time it lands.
    pub(super) async fn settle(&mut self, done: Done) {
        // Events arrive before the end that follows them, so both race;
        // take what's waiting first, or a tool row freezes mid-abandon.
        self.serve_lanes().await;
        let Some(lane) = self.core.position_of(done.token) else {
            return;
        };
        let back = self.core.lanes[lane].finish();
        let token = self.core.lanes[lane].token();
        let origins = self.drivers.unheard(token, back.unheard.len());
        let unheard: Vec<_> = back
            .unheard
            .into_iter()
            .zip(origins)
            .map(|(said, origin)| Queued {
                intent: Intent::Prompt(said),
                origin,
            })
            .collect();
        view_at(&mut self.views, token).queued.extend(unheard);
        let unsend = back.unsend;

        match done.kind {
            Kind::Turn | Kind::Bash { .. } => self.settle_run(lane, done, unsend).await,
            Kind::Compact(_) => self.settle_compact(lane, done).await,
        }
    }

    // A turn or `!` ended: puts the transcript back and saves it; shows
    // the end only if that lane is on screen. Saving can't wait for that.
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

        // A panic drops the carried transcript; the archive is the last
        // good copy, so recovering it instead avoids overwriting it later.
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

        // The view drew every row the run left, in front or not, so its cursor
        // is the end of what came home — or the next adopt draws them again.
        if ran_back {
            let tail = self.core.lanes[lane].session().and_then(tail_of);
            view_at(&mut self.views, self.core.lanes[lane].token())
                .surface
                .tail = tail;
        }

        // A panic, or an unsent prompt, has nothing to tell the model; nor
        // does a user-stopped `!`, which was never the model's request.
        if ran_back && !unsend && was_turn && self.core.lanes[lane].session().is_some() {
            self.core.lanes[lane].note_outcome(&out);
        }

        // Saved either way — an interrupted turn is worth keeping — unless
        // the transcript never came back, which would overwrite the disk copy.
        if recovered && let Err(e) = self.core.save_lane(lane) {
            self.say_of(lane, core::not_saved(&e));
        }
        // The save put the session on disk, the one `/resume` is likeliest to
        // want back; make the completion list see it.
        self.refresh_sessions();

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

        // Before the split, so an off-screen round still comes due. A run
        // that never came back is named the failure it stood in for.
        let how = if unsend {
            Ended::Unsent
        } else if !ran_back {
            Ended::Failed("the run crashed".into())
        } else {
            match &out {
                Ok(_) => Ended::Done,
                Err(AgentError::Cancelled) => Ended::Stopped,
                Err(e) => Ended::Failed(e.to_string()),
            }
        };
        let token = self.core.lanes[lane].token();
        let cap = self.core.config.loop_cap();
        let ended = self
            .drivers
            .turn_ended(token, &how, self.core.lanes[lane].ctx(), cap);

        let ok = out.is_ok();
        self.close_run(lane, out);
        if lane == self.core.current {
            if unsend && let Some(id) = self.core.lane().last_ask() {
                self.rewind_turn(id);
            }
        } else {
            // Out of sight: the lane says so in the bar until someone looks,
            // and a prompt asked back is taken back then.
            self.core.lanes[lane].end(ok, unsend);
        }
        // Last, and outside the split: a rewind rebuilds the whole surface, and
        // a row landed before it would go with the old drawing.
        if let Some(ended) = ended {
            self.say_of(lane, ended);
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
                self.say_of(lane, core::not_saved(&e));
            }
            self.refresh_sessions();
        } else if stopped {
            // Nothing written, nothing to report or save — same word a
            // stopped turn ends on.
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

    // Puts back the archive when a job never returned the borrowed
    // transcript, naming it `verb`. Says if it's safe to save over.
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

    // Draw the end of a run into its lane's view, on screen or not.
    pub(super) fn close_run(&mut self, lane: usize, out: Result<llm::stream::Usage, AgentError>) {
        let view = view_at(&mut self.views, self.core.lanes[lane].token());
        self.ui.close(view);
        // A cancelled run's calls got no `ToolEnd`; their animated rows
        // need to reach scrollback before the next flush freezes them.
        self.ui.abandon_tools(view);
        view.state.started = None;
        view.state.retry = None;
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

    // Stops every working lane and settles it, so leaving can't drop a
    // transcript living in a task. Cancels rather than waits for it to finish.
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
