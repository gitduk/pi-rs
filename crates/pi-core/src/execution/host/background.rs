//! Admit automatic inputs for every idle session, without a selected-session policy.

use crate::core::{Core, lane::Lane};
use crate::driver::{Origin, background::Table};
use crate::execution::{
    Request,
    queue::{Pending, Queued},
};
use std::sync::Arc;

pub struct Deferred {
    pub lane: u64,
    pub message: String,
}

impl Core {
    /// At most one completion per session per pass, queued in admission order.
    pub fn admit_background_results(&self) -> Vec<Deferred> {
        let Some(table) = &self.executions else {
            return Vec::new();
        };
        self.lanes
            .all()
            .iter()
            .filter_map(|lane| admit(lane, table))
            .collect()
    }
}

pub(super) fn admit(lane: &Lane, table: &Arc<Table>) -> Option<Deferred> {
    // A run holds the transcript but still reads what queues behind it.
    if lane.session().is_none() && !lane.is_running() {
        return None;
    }
    let delivery = table.claim_result(lane.root(), lane.id())?;
    let admitted = lane
        .messages()
        .admit_wire(delivery.message(lane.messages().session()));
    let input = match admitted {
        Ok(crate::execution::messages::Admission::Accepted(input)) => input.presentation(
            delivery.due.label.clone(),
            delivery.due.note_for(lane.ctx()),
        ),
        Ok(crate::execution::messages::Admission::Existing { .. }) => {
            delivery.commit();
            return None;
        }
        Err(error) => {
            let message = error.to_string();
            return delivery.refused(&message).then_some(Deferred {
                lane: lane.token(),
                message,
            });
        }
    };
    if delivery.commit() {
        lane.queue().push(Queued {
            input: Pending::Prompt(input),
            request: Request::new(Origin::Background, lane.token()),
            line: None,
            note: String::new(),
        });
    }
    None
}
