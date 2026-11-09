//! A live conversation's semantic input endpoint.

use super::Request;
use super::messages::{Admission, Messages};
use super::queue::{Pending, Queue, Queued};
use crate::driver::Origin;
use message::WireMessage;
use pi_tools::runtime::channel::Receipt;
use std::path::PathBuf;

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

    pub fn submit(&self, execution: u64, wire: WireMessage) -> Result<Receipt, String> {
        // Close waits for in-flight admission and queue insertion to finish.
        let accepting = self
            .accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*accepting {
            return Err("session is no longer accepting inputs".into());
        }
        let (request, response) = match self.messages.admit_wire(wire).map_err(|e| e.to_string())? {
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
                .map_err(|e| e.to_string())?,
            request,
            response,
        })
    }
}
