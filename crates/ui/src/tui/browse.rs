//! Conversation-only view: user and model turns, without thinking, calls,
//! or the editor.
//!
//! Shares the scrollback with other views, so nothing rebuilds on enter/exit.

use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::view::View;
use super::{Asked, Deed, Ui, screen};
use pi_store::keys;

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

    /// One keypress while browsing. Everything but the pager keys and `v` is
    /// swallowed, to avoid typing into a hidden line.
    pub(super) fn browse_key(&mut self, view: &mut View, key: KeyEvent) -> Asked {
        // Reset so `esc` here isn't read as half of the editor's double-esc.
        self.last_esc = None;
        let step = |ui: &Self, s: Step| match s {
            Step::Line => 1,
            Step::Half => ui.half_scroll_step(),
            Step::Page => ui.page_scroll_step(),
            Step::End => screen::TOP,
        };
        match self.pager(&key) {
            Some(Page::Up(s)) => self.scroll_view(view, true, step(self, s)),
            Some(Page::Down(s)) => self.scroll_view(view, false, step(self, s)),
            Some(Page::Close) => self.leave_browse(view),
            None if keys::bare_letter(&key) == Some('v') => self.leave_browse(view),
            None => {}
        }
        Asked::Own(Deed::Nothing)
    }

    /// What a pager key asks of a scrolled view — the conversation view and a
    /// command's reply read the same vim keys.
    pub(super) fn pager(&mut self, key: &KeyEvent) -> Option<Page> {
        let bare = keys::bare_letter(key);
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        // A non-`g` press cancels a pending `gg`: `g`, `k`, `g` isn't a repeat.
        if bare != Some('g')
            && let Some(v) = &mut self.vim
        {
            v.last = None;
        }
        Some(match (key.code, bare) {
            (KeyCode::Down, _) | (_, Some('j')) => Page::Down(Step::Line),
            (KeyCode::Up, _) | (_, Some('k')) => Page::Up(Step::Line),
            (_, Some('J')) => Page::Down(Step::Half),
            (_, Some('K')) => Page::Up(Step::Half),
            (KeyCode::Char('d'), _) if ctrl => Page::Down(Step::Half),
            (KeyCode::Char('u'), _) if ctrl => Page::Up(Step::Half),
            (KeyCode::PageDown, _) => Page::Down(Step::Page),
            (KeyCode::PageUp, _) => Page::Up(Step::Page),
            (KeyCode::Char('f'), _) if ctrl => Page::Down(Step::Page),
            (KeyCode::Char('b'), _) if ctrl => Page::Up(Step::Page),
            (_, Some('G')) => Page::Down(Step::End),
            (_, Some('g')) if self.doubled_g() => Page::Up(Step::End),
            (KeyCode::Esc, _) | (_, Some('q')) => Page::Close,
            _ => return None,
        })
    }

    fn doubled_g(&mut self) -> bool {
        self.vim
            .as_mut()
            .is_some_and(|v| v.completes('g', Instant::now()))
    }
}

/// A move through a scrolled view.
pub(super) enum Page {
    Up(Step),
    Down(Step),
    Close,
}

pub(super) enum Step {
    Line,
    Half,
    Page,
    // As far as the view goes.
    End,
}
