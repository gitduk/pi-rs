//! The reading page: lines of its own over the transcript's bottom rows,
//! under a rule, the line still under it with the keys. It reads what a
//! command answered, or what a click opens.
//!
//! The page keeps its own scroll, so the transcript's is where it was when
//! the page closes.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;

use super::Ui;
use super::mouse::Regions;
use super::view::{Scroll, View};
use pi_core::core::Core;
use pi_store::keys::Mode;
use pi_store::listing::Listing;

pub(super) const MORE: &str = " · more";

pub(super) enum PageInput {
    Empty,
    Editing,
}

/// An overlay over the normal frame, with its own reading window.
pub(super) struct PageLayout {
    pub(super) surface: Rect,
    pub(super) rule: Rect,
    pub(super) body: Rect,
    pub(super) max_scroll: usize,
    covers_editor: bool,
}

impl PageLayout {
    pub(super) fn of(base: Regions, rows: usize, input: PageInput, top_line: bool) -> Self {
        // A page may cover the input line only where no top layout line stands
        // between it and the transcript.
        let covers_editor = matches!(input, PageInput::Empty) && !top_line;
        let bottom = if covers_editor {
            base.editor.bottom()
        } else {
            base.statusline_top.y
        };
        let height = bottom.saturating_sub(base.history.y);
        let surface = Rect {
            height,
            ..base.history
        };
        let rule_height = surface.height.min(1);
        let body_height = rows.min((surface.height - rule_height) as usize) as u16;
        let body = Rect {
            y: surface.bottom() - body_height,
            height: body_height,
            ..surface
        };
        Self {
            surface,
            rule: Rect {
                y: body.y - rule_height,
                height: rule_height,
                ..surface
            },
            body,
            max_scroll: rows.saturating_sub(body_height as usize),
            covers_editor,
        }
    }

    /// Visible hit regions after the page covers the base frame.
    pub(super) fn regions(&self, mut base: Regions) -> Regions {
        base.history = self.surface;
        base.gap = Rect::default();
        base.executions.height = 0;
        if self.covers_editor {
            base.editor.height = 0;
        }
        base
    }
}

#[derive(Clone, Copy)]
pub(super) struct MoreAreas {
    pub(super) body: Rect,
    pub(super) action: Rect,
}

impl MoreAreas {
    pub(super) fn of(area: Rect) -> Option<Self> {
        let wanted = Line::raw(MORE).width() as u16;
        let [body, action] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(wanted)]).areas(area);
        (action.width == wanted).then_some(Self { body, action })
    }
}

// The action is rendered separately by the frame, in the same layout.
pub(super) fn more_body(line: Line<'static>, width: usize) -> Line<'static> {
    let area = Rect::new(0, 0, width as u16, 1);
    let body = MoreAreas::of(area).map_or(area, |areas| areas.body);
    super::screen::clip_line(&line, body.width as usize)
}

// A line of a text page: the code block a click on it copies when it is
// that block's badge, and what in it a double click takes whole.
pub(super) struct PageLine {
    pub(super) line: Line<'static>,
    pub(super) copies: Option<String>,
    pub(super) units: crate::unit::Units,
    pub(super) prefix: Option<Box<crate::render::Prefix>>,
}

impl From<Line<'static>> for PageLine {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            copies: None,
            units: Vec::new(),
            prefix: None,
        }
    }
}

impl PageLine {
    pub(super) fn prepend(&mut self, mut prefix: crate::render::Prefix) {
        if let Some(inside) = self.prefix.take() {
            prefix.first.spans.extend(inside.first.spans);
            prefix.rest.spans.extend(inside.rest.spans);
            prefix.rule = inside.rule;
        }
        self.prefix = Some(Box::new(prefix));
    }

    pub(super) fn ready(&self, width: usize) -> super::screen::Ready<'static> {
        let line = self.prefix.as_ref().map_or_else(
            || self.line.clone(),
            |p| p.fit_body(self.line.clone(), width),
        );
        super::screen::Ready {
            line,
            width,
            prefix: self.prefix.clone(),
        }
    }
}

// What a page was laid out from, when it can be laid out again: the width
// changes, or the list it came from does.
pub(super) enum Source {
    // The model list, which a reload can add to.
    Models(Listing),
    // A command's report, whose rows never change once answered.
    Report(Listing),
}

impl Source {
    fn listing(&self) -> &Listing {
        match self {
            Self::Models(listing) | Self::Report(listing) => listing,
        }
    }
}

pub(super) struct Read {
    pub(super) page: Vec<PageLine>,
    pub(super) source: Option<Source>,
    // Where the page is scrolled to, as the transcript has its own.
    pub(super) scroll: Scroll,
    // The mode the line goes back to when the page closes, if opening it
    // moved the line to Normal.
    back: Option<Mode>,
}

impl Ui {
    // Columns land on other rows, so the page has none to stay on: it reads
    // from its head, as one just opened does.
    fn relayout(&mut self, source: Source, view: &mut View) {
        let lines = super::slash::lines(source.listing(), self.screen.width(), &self.paint);
        self.search_close(view);
        self.selection = None;
        if let Some(read) = &mut self.read {
            read.page = lines.into_iter().map(PageLine::from).collect();
            read.scroll = Scroll {
                by: super::screen::TOP,
                top: None,
            };
            read.source = Some(source);
        }
    }

    pub(super) fn reflow_page(&mut self, view: &mut View) {
        let Some(source) = self.read.as_mut().and_then(|read| read.source.take()) else {
            return;
        };
        self.relayout(source, view);
    }

    pub(super) fn refresh_model_page(&mut self, core: &Core, view: &mut View) {
        let Some(Source::Models(previous)) =
            self.read.as_ref().and_then(|read| read.source.as_ref())
        else {
            return;
        };
        let models = core.model_listing();
        if previous == &models {
            return;
        }
        self.relayout(Source::Models(models), view);
    }

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
        if content.is_report()
            && let Some(read) = &mut self.read
        {
            read.source = Some(Source::Report(content));
        }
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
            source: None,
            // The draw stops `TOP` at the page's head.
            scroll: Scroll {
                by: super::screen::TOP,
                top: None,
            },
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
    pub(super) fn window_mut<'a>(&'a mut self, view: &'a mut View) -> &'a mut Scroll {
        match &mut self.read {
            Some(read) => &mut read.scroll,
            _ => &mut view.surface.scroll,
        }
    }

    // Keep the top row where it is through a change under it, even on the
    // newest rows, which would otherwise follow the bottom.
    pub(super) fn pin(&mut self, view: &mut View) {
        let here = self.anchor_here(view);
        self.window_mut(view).top = here;
    }
}
