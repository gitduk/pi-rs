//! Conversation-only view: user and model turns, without thinking, calls,
//! or the editor.
//!
//! Shares the scrollback with other views, so nothing rebuilds on enter/exit.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};

use super::view::View;
use super::{Asked, Deed, Ui, screen};
use pi_core::store::keys;

impl Ui {
    /// Take the conversation view up, on the newest rows.
    pub(super) fn browse(&mut self, view: &mut View) {
        self.browsing = true;
        view.surface.scroll = 0;
    }

    // Scroll resets to 0; the prompt line itself is untouched here.
    fn leave_browse(&mut self, view: &mut View) {
        self.browsing = false;
        view.surface.scroll = 0;
    }

    /// One keypress while browsing. Everything but scroll/exit keys is
    /// swallowed, to avoid typing into a hidden line.
    pub(super) fn browse_key(&mut self, view: &mut View, key: KeyEvent) -> Asked {
        // Reset so `esc` here isn't read as half of the editor's double-esc.
        self.last_esc = None;
        // Bare letters only; modified ones are menu keys, not used here.
        let bare = keys::bare_letter(&key).is_some();
        let page = self.page_scroll_step();
        let half = self.half_scroll_step();
        // A non-`g` press cancels a pending `gg`: `g`, `k`, `g` isn't a repeat.
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
