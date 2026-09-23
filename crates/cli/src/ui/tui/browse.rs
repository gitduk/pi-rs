//! The conversation alone: what the user said and what the model answered,
//! with the thinking, the calls and the editor itself out of the way.
//!
//! A way of looking rather than a place: the scrollback underneath is the same
//! one, so nothing is rebuilt to enter or leave, and a run still streaming
//! lands its answer in it.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::view::View;
use super::{Asked, Deed, Ui};

impl Ui {
    /// Take the conversation view up, on the newest rows.
    pub(super) fn browse(&mut self, view: &mut View) {
        self.browsing = true;
        view.surface.scroll = 0;
    }

    // Back to the prompt, the way the mode opened: the rows it scrolled are
    // the conversation's, and the line is where the user left it.
    fn leave_browse(&mut self, view: &mut View) {
        self.browsing = false;
        view.surface.scroll = 0;
    }

    /// One keypress while it is up. Everything but the reading keys is
    /// swallowed: a key that types into a hidden line is worse than one that
    /// does nothing.
    pub(super) fn browse_key(&mut self, view: &mut View, key: KeyEvent) -> Asked {
        // Nothing in here arms a key outside it: the `esc` that leaves must
        // not read as half of the editor's double-tap.
        self.last_esc = None;
        // The letters are bare ones, as the panel's vocabulary is: with a
        // modifier they are the menu's keys, which is nothing on this screen.
        let bare = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let page = self.page_scroll_step();
        match key.code {
            KeyCode::Down => self.scroll_view(view, false, 1),
            KeyCode::Up => self.scroll_view(view, true, 1),
            KeyCode::PageDown => self.scroll_view(view, false, page),
            KeyCode::PageUp => self.scroll_view(view, true, page),
            KeyCode::Char('j') if bare => self.scroll_view(view, false, 1),
            KeyCode::Char('k') if bare => self.scroll_view(view, true, 1),
            KeyCode::Esc => self.leave_browse(view),
            KeyCode::Char('q' | 'v') if bare => self.leave_browse(view),
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }
}
