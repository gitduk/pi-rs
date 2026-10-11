//! The reading page: the transcript region alone, the editor hidden, read
//! with the pager keys. It reads the conversation, or lines of their own —
//! what a command answered, or what a step opens.
//!
//! The page keeps its own scroll, so the transcript's is where it was when
//! the page closes.

use ratatui::text::Line;

use super::Ui;
use super::ui::Focus;
use super::view::View;
use pi_store::listing::Listing;

// A line of a text page, with the code block a click on it copies when it
// is that block's badge.
pub(super) type PageLine = (Line<'static>, Option<String>);

// What a click on a line of a text page does.
pub(super) enum TextClick {
    Copy(String),
    Close,
}

pub(super) struct Read {
    pub(super) page: Page,
    // Rows the page is scrolled up by, as the transcript's `scroll`.
    pub(super) scroll: usize,
    // The rows the page measured last, as the transcript's `counted`.
    pub(super) counted: Option<usize>,
}

pub(super) enum Page {
    // The conversation alone: no thinking, calls or notices.
    Conversation,
    // A command's answer, under the transcript it was asked over.
    Reply(Vec<Line<'static>>),
    // What a closed line opens onto, standing alone: its first line is the
    // one clicked, copied out when the page opened.
    Preview(Vec<PageLine>),
}

impl Ui {
    pub(super) fn reading(&self) -> bool {
        matches!(self.focus, Focus::Read(_))
    }

    pub(super) fn browsing(&self) -> bool {
        matches!(self.page(), Some(Page::Conversation))
    }

    pub(super) fn page(&self) -> Option<&Page> {
        match &self.focus {
            Focus::Read(read) => Some(&read.page),
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
        match &lines.get(n)?.1 {
            Some(code) => Some(TextClick::Copy(code.clone())),
            None => (n == 0).then_some(TextClick::Close),
        }
    }

    // Read what a closed line opens onto, from its head.
    pub(super) fn preview(&mut self, lines: Vec<PageLine>) {
        self.open_page(Page::Preview(lines), super::screen::TOP);
    }

    /// Shows a command's reply, replacing any prior one. Empty output opens
    /// nothing, since dismissing an empty overlay is worse than silence.
    pub(super) fn open_reply(&mut self, content: Listing) {
        if !content.is_empty() {
            let lines = super::reply::lines(&content, self.screen.usable(), &self.paint);
            // From its first line: the draw stops `TOP` at the reply's head.
            self.open_page(Page::Reply(lines), super::screen::TOP);
        } else if matches!(self.page(), Some(Page::Reply(_))) {
            self.focus = Focus::Editor;
        }
    }

    fn open_page(&mut self, page: Page, scroll: usize) {
        self.focus = Focus::Read(Read {
            page,
            scroll,
            counted: None,
        });
    }

    // The scroll the wheel and the pager keys move: the page's while one
    // is up, else the transcript's.
    pub(super) fn scroll_mut<'a>(&'a mut self, view: &'a mut View) -> &'a mut usize {
        match &mut self.focus {
            Focus::Read(read) => &mut read.scroll,
            _ => &mut view.surface.scroll,
        }
    }

    // A change the scroll was already moved for: the next frame measures
    // from it rather than taking it for growth.
    pub(super) fn rebase(&mut self, view: &mut View) {
        view.surface.counted = None;
        if let Focus::Read(read) = &mut self.focus {
            read.counted = None;
        }
    }
}
