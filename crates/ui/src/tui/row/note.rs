//! A note — what the model is told, shown as it reads it — under a mark of
//! its own so it never reads as reasoning: its first line, the rest once
//! opened, as a relayed turn's.

use ratatui::text::{Line, Span};

use super::{Fold, Opened, clip_beside, count};
use crate::render::Paint;
use crate::tui::browse::PageLine;
use pi_store::icons;

pub(super) struct Note {
    lines: Vec<String>,
    open: bool,
    hovered: bool,
}

impl Note {
    pub(super) fn new(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_string).collect(),
            open: false,
            hovered: false,
        }
    }

    pub(super) fn len(&self) -> usize {
        match self.open {
            true => self.lines.len(),
            false => 1,
        }
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        if !self.open {
            return (
                Line::from(self.head(paint, width)),
                Some(self.lead(0, paint)),
            );
        }
        let (page, lead) = self.opened_line(i, self.hovered && i == 0, paint);
        (page.line, lead)
    }

    // The mark before the first line, its blank width before every other,
    // so a wrap hangs under the text, not the mark.
    fn lead(&self, i: usize, paint: &Paint) -> Line<'static> {
        let mark = icons::bar(icons::NOTE_MARK);
        Line::from(match i {
            0 => paint.span(&paint.theme.prompt.color, mark),
            _ => Span::raw(icons::under_bar(icons::NOTE_MARK)),
        })
    }

    // Line `i` opened, as the row draws it and a page reads it.
    fn opened_line(
        &self,
        i: usize,
        hovered: bool,
        paint: &Paint,
    ) -> (PageLine, Option<Line<'static>>) {
        let body = paint.span_hovered(hovered, &paint.theme.muted, self.lines[i].clone());
        (Line::from(body).into(), Some(self.lead(i, paint)))
    }

    // Closed, its first line cut to one row, and how much opening shows.
    fn head(&self, paint: &Paint, width: usize) -> Span<'static> {
        let (text, tail) = self.head_text(width);
        paint.span_hovered(self.hovered, &paint.theme.muted, format!("{text}{tail}"))
    }

    // The head's text, cut to fit beside its tail, and the tail.
    fn head_text(&self, width: usize) -> (String, String) {
        let tail = match self.lines.len() - 1 {
            0 => String::new(),
            n => format!("{}{}", icons::PART_SEP, count(n, "line")),
        };
        let mark = pi_store::text::width(&icons::bar(icons::NOTE_MARK));
        (clip_beside(&self.lines[0], mark, &tail, width), tail)
    }

    pub(super) fn hover(&mut self, hovered: bool) {
        self.hovered = hovered;
    }
}

impl Fold for Note {
    fn expandable(&self) -> bool {
        self.lines.len() > 1
    }

    fn flip(&mut self) {
        self.open = !self.open;
    }

    // Its first line opens and closes it, or shows it whole if only cut;
    // the rest is read.
    fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        (i == 0 && (self.expandable() || self.clipped(width))).then_some(0)
    }

    fn opened(&self, _i: usize, paint: &Paint) -> Option<Opened> {
        if self.open {
            return None;
        }
        Some(
            (0..self.lines.len())
                .map(|i| self.opened_line(i, false, paint))
                .collect(),
        )
    }

    fn clipped(&self, width: usize) -> bool {
        !self.open && self.head_text(width).0 != self.lines[0]
    }
}
