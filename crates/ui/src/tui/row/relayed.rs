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
        if self.open { 1 + self.body.len() } else { 1 }
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        if i == 0 && !self.open {
            let (line, border) = self.head(self.hovered, paint, width);
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
    // own line, then the text under it.
    fn opened_line(
        &self,
        i: usize,
        hovered: bool,
        paint: &Paint,
    ) -> (PageLine, Option<Line<'static>>) {
        let Some(j) = i.checked_sub(1) else {
            let (line, border) = self.head(hovered, paint, usize::MAX);
            return (line.into(), Some(border));
        };
        let Some(r) = self.body.get(j) else {
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
        let body = i.checked_sub(1).and_then(|i| self.body.get(i));
        body.map_or_else(Vec::new, |row| row.units.clone())
    }

    pub(super) fn hover(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    // Its own line: the label, how much opening it shows, when it came in.
    fn head(&self, hovered: bool, paint: &Paint, width: usize) -> (Line<'static>, Line<'static>) {
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
        (
            Line::from(vec![
                paint.span_hovered(hovered, &paint.theme.muted, text),
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

    fn flip(&mut self) {
        self.open = !self.open;
    }

    fn opening(&self, _i: usize, paint: &Paint) -> Option<Opening> {
        if !self.expandable() {
            return None;
        }
        Some(page_of(
            (0..=self.body.len())
                .map(|i| self.opened_line(i, false, paint))
                .collect(),
        ))
    }
}
