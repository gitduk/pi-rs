//! A turn nobody typed, carrying background command results: one line naming it,
//! its text under the line once opened.

use ratatui::text::Line;

use super::steps::STEP_INDENT;
use super::{CODE_INDENT, Fold, Opening, clip_beside, count, page_of};
use crate::render::{self, Paint};
use crate::tui::browse::PageLine;
use pi_store::icons;

pub(super) struct Relayed {
    label: String,
    // When it came in, in wall-clock seconds.
    at: u64,
    body: Vec<render::Rendered>,
    open: bool,
    hovered: bool,
}

impl Relayed {
    pub(super) fn new(label: &str, text: &str, at: u64, paint: &Paint) -> Self {
        // A one-line text the label already says opens onto nothing new.
        let said = !text.trim().contains('\n') && label.contains(text.trim());
        Self {
            label: label.to_string(),
            at,
            body: if said {
                Vec::new()
            } else {
                render::render_coded(text, paint)
            },
            open: false,
            hovered: false,
        }
    }

    pub(super) fn len(&self) -> usize {
        match self.open {
            true => self.opened_len(),
            false => 1,
        }
    }

    // The rows it takes once opened: its own line, the blank under it, and
    // the text.
    fn opened_len(&self) -> usize {
        2 + self.body.len()
    }

    // The row of its text window line `i` reads, past its own line and the
    // blank that sets the text off it.
    fn body_at(&self, i: usize) -> Option<&render::Rendered> {
        self.body.get(i.checked_sub(2)?)
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        if i == 0 && !self.open {
            let (line, border) = self.head(self.hovered, paint, width, false);
            return (line, Some(border));
        }
        let (mut page, indent) = self.opened_line(i, self.hovered && i == 0, paint);
        page.prepend(render::Prefix::hanging(indent.unwrap_or_default()));
        let first = page.prefix.as_ref().map(|p| p.first.clone());
        if let Some(prefix) = &page.prefix {
            page.line = prefix.fit_body(page.line, width);
        }
        (page.line, first)
    }

    // Line `i` opened, whole, as the row draws it and a page reads it: its
    // own line, a blank, then the text under it.
    fn opened_line(
        &self,
        i: usize,
        hovered: bool,
        paint: &Paint,
    ) -> (PageLine, Option<Line<'static>>) {
        if i == 0 {
            let (line, border) = self.head(hovered, paint, usize::MAX, true);
            return (line.into(), Some(border));
        }
        let Some(r) = self.body_at(i) else {
            return (Line::default().into(), Some(Line::from(STEP_INDENT)));
        };
        let indent = if r.coded == render::Coded::Code {
            format!("{STEP_INDENT}{CODE_INDENT}")
        } else {
            STEP_INDENT.to_string()
        };
        // Not `PageLine::from`: a code line's indent rides in the border here,
        // so a wrap keeps it.
        let page = PageLine {
            line: r.line.clone(),
            copies: match &r.coded {
                render::Coded::Badge(code) => Some(code.clone()),
                _ => None,
            },
            units: r.units.clone(),
            prefix: r.prefix.clone().map(Box::new),
        };
        (page, Some(Line::from(indent)))
    }

    // What in line `i` reads as one; its own line holds nothing such.
    pub(super) fn units(&self, i: usize) -> crate::unit::Units {
        self.body_at(i)
            .map_or_else(Vec::new, |row| row.units.clone())
    }

    pub(super) fn hover(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    // Its own line: the label, how much opening it shows, when it came in.
    // Shut, it is the aside it is; opened, it reads as the message it holds.
    fn head(
        &self,
        hovered: bool,
        paint: &Paint,
        width: usize,
        open: bool,
    ) -> (Line<'static>, Line<'static>) {
        let lines = match self.body.len() {
            0 => String::new(),
            n => format!("{}{}", icons::PART_SEP, count(n, "line")),
        };
        let now = agent::session::now();
        let time = format!("{}{}", icons::PART_SEP, render::clock(self.at, now));
        let tail = format!("{lines}{time}");
        let border = Line::from(vec![
            paint.span_hovered(hovered, &paint.theme.prompt.color, icons::RELAYED_MARK),
            " ".into(),
        ]);
        let text = clip_beside(&self.label, border.width(), &tail, width);
        let said = match open {
            true => &paint.theme.input,
            false => &paint.theme.muted,
        };
        (
            Line::from(vec![
                paint.span_hovered(hovered, said, text),
                paint.action_hint(hovered, lines),
                paint.span_hovered(hovered, &paint.theme.muted, time),
            ]),
            border,
        )
    }
}

impl Fold for Relayed {
    fn continuation(&self, i: usize, paint: &Paint, _width: usize) -> Option<Line<'static>> {
        let (mut page, border) = self.opened_line(i, false, paint);
        page.prepend(render::Prefix::hanging(border.unwrap_or_default()));
        page.prefix.map(|p| p.rest)
    }

    fn more_border(&self, _heading: usize, _paint: &Paint, _width: usize) -> Option<Line<'static>> {
        Some(Line::from(STEP_INDENT))
    }

    fn is_open(&self) -> bool {
        self.open
    }

    fn click_line(&self, i: usize, _width: usize) -> Option<usize> {
        (i == 0 && self.expandable()).then_some(0)
    }

    fn expandable(&self) -> bool {
        !self.body.is_empty()
    }

    fn opened_rows(&self) -> usize {
        self.opened_len()
    }

    // A turn nobody typed reads whole on a page, never cut with a count to
    // click through.
    fn opens_on_a_page(&self) -> bool {
        true
    }

    // Shut, its line is cut to one row; opened, it wraps for as many as it
    // takes, and that is what the budget is measured against.
    fn opened_head(&self, paint: &Paint) -> Option<(Line<'static>, Option<Line<'static>>)> {
        let (line, border) = self.head(false, paint, usize::MAX, true);
        Some((line, Some(border)))
    }

    fn flip(&mut self) {
        self.open = !self.open;
    }

    fn opening(&self, _i: usize, paint: &Paint) -> Option<Opening> {
        if !self.expandable() {
            return None;
        }
        Some(page_of(
            (0..self.opened_len())
                .map(|i| self.opened_line(i, false, paint))
                .collect(),
        ))
    }
}
