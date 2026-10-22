//! A note — what the model is told, shown as it reads it — under a mark of
//! its own so it never reads as reasoning; a long one folds as a result does.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{Fold, Opening};
use crate::render::{self, Paint};
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

    fn shown(&self) -> render::Shown {
        match self.open {
            true => render::Shown::Open,
            false => render::Shown::Sketch,
        }
    }

    // The lines under the first that show, and the footer under them.
    fn under(&self) -> (&[String], Option<String>) {
        render::sketched(self.lines.get(1..).unwrap_or_default(), self.shown())
    }

    pub(super) fn len(&self) -> usize {
        let (under, footer) = self.under();
        1 + under.len() + usize::from(footer.is_some())
    }

    // The mark leads the first line, its blank width every other, so a wrap
    // hangs under the text, not the mark.
    pub(super) fn line(&self, i: usize, paint: &Paint) -> (Line<'static>, Option<Line<'static>>) {
        let mark = icons::bar(icons::NOTE_MARK);
        let lead = match i {
            0 => paint.span(&paint.theme.prompt.color, mark),
            _ => Span::raw(" ".repeat(mark.width())),
        };
        let (under, footer) = self.under();
        let body = match i {
            0 => paint.span_hovered(self.hovered, &paint.theme.muted, self.lines[0].clone()),
            i if i <= under.len() => paint.span(&paint.theme.muted, under[i - 1].clone()),
            _ => paint.span_hovered(self.hovered, &paint.theme.muted, footer.unwrap_or_default()),
        };
        (Line::from(body), Some(Line::from(lead)))
    }

    pub(super) fn hover(&mut self, hovered: bool) {
        self.hovered = hovered;
    }
}

impl Fold for Note {
    fn expandable(&self) -> bool {
        render::folds(self.lines.len())
    }

    fn flip(&mut self) {
        self.open = !self.open;
    }

    // As a result's: its first line and the footer open it; the rest is read.
    fn click_line(&self, i: usize, _width: usize) -> Option<usize> {
        (self.expandable() && (i == 0 || i + 1 == self.len())).then_some(0)
    }

    fn opening(&self, _i: usize, paint: &Paint, _width: usize) -> Option<Opening> {
        if self.open || !self.expandable() {
            return None;
        }
        let head = Line::from(vec![
            paint.span(&paint.theme.prompt.color, icons::bar(icons::NOTE_MARK)),
            paint.span(&paint.theme.muted, self.lines[0].clone()),
        ]);
        let body = (self.lines[1..].iter())
            .map(|l| PageLine::from(Line::from(paint.span(&paint.theme.muted, l.clone()))))
            .collect();
        Some((head, body))
    }
}
