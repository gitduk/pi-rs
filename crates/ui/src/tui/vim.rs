//! The modal keys: what a press means when the keys are not just text, and
//! where the modes are kept.

use super::view::View;
use super::{Ui, screen};
use pi_store::keys::Mode;

impl Ui {
    // Back to Insert.
    pub(super) fn leave_normal(&mut self) {
        self.set_mode(Mode::Insert);
    }

    pub(super) fn set_mode(&mut self, mode: Mode) {
        if let Some(m) = &mut self.vim {
            *m = mode;
        }
        self.show_mode();
    }

    // `S`/`cc`: clear the line and drop back to Insert, since typing follows.
    pub(super) fn change_line(&mut self) {
        self.editor.clear_line();
        self.leave_normal();
    }

    // `gg` and `G` command the line's buffer while there is a line to command;
    // with the line empty they command the history's ends instead.
    pub(super) fn buffer_ends(&mut self, view: &mut View, start: bool) {
        if self.editor.is_empty() {
            // One step past either end, which the window clamps back to it.
            self.scroll_view(view, start, screen::TOP);
        } else if start {
            self.editor.buffer_start();
        } else {
            self.editor.buffer_end();
        }
    }

    // Shows the mode via caret shape (and prompt icon, if themed) so a mode
    // that persists across lines is visible, not a silent trap.
    pub(super) fn show_mode(&mut self) {
        // Three states, not two: vim off is not "Insert", and a caret shaped
        // for a mode nobody turned on is a change to somebody else's terminal.
        let normal = self.vim.map(|m| m == Mode::Normal);
        let icon = match normal {
            Some(true) => self.paint.theme.prompt.normal.clone(),
            _ => self.paint.theme.prompt.icon.clone(),
        };
        self.prompt = Self::paint_prompt(&self.paint, &icon);
        self.editor
            .set_prompts(self.prompt.clone(), self.bang_prompt.clone());
        self.screen.cursor_shape(normal);
    }

    // Turning modal keys off drops the state (not parks it), so coming back
    // never resumes Normal without an intervening keystroke.
    pub(super) fn set_vim(&mut self, cfg: &pi_store::config::Vim) {
        // Pairs of bare characters outlive the modes: `g g` reads a reply too.
        self.pair_window = std::time::Duration::from_millis(cfg.pair_timeout_ms);
        match (&mut self.vim, cfg.enabled) {
            (slot @ None, true) => *slot = Some(Mode::Insert),
            (slot, false) => *slot = None,
            (Some(_), true) => {}
        }
        self.show_mode();
    }
}
