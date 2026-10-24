//! The reading page: lines of its own over the transcript's bottom rows,
//! under a rule, the line still under it with the keys. It reads what a
//! command answered, or what a click opens.
//!
//! The page keeps its own scroll, so the transcript's is where it was when
//! the page closes.

use ratatui::text::Line;

use super::Ui;
use super::search::Anchor;
use super::view::View;
use pi_store::keys::Mode;
use pi_store::listing::Listing;

// A line of a text page: the code block a click on it copies when it is
// that block's badge, and what in it a double click takes whole.
pub(super) struct PageLine {
    pub(super) line: Line<'static>,
    pub(super) copies: Option<String>,
    pub(super) units: crate::unit::Units,
}

impl From<Line<'static>> for PageLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            copies: None,
            units: Vec::new(),
        }
    }
}

pub(super) struct Read {
    pub(super) page: Vec<PageLine>,
    // Rows the page is scrolled up by, as the transcript's `scroll`.
    pub(super) scroll: usize,
    // The row the page opens on, as the transcript's `top`.
    pub(super) top: Option<Anchor>,
    // The mode the line goes back to when the page closes, if opening it
    // moved the line to Normal.
    back: Option<Mode>,
}

impl Ui {
    pub(super) fn page(&self) -> Option<&[PageLine]> {
        self.read.as_ref().map(|read| read.page.as_slice())
    }

    // The code block a click on line `n` of the page copies, when it is
    // that block's badge.
    pub(super) fn page_copies(&self, n: usize) -> Option<String> {
        self.page()?.get(n)?.copies.clone()
    }

    /// Shows a command's answer, replacing any page up. An empty answer
    /// opens nothing, since dismissing an empty page is worse than silence,
    /// and closes what was up: the command moved on from it.
    pub(super) fn open_answer(&mut self, content: Listing) {
        if content.is_empty() {
            self.close_page();
            return;
        }
        let lines = super::slash::lines(&content, self.screen.width(), &self.paint);
        self.open_page(lines.into_iter().map(PageLine::from).collect());
    }

    // Read `page` in the history region, from its first line.
    pub(super) fn open_page(&mut self, page: Vec<PageLine>) {
        // Over an empty line in Insert, Normal: its letters move the page.
        // A page replacing a page keeps the mode the first came from.
        let back = match self.read.take() {
            Some(read) => read.back,
            None if self.editor.is_empty() && self.vim == Some(Mode::Insert) => {
                self.set_mode(Mode::Normal);
                Some(Mode::Insert)
            }
            None => None,
        };
        // A search goes with what it was searching.
        self.search = None;
        self.read = Some(Read {
            page,
            // The draw stops `TOP` at the page's head.
            scroll: super::screen::TOP,
            top: None,
            back,
        });
    }

    // The transcript back in the history region; false when no page was up.
    pub(super) fn close_page(&mut self) -> bool {
        let Some(read) = self.read.take() else {
            return false;
        };
        self.search = None;
        if let Some(mode) = read.back {
            self.set_mode(mode);
        }
        true
    }

    // The scroll and top row of what is up: the page's, else the transcript's.
    pub(super) fn window_mut<'a>(
        &'a mut self,
        view: &'a mut View,
    ) -> (&'a mut usize, &'a mut Option<Anchor>) {
        match &mut self.read {
            Some(read) => (&mut read.scroll, &mut read.top),
            _ => (&mut view.surface.scroll, &mut view.surface.top),
        }
    }

    // Keep the top row where it is through a change under it, even on the
    // newest rows, which would otherwise follow the bottom.
    pub(super) fn pin(&mut self, view: &mut View) {
        let here = self.anchor_here(view);
        *self.window_mut(view).1 = here;
    }
}
