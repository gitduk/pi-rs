//! The conversation alone: what the user said and what the model answered,
//! with the thinking, the calls and the editor itself out of the way.
//!
//! A way of looking rather than a place: the scrollback underneath is the same
//! one, so nothing is rebuilt to enter or leave, and a run still streaming
//! lands its answer in it.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::view::View;
use super::{Asked, Deed, Ui, screen};
use crate::store::keys;

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
        // The letters are bare ones, as a reply's vocabulary is: with a
        // modifier they are the menu's keys, which is nothing on this screen.
        let bare = keys::bare_letter(&key).is_some();
        let page = self.page_scroll_step();
        let half = self.half_scroll_step();
        // A key that is not the second `g` ends the pair it might be half of:
        // `g`, `k`, `g` is three presses, not a `gg`.
        if !matches!(key.code, KeyCode::Char('g') if bare)
            && let Some(v) = &mut self.vim
        {
            v.last = None;
        }
        match key.code {
            KeyCode::Down => self.scroll_view(view, false, 1),
            KeyCode::Up => self.scroll_view(view, true, 1),
            KeyCode::PageDown => self.scroll_view(view, false, page),
            KeyCode::PageUp => self.scroll_view(view, true, page),
            KeyCode::Char('j') if bare => self.scroll_view(view, false, 1),
            KeyCode::Char('k') if bare => self.scroll_view(view, true, 1),
            KeyCode::Char('J') if bare => self.scroll_view(view, false, half),
            KeyCode::Char('K') if bare => self.scroll_view(view, true, half),
            KeyCode::Char('G') if bare => self.scroll_view(view, false, screen::TOP),
            KeyCode::Char('g') if bare => self.doubled_g(view),
            KeyCode::Esc => self.leave_browse(view),
            KeyCode::Char('q' | 'v') if bare => self.leave_browse(view),
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // The second `g` inside the doubled-key window reaches the top of the
    // conversation; a lone one commands nothing, as it does nothing outside.
    fn doubled_g(&mut self, view: &mut View) {
        let doubled = self
            .vim
            .as_mut()
            .is_some_and(|v| v.completes('g', Instant::now()));
        if doubled {
            self.scroll_view(view, true, screen::TOP);
        }
    }
}
