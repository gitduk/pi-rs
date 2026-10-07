//! Conversation-only view: user and model turns, without thinking, calls,
//! or the editor.
//!
//! Shares the scrollback with other views, so nothing rebuilds on enter/exit.

use super::Ui;
use super::ui::Focus;
use super::view::View;

impl Ui {
    pub(super) fn browsing(&self) -> bool {
        matches!(self.focus, Focus::Browse)
    }

    /// Take the conversation view up, on the newest rows.
    pub(super) fn browse(&mut self, view: &mut View) {
        self.focus = Focus::Browse;
        view.surface.scroll = 0;
    }

    // Scroll resets to 0; the prompt line itself is untouched here.
    pub(super) fn leave_browse(&mut self, view: &mut View) {
        self.focus = Focus::Editor;
        view.surface.scroll = 0;
    }
}
