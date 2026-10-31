//! Adapting operation outputs to the local surface and the submitting channel.

use tokio::sync::mpsc::UnboundedSender;

use super::run::{Done, Fetched, Started};
use super::{Tui, clipboard, front_view, keys};
use pi_core::driver::{Ended, Origin, Said};
use pi_core::execution::{self, Action, Change, Completion, Output, Request};
use pi_core::input::{Intent, Rewound};
use toolbox::channel::Response;

impl Tui {
    /// Every admitted operation uses this executor, independently of where its
    /// text came from. Presentation consumes the result after execution.
    pub(super) async fn execute(
        &mut self,
        intent: Intent,
        origin: Origin,
        line: Option<String>,
        done: &UnboundedSender<Done>,
        fetched: &UnboundedSender<Fetched>,
        started: &UnboundedSender<Started>,
    ) -> bool {
        let request = Request::new(origin, self.core.lane().token());
        tracing::info!(target: "pi::session", request = request.id, ?origin,
            lane = request.lane, ?intent, "dispatch");
        let was = self.core.current;
        let action = execution::dispatch(&mut self.core, intent);
        self.reconcile(was);

        self.drivers.started(request, false);
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
                    Ended::Done,
                );
            }
            Action::Bash(command) => {
                if let Some(line) = line {
                    self.echo(&line);
                }
                self.start_bash(command, request, done);
            }
            Action::Compact(focus) => self.start_compact(focus, request, done),
            Action::Tool { name, args } => self.start_tool(name, args, request, started),
            Action::McpPrompt {
                prompt,
                args,
                typed,
            } => {
                self.start_fetch(prompt, args, typed, request, fetched);
            }
            Action::Edit(file) => {
                if matches!(origin, Origin::Input(_)) {
                    let why = "this operation needs the local terminal's editor".to_string();
                    self.complete(request, Output::Notice(why.clone()), Ended::Failed(why));
                } else {
                    let output = self.edit_file(file).await;
                    self.complete(request, output, Ended::Done);
                }
                self.ui.lists.forget();
            }
            Action::Surface { word, args: _ } => {
                let output = if word == keys::KEYS {
                    let mut listing = self.ui.keys.listing();
                    listing.rows.push(pi_store::listing::Row::new([
                        "Normal, with no menu: ctrl+k focuses the last execution. In executions: ctrl+k/j move; x stops; r restarts; X stops all; enter opens output; q or esc returns to input."
                    ]));
                    Output::Listing(listing)
                } else {
                    Output::Notice(format!("unknown command {word}"))
                };
                self.complete(request, output, Ended::Done);
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
                self.complete(request, output, Ended::Done);
            }
            Action::Prompt { send, typed } => {
                let ask = clipboard::prompt(send, typed, &self.ui.images);
                if !self.deliver_prompt_for(ask, request, line, done)
                    && !self.core.lane().is_running()
                    && let Some(said) = self.drivers.unstarted(request)
                {
                    self.say_of(self.core.current, super::by_pi(&said));
                }
            }
            Action::Quit => {
                self.complete(request, Output::Notice("exiting".into()), Ended::Done);
                return true;
            }
        }
        if !starts_turn && let Some(said) = self.drivers.unstarted(request) {
            self.say_of(self.core.current, super::by_pi(&said));
        }
        false
    }

    fn adopt_change(&mut self, change: Change) {
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

    pub(super) fn complete(&mut self, request: Request, output: Output, ended: Ended) {
        self.present(Completion {
            request,
            output,
            ended,
        });
    }

    /// One output is rendered for both consumers. Only command outputs pass
    /// here; incidental local notifications cannot become remote answers.
    pub(super) fn present(&mut self, completion: Completion) {
        let Completion {
            request,
            output,
            ended,
        } = completion;
        tracing::info!(target: "pi::session", request = request.id,
            lane = request.lane, outcome = ?ended, "completed");
        let text = match &output {
            Output::Notice(text) | Output::Text(text) | Output::Note(text) => text.clone(),
            Output::Listing(rows) => crate::listing::lines(rows).join("\n"),
            Output::Compacted(report) => pi_core::core::meter::compaction_line(report),
        };
        let text = if text.is_empty() {
            match ended {
                Ended::Done => "done".into(),
                Ended::Stopped | Ended::Unsent => "stopped".into(),
                Ended::Failed(why) => format!("failed: {why}"),
            }
        } else {
            text
        };
        self.drivers.respond(request, Response::Finished(text));
        let Some(at) = self.core.position_of(request.lane) else {
            return;
        };
        match output {
            Output::Notice(text) => {
                if at == self.core.current {
                    self.land_notice(text);
                } else if !text.is_empty() {
                    self.say_of(at, super::by_pi(&text));
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
            Output::Text(text) => {
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
