//! Keyboard focus for the background jobs above the input line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_core::driver::jobs::{Job, State};
use pi_store::keys::Mode;

use super::{Asked, Deed, Ui};

pub(super) const HINT: &str =
    "ctrl+k/j move · k stop · r restart · K stop all · enter output · esc back";

impl Ui {
    // A stopped row must remain available for restart while focused.
    pub(super) fn sync_jobs(&mut self, all: Vec<Job>) {
        let at = self
            .job_focus
            .and_then(|id| self.jobs.iter().position(|j| j.id == id));
        let focused = self.job_focus.is_some();
        let jobs = all
            .iter()
            .filter(|j| {
                j.state == State::Running || (focused && self.jobs.iter().any(|old| old.id == j.id))
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(id) = self.job_focus
            && !jobs.iter().any(|j| j.id == id)
        {
            self.job_focus = at
                .and_then(|at| jobs.get(at.min(jobs.len().saturating_sub(1))))
                .map(|j| j.id);
        }
        self.every_job = all;
        self.jobs = jobs;
    }

    pub(super) fn leave_jobs(&mut self) {
        if self.job_focus.take().is_some() {
            self.jobs.retain(|j| j.state == State::Running);
            self.disarm();
        }
    }

    // Menus and reading pages own their keys even with jobs present.
    pub(super) fn job_key(&mut self, key: KeyEvent) -> Option<Asked> {
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        if self.job_focus.is_none() && !(ctrl && key.code == KeyCode::Char('k')) {
            return None;
        }
        if self.vim != Some(Mode::Normal) || self.reading() || !self.menu().is_empty() {
            self.leave_jobs();
            return None;
        }
        let Some(id) = self.job_focus else {
            self.job_focus = self.jobs.last().map(|j| j.id);
            self.disarm();
            return Some(Asked::Own(Deed::Nothing));
        };
        self.disarm();
        let deed = if ctrl {
            match key.code {
                KeyCode::Char('k') | KeyCode::Char('j') => {
                    let at = self.jobs.iter().position(|j| j.id == id).unwrap_or(0);
                    let next = if key.code == KeyCode::Char('k') {
                        Some(at.saturating_sub(1))
                    } else {
                        at.checked_add(1).filter(|&next| next < self.jobs.len())
                    };
                    match next.and_then(|at| self.jobs.get(at)) {
                        Some(job) => self.job_focus = Some(job.id),
                        None => self.leave_jobs(),
                    }
                    None
                }
                _ => None,
            }
        } else if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            match key.code {
                KeyCode::Esc => {
                    self.leave_jobs();
                    None
                }
                KeyCode::Enter => return Some(Asked::Own(Deed::Job(id))),
                KeyCode::Char('k') if key.modifiers.is_empty() => Some(Deed::StopJobs(vec![id])),
                KeyCode::Char('r') if key.modifiers.is_empty() => Some(Deed::RestartJobs(vec![id])),
                KeyCode::Char('K') => Some(Deed::StopJobs(Vec::new())),
                KeyCode::Char('k') if key.modifiers == KeyModifiers::SHIFT => {
                    Some(Deed::StopJobs(Vec::new()))
                }
                _ => None,
            }
        } else {
            None
        };
        Some(Asked::Own(deed.unwrap_or(Deed::Nothing)))
    }
}
