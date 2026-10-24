//! A turn nobody typed — a `later` come due, a background subagent's
//! answer: one line naming it, its text under the line once opened.

use ratatui::text::Line;

use super::steps::STEP_INDENT;
use super::{CODE_INDENT, Fold, Opened, clip_beside, count};
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
        1 + if self.open { self.body.len() } else { 0 }
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        if i == 0 && !self.open {
            return (self.head(self.hovered, paint, width), None);
        }
        let (page, indent) = self.opened_line(i, self.hovered && i == 0, paint);
        (page.line, indent)
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
            return (self.head(hovered, paint, usize::MAX).into(), None);
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
        };
        (page, Some(Line::from(indent)))
    }

    // What in line `i` reads as one; its own line holds nothing such.
    pub(super) fn units(&self, i: usize) -> &[std::ops::Range<usize>] {
        let body = i.checked_sub(1).and_then(|i| self.body.get(i));
        body.map_or(&[], |r| &r.units)
    }

    pub(super) fn hover(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    // Its own line: the label, how much opening it shows, when it came in.
    fn head(&self, hovered: bool, paint: &Paint, width: usize) -> Line<'static> {
        let lines = match self.body.len() {
            0 => String::new(),
            n => format!("{}{}", icons::PART_SEP, count(n, "line")),
        };
        let now = agent::session::now();
        let tail = format!("{lines}{}{}", icons::PART_SEP, render::clock(self.at, now));
        let text = clip_beside(&self.label, 2, &tail, width);
        Line::from(vec![
            paint.span_hovered(hovered, &paint.theme.prompt.color, icons::RELAYED_MARK),
            paint.span_hovered(hovered, &paint.theme.muted, format!(" {text}{tail}")),
        ])
    }
}

impl Fold for Relayed {
    fn expandable(&self) -> bool {
        !self.body.is_empty()
    }

    fn flip(&mut self) {
        self.open = !self.open;
    }

    fn opened(&self, _i: usize, paint: &Paint) -> Option<Opened> {
        if self.open || !self.expandable() {
            return None;
        }
        Some(
            (0..=self.body.len())
                .map(|i| self.opened_line(i, false, paint))
                .collect(),
        )
    }
}
