//! Adapting operation outputs to the local surface and the submitting channel.

use super::run::Replies;
use super::view::{Pending, Queued};
use super::{Tui, clipboard, front_view, keys};
use pi_core::driver::{Origin, Said};
use pi_core::execution::{self, Action, Change, Completion, Output, Request};
use pi_core::input::{Intent, Rewound};
use toolbox::channel::{Outcome, Response};

impl Tui {
    /// Every admitted operation uses this executor, independently of where its
    /// text came from. Presentation consumes the result after execution.
    pub(super) async fn execute(
        &mut self,
        intent: Intent,
        origin: Origin,
        line: Option<String>,
        replies: &Replies,
    ) -> bool {
        let request = Request::new(origin, self.core.lane().token());
        self.execute_for(intent, request, line, replies).await
    }

    pub(super) async fn execute_for(
        &mut self,
        intent: Intent,
        request: Request,
        line: Option<String>,
        replies: &Replies,
    ) -> bool {
        let origin = request.origin;
        tracing::info!(target: "pi::session", request = request.id, ?origin,
            lane = request.lane, ?intent, "dispatch");
        let was = self.core.current;
        let action = execution::dispatch(&mut self.core, intent);
        self.reconcile(was);

        if !matches!(action, Action::Prompt { .. }) || !self.core.lane().is_running() {
            self.drivers.started(request, false);
        }
        let starts_turn = matches!(action, Action::Prompt { .. } | Action::McpPrompt { .. });
        match action {
            Action::Complete { change, output } => {
                self.adopt_change(change);
                self.complete(
                    Request {
                        lane: self.core.lane().token(),
                        ..request
                    },
                    output,
                    Outcome::Done,
                );
            }
            Action::Bash(command) => {
                if let Some(line) = line {
                    self.echo(&line);
                }
                self.start_bash(command, request, &replies.done);
            }
            Action::Compact(focus) => self.start_compact(focus, request, &replies.done),
            Action::Tool { name, args } => self.start_tool(name, args, request, &replies.started),
            Action::McpPrompt {
                prompt,
                args,
                typed,
            } => {
                self.start_fetch(prompt, args, typed, request, &replies.fetched);
            }
            Action::Removal(removal) => self.start_removal(removal, request, &replies.removed),
            Action::Edit(file) => {
                if matches!(origin, Origin::Input(_)) {
                    let why = "this operation needs the local terminal's editor".to_string();
                    self.complete(request, Output::Notice(String::new()), Outcome::Failed(why));
                } else {
                    let output = self.edit_file(file).await;
                    self.complete(request, output, Outcome::Done);
                }
                self.ui.lists.forget();
            }
            Action::Surface { word, args: _ } => {
                let output = if word == keys::KEYS {
                    let mut listing = self.ui.keys.listing();
                    listing.rows.push(pi_store::listing::Row::new([format!(
                        "Normal, with no menu: ctrl+k focuses the last execution. In executions: {}",
                        super::background::HINT
                    )]));
                    Output::Listing(listing)
                } else {
                    Output::Notice(format!("unknown command {word}"))
                };
                self.complete(request, output, Outcome::Done);
            }
            Action::Drive(drive) => {
                let lane = self.core.lane();
                let said = self.drivers.command(drive, lane.token(), lane.ctx());
                let output = match said {
                    Said::Nothing => Output::Notice(String::new()),
                    Said::Notice(text) => Output::Notice(text),
                    Said::Noted(text, note) => {
                        let lane = self.core.lane_mut();
                        let view = front_view(&mut self.views, lane);
                        self.ui.say_note(lane, view, &note);
                        Output::Notice(text)
                    }
                    Said::Transcript(text) => Output::Text(text),
                };
                self.complete(request, output, Outcome::Done);
            }
            Action::Prompt { send, typed } => {
                let ask = clipboard::prompt(send, typed, &self.ui.images);
                if !self.deliver_prompt_for(ask, request, line, &replies.done)
                    && !self.core.lane().is_running()
                    && let Some(said) = self.drivers.unstarted(request)
                {
                    self.say_notice_of(self.core.current, &said);
                }
            }
            Action::Quit => {
                self.complete(request, Output::Notice("exiting".into()), Outcome::Done);
                return true;
            }
        }
        if !starts_turn && let Some(said) = self.drivers.unstarted(request) {
            self.say_notice_of(self.core.current, &said);
        }
        false
    }

    pub(super) fn enqueue(&mut self, queued: Queued) {
        let request = queued.request;
        match &queued.input {
            Pending::Intent(intent) => {
                tracing::info!(target: "pi::session", request = request.id,
                    lane = request.lane, origin = ?request.origin, ?intent, "dispatch queued");
            }
            Pending::Prompt(_) => {
                tracing::info!(target: "pi::session", request = request.id,
                    lane = request.lane, origin = ?request.origin, "prompt queued");
            }
        }
        front_view(&mut self.views, self.core.lane())
            .queued
            .push(queued);
        self.drivers.respond(request, Response::Queued);
    }

    pub(super) async fn resume_queued(&mut self, queued: Queued, replies: &Replies) -> bool {
        let Queued {
            input,
            request,
            line,
        } = queued;
        match input {
            Pending::Intent(intent) => self.execute_for(intent, request, line, replies).await,
            Pending::Prompt(ask) => {
                self.deliver_prompt_for(ask, request, line, &replies.done);
                false
            }
        }
    }

    pub(super) fn cancel_queued(&self, request: Request, reason: &str) {
        tracing::info!(target: "pi::session", request = request.id,
            lane = request.lane, reason, "queued request cancelled");
        self.drivers.respond(
            request,
            Response::Finished {
                text: String::new(),
                outcome: Outcome::Stopped(reason.into()),
            },
        );
    }

    pub(super) fn adopt_change(&mut self, change: Change) {
        match change {
            Change::None => {}
            Change::Session => {
                self.rebuild_front();
                self.ui.lists.at(self.core.lane().root());
            }
            Change::Directory => self.follow_root(),
            Change::Worktrees => self.ui.lists.forget(),
            Change::Rewound(outcome) => match outcome {
                Rewound::Nothing => {}
                Rewound::Kept => self.rebuild_front(),
                Rewound::Unsent(text) => {
                    self.rebuild_front();
                    if self.ui.editor.is_empty() {
                        self.ui.editor.set_line(&text);
                    }
                }
            },
        }
    }

    pub(super) fn complete(&mut self, request: Request, output: Output, outcome: Outcome) {
        self.present(Completion {
            request,
            output,
            outcome,
        });
    }

    /// Command content and its outcome travel separately. Incidental local
    /// notifications cannot become remote answers.
    pub(super) fn present(&mut self, completion: Completion) {
        let Completion {
            request,
            output,
            outcome,
        } = completion;
        tracing::info!(target: "pi::session", request = request.id,
            lane = request.lane, ?outcome, "completed");
        let text = match &output {
            Output::Notice(text) | Output::Text(text) | Output::Note(text) => text.clone(),
            Output::Listing(rows) => crate::listing::lines(rows).join("\n"),
            Output::Compacted(report) => pi_core::core::meter::compaction_line(report),
        };
        let status = match &outcome {
            Outcome::Done => None,
            Outcome::Stopped(reason) => Some(if reason.is_empty() {
                "stopped".to_string()
            } else {
                format!("stopped: {reason}")
            }),
            Outcome::Failed(why) => Some(format!("failed: {why}")),
            Outcome::Exited(0) if text.is_empty() => Some("exit 0, no output".into()),
            Outcome::Exited(0) => None,
            Outcome::Exited(code) => Some(format!("exit {code}")),
        };
        self.drivers
            .respond(request, Response::Finished { text, outcome });
        let Some(at) = self.core.position_of(request.lane) else {
            return;
        };
        match output {
            Output::Notice(mut text) => {
                if let Some(status) = status {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&status);
                }
                if at == self.core.current {
                    self.land_notice(text);
                } else if !text.is_empty() {
                    self.say_notice_of(at, &text);
                }
            }
            Output::Listing(rows) => {
                if at == self.core.current {
                    self.land_lines(rows);
                } else {
                    for line in crate::listing::lines(&rows) {
                        self.say_of(at, line);
                    }
                }
            }
            Output::Text(mut text) => {
                if let Some(status) = status {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&status);
                }
                super::view_at(&mut self.views, request.lane)
                    .surface
                    .scrollback
                    .extend(
                        text.lines()
                            .map(|line| super::row::Row::notice(line.to_string())),
                    );
            }
            Output::Note(text) => {
                let lane = &mut self.core.lanes[at];
                let view = super::view::opened(&mut self.views, lane, &self.ui.paint);
                self.ui.say_note(lane, view, &text);
            }
            Output::Compacted(report) => {
                let _ = self.core.lanes[at]
                    .sender()
                    .send(agent::Event::Compacted(report));
            }
        }
    }
}
