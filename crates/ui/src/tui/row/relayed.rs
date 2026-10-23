//! A turn nobody typed — a `later` come due, a background subagent's
//! answer: one line naming it, its text under the line once opened.

use ratatui::text::Line;

use super::steps::STEP_INDENT;
use super::{CODE_INDENT, Fold, Opening, clip_beside, count};
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
        if i == 0 {
            return (self.head(self.hovered, paint, width), None);
        }
        let Some(r) = self.body.get(i - 1) else {
            return (Line::default(), Some(Line::from(STEP_INDENT)));
        };
        let indent = if r.coded == render::Coded::Code {
            format!("{STEP_INDENT}{CODE_INDENT}")
        } else {
            STEP_INDENT.to_string()
        };
        (r.line.clone(), Some(Line::from(indent)))
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

    fn opening(&self, _i: usize, paint: &Paint, width: usize) -> Option<Opening> {
        if self.open || !self.expandable() {
            return None;
        }
        let body = self
            .body
            .iter()
            .map(|r| PageLine::from(r.clone()))
            .collect();
        Some((self.head(false, paint, width), body))
    }
}
