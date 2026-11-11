//! Preparing and saving model runs without a rendering or input frontend.

mod background;
mod runtime;
mod settle;
pub use background::Deferred;
pub use runtime::{Event, Runtime, Started};
pub use settle::{Recovery, Returned, Settlement};

use super::messages::AdmittedInput;
use super::{Done, Request, Turn};
use crate::core::lane::{Lane, Save};
use crate::driver::{Ended, background::Table};
use agent::session::{Entry, EntryId};
use std::sync::Arc;

/// A run owns the transcript and its captured save destination until completion.
#[must_use = "a prepared model run owns the session transcript"]
pub struct ModelRun {
    turn: Turn,
    save: Save,
}

/// Facts a frontend may use to bind its view before polling the execution.
pub struct Opened {
    pub input: EntryId,
    pub entries: Vec<Entry>,
    pub run: ModelRun,
}

/// Move one admitted request into execution on its original session.
pub fn prepare(
    lane: &mut Lane,
    input: AdmittedInput,
    request: Request,
    store: &pi_store::session::Store,
    executions: Arc<Table>,
    retry: agent::Retry,
) -> Result<Opened, String> {
    if request.lane != lane.token() || input.message().session().as_str() != lane.id() {
        return Err("input belongs to a different session".into());
    }
    if !lane.ready_for_input() || !input.is_next() {
        return Err("input is not ready to run".into());
    }
    let Some(mut session) = lane.take_session() else {
        input.reject(super::messages::Failure {
            code: "SESSION_UNAVAILABLE".into(),
            message: "session transcript is unavailable".into(),
        });
        return Err("session transcript is unavailable".into());
    };
    let (prompt, responder) = match input.start() {
        Ok(started) => started,
        Err(error) => {
            lane.return_session(session);
            return Err(error.to_string());
        }
    };
    let previous = session.entries().len();
    let entry = session.send(prompt);
    let entries = session.entries()[previous..].to_vec();
    let cancel = lane.ctx().cancel.child_token();
    let turn = Turn {
        responder,
        request,
        session,
        agent: lane.agent().clone(),
        ctx: lane.ctx_for(cancel.clone()),
        events: lane.sender().clone(),
        input: crate::driver::input::Input::new(
            lane.root().to_path_buf(),
            executions,
            lane.notes(),
        )
        .with_queue(lane.queue().clone()),
        retry,
    };
    let save = Save::of(lane, store);
    lane.seed_meter();
    lane.begin_for(request, cancel);
    tracing::info!(target: "pi::session", request = request.id, lane = request.lane, message_id = turn.responder.request().id().as_str(), session = lane.id(), "model execution started");
    Ok(Opened {
        input: entry,
        entries,
        run: ModelRun { turn, save },
    })
}

impl ModelRun {
    pub fn request(&self) -> Request {
        self.turn.request
    }

    pub fn message(&self) -> Arc<message::Message> {
        self.turn.responder.request()
    }

    /// Save before returning the completion. Dropping a receiver cannot skip it.
    pub async fn run(self) -> Done {
        let Self { turn, save } = self;
        let messages = turn.responder.messages();
        let meter = turn.agent.meter.clone();
        let mut done = turn.run().await;
        let ended = done.ended();
        if let Some((session, _)) = &mut done.ran {
            match ended {
                Ended::Done | Ended::Unsent => {}
                Ended::Stopped => session.note_user_stop(),
                Ended::Failed(why) => session.note_failure(why),
            }
        }
        let snapshot = messages.snapshot();
        crate::core::saves::wait(move || {
            if let Some((session, _)) = &done.ran {
                let _ = save.write(session, &snapshot, meter.read());
            } else {
                // Preserve the last native archive when an execution unwinds.
                let _ = save.write_messages(&snapshot);
            }
            done
        })
        .await
        .expect("the transcript saver returns its completed execution")
    }
}

impl crate::core::Core {
    /// One eligible input per idle session. Selection does not depend on a frontend.
    pub fn take_ready_inputs(&self) -> Vec<super::queue::Queued> {
        ready(self.lanes.all())
    }
}

fn ready(lanes: &[Lane]) -> Vec<super::queue::Queued> {
    lanes
        .iter()
        .filter(|lane| lane.ready_for_input())
        .filter_map(|lane| lane.queue().pop_ready())
        .collect()
}

#[cfg(test)]
mod tests;
