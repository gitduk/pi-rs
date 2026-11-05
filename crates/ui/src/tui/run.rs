//! One run's life, armed to closed: starting a turn, a `!` line, or a
//! `/compact` off the loop, and putting the transcript back when it returns.
//! Beside them, what is asked off the loop without lending the transcript:
//! a tool named directly, and an MCP prompt's text.

use agent::session::{Entry as LogEntry, Prompt, Session};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::term::{Deafened, EXIT_GRACE, external_editor, scratch_file};
use super::view::{Doing, Work, front_view, tail_of, view_at};
use super::{NO_TRANSCRIPT, Tui};
use pi_core::driver::{Ended, Origin};
use pi_core::execution::{self, Change, Output, Request, RunError, RunResult};
pub(super) use pi_core::execution::{Done, Fetched, Kind, RemovedWorktrees, Started};
use toolbox::channel::Outcome;

// Where off-loop work reports back: one channel per kind of result.
pub(super) struct Replies {
    pub done: UnboundedSender<Done>,
    pub fetched: UnboundedSender<Fetched>,
    pub started: UnboundedSender<Started>,
    pub removed: UnboundedSender<RemovedWorktrees>,
}

// The receiving halves of [`Replies`]; the loop reads them.
pub(super) struct RepliesIn {
    pub done: UnboundedReceiver<Done>,
    pub fetched: UnboundedReceiver<Fetched>,
    pub started: UnboundedReceiver<Started>,
    pub removed: UnboundedReceiver<RemovedWorktrees>,
}

impl Replies {
    pub(super) fn channels() -> (Self, RepliesIn) {
        let (done, done_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fetched, fetched_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (removed, removed_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                done,
                fetched,
                started,
                removed,
            },
            RepliesIn {
                done: done_rx,
                fetched: fetched_rx,
                started: started_rx,
                removed: removed_rx,
            },
        )
    }
}

impl Tui {
    // The front lane's transcript, for `request`'s run about to start on it.
    // Missing only when a panic took the transcript and the archive won't
    // read back; then `request` fails here.
    fn take_carried(&mut self, request: Request) -> Option<(Session, Request)> {
        let Some(carried) = self.core.lane_mut().take_session() else {
            self.ui.flash(NO_TRANSCRIPT);
            self.complete(
                request,
                Output::Notice(String::new()),
                Outcome::Failed(NO_TRANSCRIPT.into()),
            );
            return None;
        };
        Some((carried, request))
    }

    // Hands the view to a run about to start: clock running, run figures
    // at zero.
    pub(super) fn arm_view(&mut self, doing: Doing) {
        let view = front_view(&mut self.views, self.core.lane());
        view.state.work = Some(Work::new(doing));
        self.core.lane_mut().seed_meter();
    }

    pub(super) fn deliver_prompt(
        &mut self,
        ask: Prompt,
        origin: Origin,
        line: Option<String>,
        done: &UnboundedSender<Done>,
    ) -> bool {
        let request = Request::new(origin, self.core.lane().token());
        self.deliver_prompt_for(ask, request, line, done)
    }

    pub(super) fn deliver_prompt_for(
        &mut self,
        ask: Prompt,
        request: Request,
        line: Option<String>,
        done: &UnboundedSender<Done>,
    ) -> bool {
        if self.core.lane().is_running() {
            self.enqueue(super::view::Queued {
                input: super::view::Pending::Prompt(ask),
                request,
                line,
            });
            return false;
        }
        if let Some(line) = line {
            self.echo(&line);
        }
        self.start_turn_for(ask, request, done)
    }

    #[cfg(test)]
    pub(super) fn start_turn(&mut self, ask: Prompt, done: &UnboundedSender<Done>) {
        let request = Request::new(Origin::Typed, self.core.lane().token());
        self.start_turn_for(ask, request, done);
    }

    fn start_turn_for(
        &mut self,
        ask: Prompt,
        request: Request,
        done: &UnboundedSender<Done>,
    ) -> bool {
        if let Some(said) = self.core.refresh_model_config() {
            let view = front_view(&mut self.views, self.core.lane());
            self.ui.adopt_config(&self.core, view);
            self.notice(pi_core::input::notice(said));
        }
        if let Some(said) = self.core.refresh_prompt_files() {
            self.notice(pi_core::input::notice(said));
        }
        if let Some(note) = self.core.lane().drift() {
            let lane = self.core.lane_mut();
            let view = front_view(&mut self.views, lane);
            self.ui.say_note(lane, view, &note);
        }
        // Lent to the run for the turn's length.
        let Some((mut carried, request)) = self.take_carried(request) else {
            return false;
        };
        let id = carried.send(ask);
        // The stop note the send filed is an entry now:
        // derive rows like any commit so they show without a rebuild.
        let view = front_view(&mut self.views, self.core.lane());
        view.surface.scrollback.bind_prompt(id);
        let tail = view.surface.tail;
        let fresh: Vec<LogEntry> = carried
            .entries()
            .iter()
            .filter(|e| tail.is_none_or(|t| e.id() > t) && !matches!(e, LogEntry::Ask { .. }))
            .cloned()
            .collect();
        self.ui.adopt(view, &fresh);
        let cancel = CancellationToken::new();
        let ctx = self.core.lane().ctx_for(cancel.clone());

        self.arm_view(Doing::Turn { committed: false });
        // Read while the agent is still reachable: `/model` may replace it
        // while this run works, and the run keeps the one it started on.
        front_view(&mut self.views, self.core.lane()).model_name =
            self.core.lane_mut().model_name().to_string();

        let agent = self.core.lane_mut().agent().clone();
        let sent = self.core.lane_mut().sender().clone();
        let done = done.clone();
        // Read where the run starts, not carried on the agent: a reload
        // between two turns reaches the next one this way.
        let retry = pi_core::core::retry(&self.core.config);
        let queued = front_view(&mut self.views, self.core.lane()).queued.clone();
        let input = pi_core::driver::input::Input::new(
            self.core.lane().root().to_path_buf(),
            self.drivers.background(),
            ctx.clone(),
            move || queued.take_prompt(),
            self.core.lane().notes(),
        );
        self.drivers.started(request, true);
        let turn = execution::Turn {
            request,
            session: carried,
            agent,
            ctx,
            events: sent,
            retry,
            input,
        };
        tokio::spawn(async move {
            let _ = done.send(turn.run().await);
        });
        self.core.lane_mut().begin(cancel);
        true
    }

    // Runs a `!` command off the loop, so the screen stays live. Borrows
    // the transcript like a turn — the result is filed in it meanwhile.
    pub(super) fn start_bash(
        &mut self,
        command: String,
        request: Request,
        done: &UnboundedSender<Done>,
    ) {
        let Some((carried, request)) = self.take_carried(request) else {
            return;
        };
        let cancel = CancellationToken::new();
        let ctx = self.core.lane_mut().ctx_for(cancel.clone());
        // A `!` forecloses `Deed::Unsend`, the only writer of
        // `Run::Running.unsend`, which it has no prompt to honour.
        self.arm_view(Doing::Bash);

        let done = done.clone();
        let shell = toolbox::bash::Bash::new(std::sync::Arc::new(
            pi_core::driver::background::Commands::new(self.drivers.background()),
        ));
        tokio::spawn(async move {
            let _ = done.send(execution::bash(request, carried, ctx, command, shell).await);
        });
        self.core.lane_mut().begin(cancel);
    }

    // Runs the tool the user named off the loop, beside whatever turn is going.
    pub(super) fn start_tool(
        &mut self,
        name: String,
        args: serde_json::Value,
        request: Request,
        started: &UnboundedSender<Started>,
    ) {
        let Some(tool) = self.core.lane().agent().brief.registry.get(&name) else {
            self.complete(
                request,
                Output::Notice(String::new()),
                Outcome::Failed(format!("{name} cannot start here")),
            );
            return;
        };
        self.ui.flash(format!("starting {name}…"));
        let ctx = self.core.lane().ctx_for(CancellationToken::new());
        let started = started.clone();
        tokio::spawn(async move {
            let _ = started.send(execution::tool(request, tool, args, ctx).await);
        });
    }

    pub(super) fn land_started(&mut self, started: Started) {
        self.present(started);
    }

    // Removes checkouts off the loop: a big one takes seconds on the disk,
    // and the screen keeps drawing meanwhile.
    pub(super) fn start_removal(
        &mut self,
        removal: pi_core::input::Removal,
        request: Request,
        removed: &UnboundedSender<RemovedWorktrees>,
    ) {
        let names: Vec<&str> = removal
            .targets
            .iter()
            .map(|target| target.name.as_str())
            .collect();
        if !names.is_empty() {
            self.ui.flash(format!("removing {}…", names.join(", ")));
        }
        let store = self.core.store.clone();
        let executions = self.core.executions.clone();
        let removed = removed.clone();
        tokio::spawn(async move {
            let done = execution::remove_worktrees(request, removal, store, executions).await;
            let _ = removed.send(done);
        });
    }

    // A removal came home: close the lanes that worked in its checkouts, say
    // what went, and drop the cached list that still names them.
    pub(super) fn land_removal(&mut self, done: RemovedWorktrees) {
        let RemovedWorktrees {
            request,
            removal,
            results,
        } = done;
        let mut said = removal.refused.clone();
        for result in results {
            match result {
                Ok(gone) => {
                    if removal.all {
                        said.push(format!("{} removed", gone.name));
                    }
                    let line = self.core.settle_removal(gone);
                    if !line.is_empty() {
                        said.push(line);
                    }
                }
                Err(why) => said.push(why),
            }
        }
        for target in &removal.targets {
            self.core.release_removal(&target.name);
        }
        if removal.switched {
            self.adopt_change(Change::Session);
        }
        self.adopt_change(Change::Worktrees);
        // Said on the lane in front: the lane the request came from may be
        // one of the checkouts that just went.
        self.complete(
            Request {
                lane: self.core.lane().token(),
                ..request
            },
            Output::Notice(pi_core::input::notice(said)),
            Outcome::Done,
        );
    }

    // Asks an MCP server for a prompt's text off the loop: a slow server
    // must not hold the screen.
    pub(super) fn start_fetch(
        &mut self,
        prompt: mcp::Prompt,
        args: String,
        typed: String,
        request: Request,
        fetched: &UnboundedSender<Fetched>,
    ) {
        self.ui.flash(format!("asking {}…", prompt.word()));
        let fetched = fetched.clone();
        tokio::spawn(async move {
            let _ = fetched.send(execution::fetch(request, prompt, args, typed).await);
        });
    }

    // A fetched prompt starts its turn only on the lane it was asked on,
    // still in front and idle; otherwise it is dropped, and said so.
    pub(super) fn land_fetched(&mut self, fetched: Fetched, done: &UnboundedSender<Done>) {
        let Fetched {
            request,
            typed,
            text,
        } = fetched;
        let token = request.lane;
        let front = self.core.lane().token() == token;
        let started = match text {
            Err(why) => {
                self.complete(request, Output::Notice(String::new()), Outcome::Failed(why));
                false
            }
            Ok(send) if front && !self.core.lane().is_running() => {
                self.echo(&typed);
                self.start_turn_for(
                    super::clipboard::prompt(send, Some(typed), &self.ui.images),
                    request,
                    done,
                )
            }
            Ok(_) if !front => {
                self.complete(
                    request,
                    Output::Notice(String::new()),
                    Outcome::Stopped(format!(
                        "{typed} came back after its lane left the front; run it again"
                    )),
                );
                false
            }
            Ok(_) => {
                self.complete(
                    request,
                    Output::Notice(String::new()),
                    Outcome::Stopped(format!(
                        "{typed} came back while another turn ran; run it again"
                    )),
                );
                false
            }
        };
        if !started
            && request.origin == Origin::Loop
            && let Some(at) = self.core.position_of(token)
            && let Some(said) = self.drivers.unstarted(request)
        {
            self.say_notice_of(at, &said);
        }
    }

    // Runs `/compact` off the loop — summarizing what it drops is a model
    // call — while the lane keeps drawing and serving the others.
    pub(super) fn start_compact(
        &mut self,
        focus: Option<String>,
        request: Request,
        done: &UnboundedSender<Done>,
    ) {
        let Some((carried, request)) = self.take_carried(request) else {
            return;
        };
        let cancel = CancellationToken::new();
        self.arm_view(Doing::Compact);
        let agent = self.core.lane().agent().clone();
        let session_id = Some(self.core.lane().id().to_string());
        let done = done.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            let _ = done
                .send(execution::compact(request, carried, agent, focus, session_id, stop).await);
        });
        self.core.lane_mut().begin(cancel);
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
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
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
        let ran = match ran {
            Err(e) => Err(format!("the editor did not run: {e}")),
            Ok(Err(e)) => Err(format!("could not run the editor: {e}")),
            Ok(Ok(status)) => Ok(status),
        };
        (ran, resumed)
    }

    // A config or prompt file in `$EDITOR`, then the reload that makes it
    // count. A non-zero exit (`:cq`) is "forget it": nothing is reloaded.
    pub(super) async fn edit_file(&mut self, path: std::path::PathBuf) -> Output {
        let (ran, resumed) = self.in_editor(&path).await;
        let unchanged = pi_core::core::resolve::unseed(&path);
        if let Err(e) = resumed {
            self.say_notice_of(
                self.core.current,
                &format!("the screen did not come back: {e}"),
            );
        }
        let said = match ran {
            Err(why) => vec![why],
            Ok(s) if !s.success() => vec![format!("editor exited {s} — nothing reloaded")],
            Ok(_) if unchanged => {
                let name = pi_core::core::instructions::short(&path, self.core.lane().root());
                vec![format!("{name} unchanged — the built-in stays")]
            }
            Ok(_) => match self.core.config_edited() {
                // The file edited first: what else the reload says follows it.
                Ok(notes) => {
                    let root = self.core.lane().root();
                    let name = pi_core::core::instructions::short(&path, root);
                    let prompt = pi_core::core::resolve::editable(&self.core.pinned, root)
                        .iter()
                        .any(|f| f.path == path);
                    let when = if prompt {
                        "the next session or compaction"
                    } else {
                        "the next turn"
                    };
                    let line = format!("{name} — in force from {when}");
                    std::iter::once(line).chain(notes).collect()
                }
                Err(why) => vec![why],
            },
        };
        Output::Notice(pi_core::input::notice(said))
    }

    // Hand the terminal to `$EDITOR` on a copy of the line, and take back what
    // was saved.
    pub(super) async fn edit_externally(&mut self) {
        let path = match scratch_file(self.ui.editor.text()) {
            Ok(path) => path,
            Err(e) => {
                self.notice(format!("no scratch file: {e}"));
                return;
            }
        };
        let (ran, resumed) = self.in_editor(&path).await;

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
        // A file left behind is the only copy of the line: its name stays
        // in the scrollback, where a flash's going cannot take it.
        if let Some(line) = said {
            if keep {
                self.say_notice_of(self.core.current, &line);
            } else {
                self.notice(line);
            }
        }
    }

    // A run came home. Every kind settles on the lane that lent it the
    // transcript, whichever lane is on screen by the time it lands.
    pub(super) async fn settle(&mut self, done: Done) {
        // Events arrive before the end that follows them, so both race;
        // take what's waiting first, or a tool row freezes mid-abandon.
        self.serve_lanes().await;
        let completion = done.completion();
        let Some(lane) = self.core.position_of(done.request.lane) else {
            if let Some(completion) = completion {
                self.present(completion);
            }
            return;
        };
        let unsend = self.core.lanes[lane].finish();

        match done.kind {
            Kind::Turn | Kind::Bash { .. } | Kind::BashBackground { .. } => {
                self.settle_run(lane, done, unsend).await
            }
            Kind::Compact(_) => self.settle_compact(lane, done).await,
        }
        if let Some(completion) = completion {
            self.present(completion);
        }
    }

    // A turn or `!` ended: puts the transcript back and saves it; shows
    // the end only if that lane is on screen. Saving can't wait for that.
    async fn settle_run(&mut self, lane: usize, done: Done, unsend: bool) {
        let how = if unsend { Ended::Unsent } else { done.ended() };
        let Done { ran, kind, .. } = done;
        // Only a turn is a request the model was working on, and only a turn's
        // ending is worth telling it about.
        let was_turn = matches!(kind, Kind::Turn);
        let was_bash = matches!(kind, Kind::Bash { .. } | Kind::BashBackground { .. });

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
            None => (self.recover_session(lane, "run"), Err(RunError::Crashed)),
        };

        // The view drew every row the run left, in front or not, so its cursor
        // is the end of what came home — or the next adopt draws them again.
        if ran_back {
            let session = self.core.lanes[lane].session();
            let surface = &mut view_at(&mut self.views, self.core.lanes[lane].token()).surface;
            if was_bash
                && let Some(entry) = session.and_then(|s| s.entries().last())
                && matches!(entry, LogEntry::Bash { .. })
                && surface.tail.is_none_or(|tail| entry.id() > tail)
            {
                surface.scrollback.bind_prompt(entry.id());
            }
            surface.tail = session.and_then(tail_of);
        }

        // A panic, or an unsent prompt, has nothing to tell the model; nor
        // does a user-stopped `!`, which was never the model's request.
        if ran_back && !unsend && was_turn && self.core.lanes[lane].session().is_some() {
            self.core.lanes[lane].note_ended(&how);
        }

        // Saved either way — an interrupted turn is worth keeping — unless
        // the transcript never came back, which would overwrite the disk copy.
        // A failure shows on the bar until a save lands; the error is in `/status`.
        if recovered {
            let _ = self.core.save_lane(lane);
        }

        // A `!` draws its own rows as it lands, so its cursor is wherever the
        // transcript that came home ends: the archive's end if it panicked.
        if was_bash && let Some(session) = self.core.lanes[lane].session() {
            view_at(&mut self.views, self.core.lanes[lane].token())
                .surface
                .tail = tail_of(session);
        }
        // Before the split, so an off-screen round still comes due. A run
        // that never came back is named the failure it stood in for.
        let token = self.core.lanes[lane].token();
        match &how {
            Ended::Done => tracing::info!(target: "pi::loop", lane = token, "run ended"),
            Ended::Stopped => tracing::info!(target: "pi::loop", lane = token, "run stopped"),
            Ended::Unsent => tracing::info!(target: "pi::loop", lane = token, "run unsent"),
            Ended::Failed(why) => {
                tracing::warn!(target: "pi::loop", lane = token, error = %why, "run failed")
            }
        }
        let cap = self.core.config.loop_cap();
        let ended = self
            .drivers
            .turn_ended(token, &how, self.core.lanes[lane].ctx(), cap);

        let ok = out.is_ok();
        // Non-model tasks have their ending in the operation output. The
        // streamed model run still closes its own live rows and tally here.
        self.close_run(
            lane,
            if was_turn {
                out
            } else {
                Ok(Default::default())
            },
            was_turn,
        );
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
            self.say_notice_of(lane, &ended);
        }
    }

    async fn settle_compact(&mut self, lane: usize, done: Done) {
        let Done { ran, kind, .. } = done;
        let Kind::Compact(report) = kind else {
            unreachable!("only a compact settles here")
        };
        // The lane it was started on may not be the one on screen any more;
        // give it back its transcript either way.
        let back = match ran {
            Some((session, _)) => {
                self.core.lanes[lane].return_session(session);
                true
            }
            None => self.recover_session(lane, "compaction"),
        };

        if report.is_some() && back {
            let _ = self.core.save_lane(lane);
        }
        // The pass is over, so the clock stops. What it was driving — the
        // live region — already went with the turn.
        view_at(&mut self.views, self.core.lanes[lane].token())
            .state
            .work = None;
    }

    // Puts back the archive when a run never returned the borrowed
    // transcript, naming it `verb`. Says if it's safe to save over.
    fn recover_session(&mut self, lane: usize, verb: &str) -> bool {
        let on = &self.core.lanes[lane];
        // This lane's own copy, the one it saves to.
        let path = self.core.store.path_of(on.root(), on.id());
        match pi_store::session::Store::read(&path) {
            Ok(stored) => {
                self.core.lanes[lane].return_session(stored.into_session());
                self.say_notice_of(
                    lane,
                    &format!("the {verb} did not finish — back to the transcript as last saved"),
                );
                true
            }
            Err(why) => {
                self.say_notice_of(
                    lane,
                    &format!(
                        "the {verb} did not finish and its transcript could not be read back \
                         ({why}) — /new or /resume to use this checkout again"
                    ),
                );
                false
            }
        }
    }

    // Draw the end of a run into its lane's view, on screen or not.
    pub(super) fn close_run(&mut self, lane: usize, out: RunResult, was_turn: bool) {
        let on = &mut self.core.lanes[lane];
        let view = view_at(&mut self.views, on.token());
        self.ui.close(view);
        // A cancelled run's calls got no `ToolEnd`; their animated rows
        // need to reach scrollback before the next flush freezes them.
        self.ui.abandon_tools(view);
        // Only `Done` keeps a turn's status line; a turn ending otherwise
        // keeps it here, read before the clock stops.
        let parts = match was_turn {
            true => crate::status::parts(&self.ui.status, &super::view::snapshot(on, view), false),
            false => Vec::new(),
        };
        view.state.work = None;
        match out {
            Ok(_) => {}
            Err(error) if error.is_cancelled() && parts.is_empty() => {
                let stopped = Line::from(self.ui.paint.span(&self.ui.paint.theme.muted, "stopped"));
                self.ui.say(view, stopped);
            }
            Err(error) if error.is_cancelled() => {
                let mut parts = parts;
                parts.insert(0, "stopped".to_string());
                let line = crate::status::settled(&parts);
                self.ui.say_tally(on, view, &line);
            }
            Err(e) => {
                let mark = self.ui.paint.span(&self.ui.paint.theme.status.err, "error");
                let rest = Span::from(format!(" {e}"));
                self.ui.say(view, Line::from(vec![mark, rest]));
                if !parts.is_empty() {
                    self.ui.say_tally(on, view, &crate::status::settled(&parts));
                }
            }
        }
    }

    // Shutdown must preserve transcripts and the cause of stopped executions.
    pub(super) async fn settle_all(&mut self, done: &mut UnboundedReceiver<Done>, why: &str) {
        for view in self.views.values() {
            while let Some(queued) = view.queued.pop_front() {
                self.cancel_queued(queued.request, why);
            }
        }
        // A note filed while idle would otherwise wait for a turn that never comes.
        self.core.save_filed();
        let mut left = 0;
        for lane in &self.core.lanes {
            if lane.is_running() {
                lane.cancel();
                left += 1;
            }
        }
        if left > 0 {
            self.notice("stopping — saving what the runs have written".to_string());
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
                // A transcript still held by a run cannot be saved here.
                self.ui.screen.leave();
                eprintln!("{left} run(s) did not stop in time; their last save is what is on disk");
            }
        }
        // A settling turn may start another command before shutdown completes.
        let executions = self.drivers.background();
        for lane in &mut self.core.lanes {
            let stopped = executions.stop_running(lane.root(), why);
            if !stopped.is_empty() {
                lane.push_note(&format!(
                    "{why} pi stopped running background commands {} during shutdown. \
                     They did not finish normally; their results will not be reported.",
                    stopped.join(", ")
                ));
            }
        }
        self.core.save_filed();
    }
}
