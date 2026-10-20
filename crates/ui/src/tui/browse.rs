//! The reading page: what the history region shows in the transcript's
//! place, the line still under it with the keys. It reads the conversation,
//! or lines of their own — what a command answered, or what a step opens.
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

// What a click on a line of a text page does.
pub(super) enum TextClick {
    Copy(String),
    Close,
}

pub(super) struct Read {
    pub(super) page: Page,
    // Rows the page is scrolled up by, as the transcript's `scroll`.
    pub(super) scroll: usize,
    // The row the page opens on, as the transcript's `top`.
    pub(super) top: Option<Anchor>,
    // What the page is searched for, `/` on.
    pub(super) search: Option<Box<super::search::Search>>,
    // The mode the line goes back to when the page closes, if opening it
    // moved the line to Normal.
    back: Option<Mode>,
}

pub(super) enum Page {
    // The conversation alone: no thinking, calls or notices.
    Conversation,
    // A slash command's output, lying over the bottom of the transcript it was
    // typed over.
    Slash(Vec<Line<'static>>),
    // What a closed line opens onto, standing alone: its first line is the
    // one clicked, copied out when the page opened.
    Preview(Vec<PageLine>),
}

impl Ui {
    pub(super) fn browsing(&self) -> bool {
        matches!(self.page(), Some(Page::Conversation))
    }

    pub(super) fn page(&self) -> Option<&Page> {
        match &self.read {
            Some(read) => Some(&read.page),
            _ => None,
        }
    }

    /// Take the conversation view up, on the newest rows.
    pub(super) fn browse(&mut self) {
        self.open_page(Page::Conversation, 0);
    }

    // What a click on line `n` of a preview does: copy the block it is the
    // badge of, or, on the line clicked to open it, close the page.
    pub(super) fn text_click(&self, n: usize) -> Option<TextClick> {
        let Some(Page::Preview(lines)) = self.page() else {
            return None;
        };
        match &lines.get(n)?.copies {
            Some(code) => Some(TextClick::Copy(code.clone())),
            None => (n == 0).then_some(TextClick::Close),
        }
    }

    // Read what a closed line opens onto, from its head.
    pub(super) fn preview(&mut self, lines: Vec<PageLine>) {
        self.open_page(Page::Preview(lines), super::screen::TOP);
    }

    /// Shows a command's output, replacing any prior one. Empty output opens
    /// nothing, since dismissing an empty overlay is worse than silence.
    pub(super) fn open_slash(&mut self, content: Listing) {
        if !content.is_empty() {
            let lines = super::slash::lines(&content, self.screen.usable(), &self.paint);
            // From its first line: the draw stops `TOP` at the output's head.
            self.open_page(Page::Slash(lines), super::screen::TOP);
        } else if matches!(self.page(), Some(Page::Slash(_))) {
            self.close_page();
        }
    }

    fn open_page(&mut self, page: Page, scroll: usize) {
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
        self.read = Some(Read {
            page,
            scroll,
            top: None,
            search: None,
            back,
        });
    }

    // The transcript back in the history region; false when no page was up.
    pub(super) fn close_page(&mut self) -> bool {
        let Some(read) = self.read.take() else {
            return false;
        };
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
