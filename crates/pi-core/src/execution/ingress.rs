//! A live conversation's semantic input endpoint.

use super::Request;
use super::messages::{Admission, Messages};
use super::queue::{Pending, Queue, Queued};
use crate::driver::Origin;
use ::execution::channel::Receipt;
use message::WireMessage;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SubmitError {
    #[error("session is no longer accepting inputs")]
    Closed,
    #[error("{0}")]
    Rejected(String),
}

/// Kept by the conversation; connection tables keep only weak references.
pub(crate) struct Ingress {
    pub root: PathBuf,
    pub messages: Messages,
    pub queue: Queue,
    pub lane: u64,
    accepting: std::sync::Mutex<bool>,
}

impl Ingress {
    pub fn new(root: PathBuf, lane: u64, messages: Messages, queue: Queue) -> Self {
        Self {
            root,
            lane,
            messages,
            queue,
            accepting: std::sync::Mutex::new(true),
        }
    }

    pub fn close(&self) {
        *self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    }

    pub fn is_busy(&self) -> bool {
        self.messages.is_busy() || !self.queue.is_empty()
    }

    pub fn submit(&self, execution: u64, wire: WireMessage) -> Result<Receipt, SubmitError> {
        // Close waits for in-flight admission and queue insertion to finish.
        let accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*accepting {
            return Err(SubmitError::Closed);
        }
        let (request, response) = match self
            .messages
            .admit_wire(wire)
            .map_err(|e| SubmitError::Rejected(e.to_string()))?
        {
            Admission::Accepted(input) => {
                let request = input.message().request_id().expect("admitted input");
                self.queue.push(Queued {
                    input: Pending::Prompt(input),
                    request: Request::new(Origin::Message(execution), self.lane),
                    line: None,
                    note: String::new(),
                });
                (request, None)
            }
            Admission::Existing { request, response } => (request, response),
        };
        Ok(Receipt {
            cursor: self
                .messages
                .request_cursor(&request)
                .map_err(|e| SubmitError::Rejected(e.to_string()))?,
            request,
            response,
        })
    }
}
