//! Pending operations in admission order. Active runs never consume new inputs.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use super::Request;
use crate::input::Intent;

pub struct Queued {
    pub input: Pending,
    pub request: Request,
    /// Original submitted text, when the operation can be recalled.
    pub line: Option<String>,
    /// Context to file when this operation begins.
    pub note: String,
}

pub enum Pending {
    Intent(Intent),
    Prompt(super::messages::AdmittedInput),
}

#[derive(Clone, Default)]
pub struct Queue(Arc<Mutex<PendingQueue>>);

#[derive(Default)]
pub struct PendingQueue {
    entries: VecDeque<Queued>,
}

impl PendingQueue {
    pub fn iter(&self) -> impl Iterator<Item = &Queued> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Recall editable submissions, retaining the remaining operations in order.
    pub fn recall(&mut self) -> Vec<(Request, String)> {
        let mut recalled = Vec::new();
        self.entries.retain_mut(|entry| {
            if let Some(line) = entry.line.take() {
                recalled.push((entry.request, line));
                false
            } else {
                true
            }
        });
        recalled
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Queued> {
        self.entries.drain(..)
    }
}

impl Queue {
    pub fn lock(&self) -> MutexGuard<'_, PendingQueue> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn push(&self, queued: Queued) {
        self.lock().entries.push_back(queued);
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Reserve the next admitted model input without crossing a queued host operation.
    pub fn pop_ready(&self) -> Option<Queued> {
        let mut queue = self.lock();
        // Concurrent producers can enqueue in a different order from admission.
        // The ticket decides readiness; a host operation still forms a boundary.
        let at = queue
            .entries
            .iter()
            .take_while(|queued| matches!(queued.input, Pending::Prompt(_)))
            .position(
                |queued| matches!(&queued.input, Pending::Prompt(input) if input.is_next()),
            )?;
        queue.entries.remove(at)
    }

    pub fn has_host_operation(&self) -> bool {
        self.lock()
            .entries
            .front()
            .is_some_and(|queued| matches!(queued.input, Pending::Intent(_)))
    }

    pub fn pop_host_operation(&self) -> Option<Queued> {
        let mut queue = self.lock();
        matches!(queue.entries.front()?.input, Pending::Intent(_))
            .then(|| queue.entries.pop_front().expect("host operation"))
    }

    pub fn pop_front(&self) -> Option<Queued> {
        self.lock().entries.pop_front()
    }
}
