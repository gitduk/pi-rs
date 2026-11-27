//! Keyboard focus for the background commands above the input line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_core::driver::background::{BackgroundCommand, State};

use super::{Asked, Deed, Ui};

pub(super) const HINT: &str = "j/k move · ctrl+k/j move across input · x stop · r restart · X stop all · enter output · q/esc back";

fn is_active_job(state: State) -> bool {
    matches!(state, State::Running | State::Back)
}

impl Ui {
    // Keep stopped rows available for restart while the drawer is open.
    pub(super) fn sync_background(&mut self, all: Vec<BackgroundCommand>) {
        if !all.iter().any(|j| is_active_job(j.state)) {
            self.leave_background();
        }
        let at = self
            .background_focus
            .and_then(|id| self.executions.iter().position(|j| j.id == id));
        let open = self.jobs_open;
        let executions = all
            .iter()
            .filter(|j| {
                is_active_job(j.state) || (open && self.executions.iter().any(|old| old.id == j.id))
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(id) = self.background_focus
            && !executions.iter().any(|j| j.id == id)
        {
            self.background_focus = at
                .and_then(|at| executions.get(at.min(executions.len().saturating_sub(1))))
                .map(|j| j.id);
        }
        if executions.is_empty() {
            self.jobs_open = false;
            self.background_focus = None;
            self.background_open = None;
        }
        self.every_command = all;
        self.executions = executions;
    }

    pub(super) fn toggle_jobs(&mut self) {
        if self.jobs_open {
            self.leave_background();
            return;
        }
        if self.executions.is_empty() {
            return;
        }
        self.jobs_open = true;
        self.disarm();
        if self.vim.is_some() && !self.reading() && self.menu().is_empty() {
            self.background_focus = self.executions.last().map(|j| j.id);
        }
    }

    pub(super) fn leave_background(&mut self) {
        if self.jobs_open || self.background_focus.is_some() {
            self.jobs_open = false;
            self.background_focus = None;
            self.background_open = None;
            self.executions.retain(|j| is_active_job(j.state));
            self.disarm();
        }
    }

    // Menus and reading pages own their keys even with executions present.
    pub(super) fn background_key(&mut self, key: KeyEvent) -> Option<Asked> {
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        if self.background_focus.is_none() && !(ctrl && key.code == KeyCode::Char('k')) {
            return None;
        }
        if self.vim.is_none() || self.reading() || !self.menu().is_empty() {
            if self.background_focus.is_some() {
                self.leave_background();
            }
            return None;
        }
        let Some(id) = self.background_focus else {
            if self.executions.is_empty() {
                return None;
            }
            self.jobs_open = true;
            self.background_focus = self.executions.last().map(|j| j.id);
            self.disarm();
            return Some(Asked::Own(Deed::Nothing));
        };
        self.disarm();
        if (ctrl || key.modifiers.is_empty()) && matches!(key.code, KeyCode::Char('k' | 'j')) {
            let at = self.executions.iter().position(|j| j.id == id).unwrap_or(0);
            let next = if key.code == KeyCode::Char('k') {
                Some(at.saturating_sub(1))
            } else {
                at.checked_add(1)
                    .filter(|&next| next < self.executions.len())
            };
            match next.and_then(|at| self.executions.get(at)) {
                Some(execution) => self.background_focus = Some(execution.id),
                None if ctrl => self.leave_background(),
                None => {}
            }
            return Some(Asked::Own(Deed::Nothing));
        }
        let deed = if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.leave_background();
                    None
                }
                KeyCode::Enter => Some(
                    if self
                        .every_command
                        .iter()
                        .any(|j| j.id == id && j.state == State::Back)
                    {
                        Deed::ReadCommand(id)
                    } else {
                        Deed::ToggleCommand(id)
                    },
                ),
                KeyCode::Char('x') if key.modifiers.is_empty() => {
                    Some(Deed::StopCommands(vec![id]))
                }
                KeyCode::Char('r') if key.modifiers.is_empty() => {
                    Some(Deed::RestartCommands(vec![id]))
                }
                KeyCode::Char('X') => Some(Deed::StopCommands(Vec::new())),
                KeyCode::Char('x') if key.modifiers == KeyModifiers::SHIFT => {
                    Some(Deed::StopCommands(Vec::new()))
                }
                _ => None,
            }
        } else {
            None
        };
        Some(Asked::Own(deed.unwrap_or(Deed::Nothing)))
    }
}
