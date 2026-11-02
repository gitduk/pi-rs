//! Keyboard focus for the background commands above the input line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pi_core::driver::background::{BackgroundCommand, State};
use pi_store::keys::Mode;

use super::{Asked, Deed, Ui};

pub(super) const HINT: &str =
    "ctrl+k/j move · x stop · r restart · X stop all · enter output · q/esc back";

impl Ui {
    // Keep stopped rows available for restart while other commands still run.
    pub(super) fn sync_background(&mut self, all: Vec<BackgroundCommand>) {
        if !all.iter().any(|j| j.state == State::Running) {
            self.leave_background();
        }
        let at = self
            .background_focus
            .and_then(|id| self.executions.iter().position(|j| j.id == id));
        let focused = self.background_focus.is_some();
        let executions = all
            .iter()
            .filter(|j| {
                j.state == State::Running
                    || (focused && self.executions.iter().any(|old| old.id == j.id))
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
        self.every_command = all;
        self.executions = executions;
        if !self.every_command.iter().any(|j| j.state == State::Back) {
            self.background_results_shown = false;
        }
    }

    pub(super) fn leave_background(&mut self) {
        if self.background_focus.take().is_some() {
            self.executions.retain(|j| j.state == State::Running);
            self.disarm();
        }
    }

    // Menus and reading pages own their keys even with executions present.
    pub(super) fn background_key(&mut self, key: KeyEvent) -> Option<Asked> {
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        if self.background_focus.is_none() && !(ctrl && key.code == KeyCode::Char('k')) {
            return None;
        }
        if self.vim != Some(Mode::Normal) || self.reading() || !self.menu().is_empty() {
            self.leave_background();
            return None;
        }
        let Some(id) = self.background_focus else {
            self.background_focus = self.executions.last().map(|j| j.id);
            self.disarm();
            return Some(Asked::Own(Deed::Nothing));
        };
        self.disarm();
        let deed = if ctrl {
            match key.code {
                KeyCode::Char('k') | KeyCode::Char('j') => {
                    let at = self.executions.iter().position(|j| j.id == id).unwrap_or(0);
                    let next = if key.code == KeyCode::Char('k') {
                        Some(at.saturating_sub(1))
                    } else {
                        at.checked_add(1)
                            .filter(|&next| next < self.executions.len())
                    };
                    match next.and_then(|at| self.executions.get(at)) {
                        Some(execution) => self.background_focus = Some(execution.id),
                        None => self.leave_background(),
                    }
                    None
                }
                _ => None,
            }
        } else if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT {
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
