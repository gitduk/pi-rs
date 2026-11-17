//! Present executions owned by the core: starting a turn, a `!` line, or a
//! `/compact`, and drawing their events and completed results.
//! Beside them, what is asked off the loop without lending the transcript:
//! a tool named directly, and an MCP prompt's text.

use agent::session::{Entry as LogEntry, Prompt};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use super::Tui;
use super::term::{Deafened, EXIT_GRACE, external_editor, scratch_file};
use super::view::{Doing, Work, front_view, tail_of, view_at};
use ::execution::channel::Outcome;
use pi_core::driver::Origin;
#[cfg(test)]
pub(super) use pi_core::execution::Done;
use pi_core::execution::{self, Change, Output, Request, RunResult};
pub(super) use pi_core::execution::{Fetched, Kind, RemovedWorktrees, Started};

// Where off-loop work reports back: one channel per kind of result.
pub(super) struct Replies {
    pub fetched: UnboundedSender<Fetched>,
    pub started: UnboundedSender<Started>,
    pub removed: UnboundedSender<RemovedWorktrees>,
}

// The receiving halves of [`Replies`]; the loop reads them.
pub(super) struct RepliesIn {
    pub fetched: UnboundedReceiver<Fetched>,
    pub started: UnboundedReceiver<Started>,
    pub removed: UnboundedReceiver<RemovedWorktrees>,
}

impl Replies {
    pub(super) fn channels() -> (Self, RepliesIn) {
        let (fetched, fetched_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started, started_rx) = tokio::sync::mpsc::unbounded_channel();
        let (removed, removed_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                fetched,
                started,
                removed,
            },
            RepliesIn {
                fetched: fetched_rx,
                started: started_rx,
                removed: removed_rx,
            },
        )
    }
}

impl Tui {
    // Hands the view to a run about to start: clock running, run figures
    // at zero.
    pub(super) fn arm_view(&mut self, doing: Doing) {
        let view = front_view(&mut self.views, self.core.lane());
        view.state.work = Some(Work::new(doing));
        self.core.lane_mut().seed_meter();
    }

    #[cfg(test)]
    pub(super) fn deliver_prompt(
        &mut self,
        ask: Prompt,
        origin: Origin,
        line: Option<String>,
    ) -> bool {
        let request = Request::new(origin, self.core.lane().token());
        self.deliver_prompt_for(ask, request, line)
    }

    pub(super) fn deliver_prompt_for(
        &mut self,
        ask: Prompt,
        request: Request,
        line: Option<String>,
    ) -> bool {
        let admitted = match self.core.lane().messages().admit(request.origin, ask) {
            Ok(admitted) => admitted,
            Err(error) => {
                self.complete(
                    request,
                    Output::Notice(String::new()),
                    Outcome::Failed(error.to_string()),
                );
                return false;
            }
        };
        self.deliver_admitted(admitted, request, line)
    }

    pub(super) fn deliver_admitted(
        &mut self,
        admitted: pi_core::execution::messages::AdmittedInput,
        request: Request,
        line: Option<String>,
    ) -> bool {
        if admitted.message().session().as_str() != self.core.lane().id() {
            self.cancel_queued(request, "the input belongs to a different session");
            if request.origin == Origin::Typed {
                self.ui
                    .flash("queued input stopped because its session changed");
            }
            return false;
        }
        if self.core.lane().is_running() || !admitted.is_next() {
            self.enqueue(super::view::Queued {
                input: super::view::Pending::Prompt(admitted),
                request,
                line,
                note: String::new(),
            });
            return false;
        }
        if let Some(line) = line {
            self.echo(&line);
        } else if matches!(request.origin, Origin::Message(_)) {
            let prompt = admitted.prompt();
            match &prompt.shown {
                Some(label) => {
                    let view = front_view(&mut self.views, self.core.lane());
                    self.ui.submit_relayed(view, label, &prompt.text);
                }
                None => self.echo(&prompt.text),
            }
        }
        self.start_turn_for(admitted, request)
    }

    #[cfg(test)]
    pub(super) fn start_turn(&mut self, ask: Prompt) {
        let request = Request::new(Origin::Typed, self.core.lane().token());
        self.deliver_prompt_for(ask, request, None);
    }

    fn start_turn_for(
        &mut self,
        admitted: pi_core::execution::messages::AdmittedInput,
        request: Request,
    ) -> bool {
        self.drivers
            .respond(request, ::execution::channel::Response::Started);
        self.start_model_on(admitted, request)
    }

    fn start_model_on(
        &mut self,
        admitted: pi_core::execution::messages::AdmittedInput,
        request: Request,
    ) -> bool {
        let started = match self.core.start_model(admitted, request) {
            Ok(started) => started,
            Err(why) => {
                self.complete(request, Output::Notice(String::new()), Outcome::Failed(why));
                return false;
            }
        };
        self.present_started(started);
        true
    }

    fn present_started(&mut self, started: execution::host::Started) {
        let request = started.request;
        let Some(at) = self.core.position_of(request.lane) else {
            return;
        };
        if at == self.core.lanes.front() {
            if started.reconfigured {
                let view = front_view(&mut self.views, self.core.lane());
                self.ui.adopt_config(&self.core, view);
            }
            for notice in &started.notices {
                self.notice(notice.clone());
            }
        }
        self.drivers.link(request, started.message.id().clone());
        let lane = &self.core.lanes[at];
        let view = super::view::opened(&mut self.views, lane, &self.ui.paint);
        view.queued = lane.queue().clone();
        view.surface.scrollback.bind_prompt(started.input);
        let fresh: Vec<_> = started
            .entries
            .into_iter()
            .filter(|entry| !matches!(entry, LogEntry::Ask { .. }))
            .collect();
        self.ui.adopt(view, &fresh);
        view.state.work = Some(Work::new(Doing::Turn { committed: false }));
        self.drivers.started(request, true);
    }

    pub(super) fn draw_agent(&mut self, token: u64, event: agent::Event) {
        let Some(at) = self.core.position_of(token) else {
            return;
        };
        let lane = self.core.lanes.at_mut(at);
        self.drivers.observe_for(token, lane.ctx(), &event);
        let view = super::view::opened(&mut self.views, lane, &self.ui.paint);
        self.ui.render_event(lane, view, event);
    }

    pub(super) fn runtime_event(&mut self, event: execution::host::Event) {
        use execution::host::Event;
        match event {
            Event::Activity => {}
            Event::Started(started) => {
                if let Some(at) = self.core.position_of(started.request.lane) {
                    let lane = &self.core.lanes[at];
                    let view = super::view::opened(&mut self.views, lane, &self.ui.paint);
                    if let Some(LogEntry::Ask { ask, .. }) = started
                        .entries
                        .iter()
                        .find(|entry| entry.id() == started.input)
                    {
                        match (&started.line, &ask.shown) {
                            (Some(line), _) => self.ui.submit(view, line),
                            (None, Some(label)) => self.ui.submit_relayed(view, label, &ask.text),
                            (None, None) => self.ui.submit(view, &ask.text),
                        }
                    }
                }
                self.present_started(started);
            }
            Event::Agent { lane, event } => self.draw_agent(lane, event),
            Event::Settled(settled) => self.present_settlement(settled),
            Event::Deferred(deferred) => {
                if let Some(at) = self.core.position_of(deferred.lane) {
                    self.say_notice_of(
                        at,
                        &format!("background result is waiting: {}", deferred.message),
                    );
                }
            }
            Event::Rejected { request, message } => self.complete(
                request,
                Output::Notice(String::new()),
                Outcome::Failed(message),
            ),
            Event::Stopping { lane } => {
                if let Some(work) = &mut view_at(&mut self.views, lane).state.work {
                    work.stopping = true;
                }
            }
        }
    }

    // Runs a `!` command off the loop, so the screen stays live. Borrows
    // the transcript like a turn — the result is filed in it meanwhile.
    pub(super) fn start_bash(&mut self, command: String, request: Request) {
        match self.core.start_bash(request, command) {
            Ok(()) => self.arm_view(Doing::Bash),
            Err(why) => self.complete(request, Output::Notice(String::new()), Outcome::Failed(why)),
        }
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
    pub(super) fn land_fetched(&mut self, fetched: Fetched) {
        let Fetched {
            request,
            typed,
            text,
        } = fetched;
        let token = request.lane;
        let front = self.core.lane().token() == token;
        match text {
            Err(why) => {
                self.complete(request, Output::Notice(String::new()), Outcome::Failed(why));
            }
            Ok(send) if front && !self.core.lane().is_running() => {
                self.echo(&typed);
                let images = if request.origin == Origin::Typed {
                    &self.ui.images[..]
                } else {
                    &[]
                };
                self.deliver_prompt_for(
                    super::clipboard::prompt(send.into(), Some(typed), images),
                    request,
                    None,
                );
            }
            Ok(_) if !front => {
                self.complete(
                    request,
                    Output::Notice(String::new()),
                    Outcome::Stopped(format!(
                        "{typed} came back after its lane left the front; run it again"
                    )),
                );
            }
            Ok(_) => {
                self.complete(
                    request,
                    Output::Notice(String::new()),
                    Outcome::Stopped(format!(
                        "{typed} came back while another turn ran; run it again"
                    )),
                );
            }
        }
    }

    // Runs `/compact` off the loop — summarizing what it drops is a model
    // call — while the lane keeps drawing and serving the others.
    pub(super) fn start_compact(&mut self, focus: Option<String>, request: Request) {
        match self.core.start_compact(request, focus) {
            Ok(()) => self.arm_view(Doing::Compact),
            Err(why) => self.complete(request, Output::Notice(String::new()), Outcome::Failed(why)),
        }
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
        let mut editor = tokio::task::spawn_blocking({
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
        });
        let ran = loop {
            tokio::select! {
                result = &mut editor => break result,
                event = self.core.next_runtime_event() => self.runtime_event(event),
            }
        };
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
                self.core.lanes.front(),
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
                self.say_notice_of(self.core.lanes.front(), &line);
            } else {
                self.notice(line);
            }
        }
    }

    // A run came home. Every kind settles on the lane that lent it the
    // transcript, whichever lane is on screen by the time it lands.
    #[cfg(test)]
    pub(super) async fn settle(&mut self, done: Done) {
        // Events arrive before the end that follows them, so both race;
        // take what's waiting first, or a tool row freezes mid-abandon.
        self.serve_lanes();
        let settled = self.core.settle_execution(done);
        self.present_settlement(settled);
    }

    pub(super) fn present_settlement(&mut self, settled: execution::host::Settlement) {
        use execution::host::Settlement;
        let settled = match settled {
            Settlement::Ignored => return,
            Settlement::Removed(completion) => {
                if let Some(completion) = completion {
                    self.present(completion);
                }
                return;
            }
            Settlement::Returned(settled) => settled,
        };
        let completion = settled.completion();
        self.show_recovery(&settled);
        match settled.kind {
            Kind::Turn | Kind::Bash { .. } | Kind::BashBackground { .. } => {
                self.settle_run(settled)
            }
            Kind::Compact(_) => self.settle_compact(settled),
        }
        if let Some(completion) = completion {
            self.present(completion);
        }
    }

    // Draw the end after the core has restored and saved the owning session.
    fn settle_run(&mut self, settled: execution::host::Returned) {
        let execution::host::Returned {
            lane,
            kind,
            result: out,
            ended: how,
            unsend,
            returned: ran_back,
            ..
        } = settled;
        let was_turn = matches!(kind, Kind::Turn);
        let was_bash = matches!(kind, Kind::Bash { .. } | Kind::BashBackground { .. });

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
        self.drivers.turn_ended(token, &how);

        let ok = out.is_ok();
        // Non-model tasks have their ending in the operation output. The
        // streamed model run still closes its own live rows here.
        self.close_run(
            lane,
            if was_turn {
                out
            } else {
                Ok(Default::default())
            },
        );
        if lane == self.core.lanes.front() {
            if unsend && let Some(id) = self.core.lane().last_ask() {
                self.rewind_turn(id);
            }
        } else {
            // Out of sight: the lane says so in the statusline until someone looks,
            // and a prompt asked back is taken back then.
            self.core.lanes.at_mut(lane).end(ok, unsend);
        }
        // Last, and outside the split: a rewind rebuilds the whole surface, and
        // a row landed before it would go with the old drawing.
    }

    fn settle_compact(&mut self, settled: execution::host::Returned) {
        view_at(&mut self.views, self.core.lanes[settled.lane].token())
            .state
            .settle();
    }

    fn show_recovery(&mut self, settled: &execution::host::Returned) {
        use execution::host::Recovery;
        let verb = if matches!(settled.kind, Kind::Compact(_)) {
            "compaction"
        } else {
            "run"
        };
        let text = match &settled.recovery {
            Recovery::Unneeded => return,
            Recovery::Restored => {
                format!("the {verb} did not finish — back to the transcript as last saved")
            }
            Recovery::Failed(why) => format!(
                "the {verb} did not finish and its transcript could not be read back ({why}) — /new or /resume to use this checkout again"
            ),
        };
        self.say_notice_of(settled.lane, &text);
    }

    // Draw the end of a run into its lane's view, on screen or not.
    pub(super) fn close_run(&mut self, lane: usize, out: RunResult) {
        let token = self.core.lanes[lane].token();
        let view = view_at(&mut self.views, token);
        self.ui.close(view);
        // A cancelled run's calls got no `ToolEnd`; their animated rows
        // need to reach scrollback before the next flush freezes them.
        self.ui.abandon_tools(view);
        view.state.settle();
        match out {
            Ok(_) => {}
            Err(error) if error.is_cancelled() => {
                let stopped = Line::from(self.ui.paint.span(&self.ui.paint.theme.muted, "stopped"));
                self.ui.say(view, stopped);
            }
            Err(e) => {
                let mark = self.ui.paint.span(&self.ui.paint.theme.status.err, "error");
                let rest = Span::from(format!(" {e}"));
                self.ui.say(view, Line::from(vec![mark, rest]));
            }
        }
    }

    // Shutdown must preserve transcripts and the cause of stopped executions.
    pub(super) async fn settle_all(&mut self, why: &str) {
        for request in self.core.stop_runtime() {
            self.cancel_queued(request, why);
        }
        self.core.save_filed();
        if self.core.runtime_busy() {
            self.notice("stopping — saving what the runs have written".to_string());
            self.ui.flush(
                self.core.lane(),
                front_view(&mut self.views, self.core.lane()),
            );
            let waited = tokio::time::timeout(EXIT_GRACE, async {
                while self.core.runtime_busy() {
                    let event = self.core.next_runtime_event().await;
                    self.runtime_event(event);
                }
            })
            .await;
            if waited.is_err() {
                self.ui.screen.leave();
                eprintln!("some runs did not stop in time; their last save is what is on disk");
            }
        }
        self.core.finish_runtime_shutdown(why);
        if tokio::time::timeout(
            agent::STOP_GRACE + EXIT_GRACE,
            self.core.wait_background_shutdown(),
        )
        .await
        .is_err()
        {
            self.ui.screen.leave();
            eprintln!(
                "some background executions did not stop in time; their last save is what is on disk"
            );
        }
    }
}
