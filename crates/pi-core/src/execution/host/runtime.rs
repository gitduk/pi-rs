//! The host event loop owns executions and advances every live conversation.

use super::{Deferred, Settlement};
use crate::core::Core;
use crate::driver::background::{Commands, Table};
use crate::execution::{Done, Kind, Request};
use agent::session::{Entry, EntryId};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::task::Poll;
use tokio::task::{Id, JoinError, JoinSet};

pub struct Runtime {
    tasks: JoinSet<Done>,
    owners: HashMap<Id, Owner>,
    events: VecDeque<Event>,
    completed: Option<Done>,
    fallback: Arc<Table>,
    closing: bool,
    next_lane: usize,
    // A title written off the run, waiting for the loop to file it: the
    // session's row has one writer, and this is where it writes.
    titles: tokio::sync::mpsc::UnboundedSender<crate::core::title::Titled>,
    titled: tokio::sync::mpsc::UnboundedReceiver<crate::core::title::Titled>,
}

impl Default for Runtime {
    fn default() -> Self {
        let (titles, titled) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tasks: JoinSet::default(),
            owners: HashMap::default(),
            events: VecDeque::default(),
            completed: None,
            fallback: Arc::default(),
            closing: false,
            next_lane: 0,
            titles,
            titled,
        }
    }
}

struct Owner {
    request: Request,
    kind: TaskKind,
}

enum TaskKind {
    Model,
    Bash,
    Compact,
}

pub struct Started {
    pub notices: Vec<String>,
    pub reconfigured: bool,
    pub request: Request,
    pub message: Arc<message::Message>,
    pub input: EntryId,
    pub entries: Vec<Entry>,
    pub line: Option<String>,
}

pub enum Event {
    Activity,
    Started(Started),
    Agent { lane: u64, event: agent::Event },
    Settled(Settlement),
    Deferred(Deferred),
    Rejected { request: Request, message: String },
    Stopping { lane: u64 },
}

impl Runtime {
    /// Where a title written off the run is handed back to the loop.
    pub(crate) fn titles(&self) -> tokio::sync::mpsc::UnboundedSender<crate::core::title::Titled> {
        self.titles.clone()
    }

    fn spawn(&mut self, owner: Owner, future: impl Future<Output = Done> + Send + 'static) {
        let id = self.tasks.spawn(future).id();
        self.owners.insert(id, owner);
    }

    fn complete(&mut self, result: Result<(Id, Done), JoinError>) {
        let id = match &result {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        let owner = self
            .owners
            .remove(&id)
            .expect("every execution has its original owner");
        self.completed = Some(match result {
            Ok((_, done)) => done,
            Err(error) => {
                tracing::error!(target:"pi::session",request=owner.request.id,lane=owner.request.lane,%error,"execution task exited unexpectedly");
                let kind = match owner.kind {
                    TaskKind::Model => Kind::Turn,
                    TaskKind::Bash => Kind::Bash { lines: Vec::new() },
                    TaskKind::Compact => Kind::Compact(None),
                };
                Done {
                    request: owner.request,
                    kind,
                    ran: None,
                }
            }
        });
    }
}

impl Core {
    fn execution_table(&self) -> Arc<Table> {
        self.executions
            .clone()
            .unwrap_or_else(|| self.runtime.fallback.clone())
    }

    pub fn start_model(
        &mut self,
        input: super::super::messages::AdmittedInput,
        request: Request,
    ) -> Result<Started, String> {
        if self.runtime.closing {
            return Err("host is stopping".into());
        }
        let at = self
            .position_of(request.lane)
            .ok_or("session is no longer open")?;
        if !self.lanes[at].ready_for_input() || !input.is_next() {
            return Err("input is not ready to run".into());
        }
        if input.message().session().as_str() != self.lanes[at].id() {
            return Err("input belongs to a different session".into());
        }
        let previous = self.lanes[at].session().map_or(0, |s| s.entries().len());
        let (notices, reconfigured) = self.prepare_model_lane(at);
        // The question this session opened with is what a row names it by
        // until someone names it: the model gets one go at a better line, on
        // the connection this turn ended up with.
        if previous == 0 {
            self.start_title(at, input.prompt().shown_text().to_owned());
        }
        let mut entries = self.lanes[at]
            .session()
            .map(|session| session.entries()[previous..].to_vec())
            .unwrap_or_default();
        let table = self.execution_table();
        let retry = self.lanes[at].resolved().retry;
        let opened = super::prepare(
            self.lanes.at_mut(at),
            input,
            request,
            &self.store,
            table,
            retry,
        )?;
        let message = opened.run.message();
        entries.extend(opened.entries);
        self.runtime.spawn(
            Owner {
                request,
                kind: TaskKind::Model,
            },
            opened.run.run(),
        );
        Ok(Started {
            notices,
            reconfigured,
            request,
            message,
            input: opened.input,
            entries,
            line: None,
        })
    }

    fn prepare_model_lane(&mut self, at: usize) -> (Vec<String>, bool) {
        let refreshed = self.refresh_config_at(at);
        let reconfigured = refreshed.is_some();
        let mut notices = refreshed.unwrap_or_default();
        notices.extend(self.refresh_prompt_files_at(at).unwrap_or_default());
        if let Some(note) = self.lanes[at].drift() {
            self.lanes.at_mut(at).push_note(&note);
        }
        (notices, reconfigured)
    }

    pub fn start_bash(&mut self, request: Request, command: String) -> Result<(), String> {
        if self.runtime.closing {
            return Err("host is stopping".into());
        }
        let at = self
            .position_of(request.lane)
            .ok_or("session is no longer open")?;
        let table = self.execution_table();
        let lane = self.lanes.at_mut(at);
        if !lane.ready_for_input() {
            return Err("session is not ready to run".into());
        }
        let session = lane
            .take_session()
            .ok_or("session transcript is unavailable")?;
        let cancel = lane.ctx().cancel.child_token();
        let ctx = lane.ctx_for(cancel.clone());
        lane.begin_for(request, cancel);
        let shell = pi_tools::bash::Bash::new(Arc::new(Commands::new(table)));
        self.runtime.spawn(
            Owner {
                request,
                kind: TaskKind::Bash,
            },
            crate::execution::bash(request, session, ctx, command, shell),
        );
        Ok(())
    }

    pub fn start_compact(&mut self, request: Request, focus: Option<String>) -> Result<(), String> {
        if self.runtime.closing {
            return Err("host is stopping".into());
        }
        let at = self
            .position_of(request.lane)
            .ok_or("session is no longer open")?;
        let lane = self.lanes.at_mut(at);
        if !lane.ready_for_input() {
            return Err("session is not ready to run".into());
        }
        let session = lane
            .take_session()
            .ok_or("session transcript is unavailable")?;
        let cancel = lane.ctx().cancel.child_token();
        let agent = lane.agent().clone();
        let session_id = Some(lane.id().to_owned());
        lane.begin_for(request, cancel.clone());
        self.runtime.spawn(
            Owner {
                request,
                kind: TaskKind::Compact,
            },
            crate::execution::compact(request, session, agent, focus, session_id, cancel),
        );
        Ok(())
    }

    fn advance_inputs(&mut self) {
        if self.runtime.closing {
            return;
        }
        let deferred = self.admit_background_results();
        self.runtime
            .events
            .extend(deferred.into_iter().map(Event::Deferred));
        for queued in self.take_ready_inputs() {
            let crate::execution::queue::Pending::Prompt(input) = queued.input else {
                unreachable!("model input selection")
            };
            let note = self.position_of(queued.request.lane).and_then(|at| {
                let lane = self.lanes.at_mut(at);
                if queued.note.is_empty() {
                    return None;
                }
                lane.push_note(&queued.note)?;
                lane.session()?.entries().last().cloned()
            });
            let event = match self.start_model(input, queued.request) {
                Ok(mut started) => {
                    if let Some(note) = note {
                        started.entries.insert(0, note);
                    }
                    started.line = queued.line;
                    Event::Started(started)
                }
                Err(message) => Event::Rejected {
                    request: queued.request,
                    message,
                },
            };
            self.runtime.events.push_back(event);
        }
    }

    pub fn interrupt_session(&mut self, scope: &::execution::background::Scope) -> Option<u64> {
        let at = self.lane_at(&scope.root)?;
        let lane = self.lanes.at_mut(at);
        if let Some(owner) = &scope.owner
            && owner.session != lane.id()
        {
            tracing::info!(target:"pi::session",source=%owner.session,session=lane.id(),"ignored interrupt for another session");
            return None;
        }
        lane.stop(false).then(|| lane.token())
    }

    /// Stop admission before cancellation, so completion cannot launch another run.
    pub fn stop_runtime(&mut self) -> Vec<Request> {
        if !self.runtime.closing {
            tracing::info!(target:"pi::session",executions=self.runtime.owners.len(),"host stopping");
            self.runtime.closing = true;
        }
        let mut cancelled = Vec::new();
        let services = self.services.clone();
        for lane in self.lanes.all_mut() {
            lane.close_inputs();
            if let Some(services) = &services {
                services.unbind(lane.id());
            }
            cancelled.extend(lane.queue().lock().drain().map(|queued| queued.request));
            lane.cancel();
        }
        cancelled
    }

    pub fn finish_runtime_shutdown(&mut self, why: &str) {
        let table = self.execution_table();
        for lane in self.lanes.all_mut() {
            let stopped = table.stop_running(lane.root(), why);
            if !stopped.is_empty() {
                lane.push_note(&format!("{why} pi stopped running background commands {} during shutdown. They did not finish normally; their results will not be reported.",stopped.join(", ")));
            }
        }
        self.save_filed();
    }

    /// Join background cleanup before the final transcript flush.
    pub async fn wait_background_shutdown(&mut self) {
        self.execution_table().wait_tasks().await;
        self.save_filed();
    }

    pub fn runtime_busy(&self) -> bool {
        !self.runtime.owners.is_empty() || self.runtime.completed.is_some()
    }

    fn record_agent(&mut self, lane: u64, event: agent::Event) -> Event {
        if let Some(at) = self.position_of(lane) {
            self.lanes.at_mut(at).note(&event);
        }
        Event::Agent { lane, event }
    }

    /// Drain live events without moving execution ownership to a consumer.
    pub fn try_agent_event(&mut self) -> Option<Event> {
        // A queued start must reach the consumer before any event from its run.
        if !self.runtime.events.is_empty() {
            return None;
        }
        let count = self.lanes.len();
        for offset in 0..count {
            let at = (self.runtime.next_lane + offset) % count;
            let lane = self.lanes.at_mut(at);
            if let Ok(event) = lane.inbox().try_recv() {
                let token = lane.token();
                self.runtime.next_lane = (at + 1) % count;
                return Some(self.record_agent(token, event));
            }
        }
        None
    }

    /// Cancel-safe host loop. State changes commit before an event is returned.
    pub async fn next_runtime_event(&mut self) -> Event {
        loop {
            if let Some(done) = &self.runtime.completed {
                if let Some(at) = self.position_of(done.request.lane) {
                    let lane = self.lanes.at_mut(at);
                    if let Ok(event) = lane.inbox().try_recv() {
                        let token = lane.token();
                        return self.record_agent(token, event);
                    }
                }
                let done = self.runtime.completed.take().expect("completed execution");
                return Event::Settled(self.settle_execution(done));
            }
            if let Some(event) = self.runtime.events.pop_front() {
                return event;
            }
            let table = self.execution_table();
            for scope in table.take_interrupts() {
                if let Some(lane) = self.interrupt_session(&scope) {
                    self.runtime.events.push_back(Event::Stopping { lane });
                }
            }
            table.relay_progress();
            self.advance_inputs();
            if let Some(event) = self.runtime.events.pop_front() {
                return event;
            }
            if let Some(result) = self.runtime.tasks.try_join_next_with_id() {
                self.runtime.complete(result);
                continue;
            }
            if let Some(event) = self.try_agent_event() {
                return event;
            }
            let due = table.progress_due();
            let lanes = &mut self.lanes;
            tokio::select! {
                Some(result) = self.runtime.tasks.join_next_with_id(), if !self.runtime.owners.is_empty() => self.runtime.complete(result),
                (lane,event) = std::future::poll_fn(|cx| {
                    for lane in lanes.all_mut() {
                        if let Poll::Ready(Some(event)) = lane.inbox().poll_recv(cx) {return Poll::Ready((lane.token(),event));}
                    }
                    Poll::Pending
                }) => return self.record_agent(lane,event),
                Some(titled) = self.runtime.titled.recv() => {
                    self.land_title(titled);
                    return Event::Activity;
                }
                () = table.changed().notified() => return Event::Activity,
                () = tokio::time::sleep_until(due.unwrap_or_else(tokio::time::Instant::now)), if due.is_some() => {},
            }
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn fixture_completion(&mut self, done: Done) {
        let kind = match done.kind {
            Kind::Turn => TaskKind::Model,
            Kind::Bash { .. } | Kind::BashBackground { .. } => TaskKind::Bash,
            Kind::Compact(_) => TaskKind::Compact,
        };
        self.runtime.spawn(
            Owner {
                request: done.request,
                kind,
            },
            async move { done },
        );
    }
}
