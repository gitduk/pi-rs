//! A turn nobody typed — a `later` come due, a background subagent's
//! answer: one line naming it, its text under the line once opened.

use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::steps::STEP_INDENT;
use super::{CODE_INDENT, Fold, Opening, count, set_off};
use crate::render::{self, Paint};
use pi_store::icons;

pub(super) struct Relayed {
    label: String,
    // Each line, and whether it sits in a code block.
    body: Vec<(Line<'static>, bool)>,
    open: bool,
    hovered: bool,
}

impl Relayed {
    pub(super) fn new(label: &str, text: &str, paint: &Paint) -> Self {
        // A one-line text the label already says opens onto nothing new.
        let said = !text.trim().contains('\n') && label.contains(text.trim());
        Self {
            label: label.to_string(),
            body: render::render_coded(text, paint)
                .into_iter()
                .map(|(line, coded)| (line, coded == render::Coded::Code))
                .filter(|_| !said)
                .collect(),
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
        let (line, code) = self.body.get(i - 1).cloned().unwrap_or_default();
        let indent = if code {
            format!("{STEP_INDENT}{CODE_INDENT}")
        } else {
            STEP_INDENT.to_string()
        };
        (line, Some(Line::from(indent)))
    }

    pub(super) fn hover(&mut self, hovered: bool) {
        self.hovered = hovered;
    }

    // Its own line: the label, and how much opening it shows.
    fn head(&self, hovered: bool, paint: &Paint, width: usize) -> Line<'static> {
        let tail = match self.body.len() {
            0 => String::new(),
            n => format!("{}{}", icons::PART_SEP, count(n, "line")),
        };
        let room = width.saturating_sub(2 + UnicodeWidthStr::width(tail.as_str()) + 1);
        let text = pi_store::text::clip(&self.label, room.max(8));
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
            .map(|(line, code)| (set_off(line.clone(), *code), None))
            .collect();
        Some((self.head(false, paint, width), body))
    }
}
