//! A tool's result row: its head and a sketch, the whole preview opened.

use std::cell::RefCell;

use ratatui::text::Line;

use super::{Fold, Opened};
use crate::render::{self, Paint};
use crate::tui::browse::PageLine;

// Kept as its parts; clipped only when a frame needs it, since a row
// clipped to its landing width could never regrow.
pub(super) struct ToolOut {
    pub(super) outcome: render::Outcome,
    pub(super) name: String,
    pub(super) preview: String,
    // Where the call ran, when not the workspace root.
    pub(super) place: Option<String>,
    pub(super) took: Option<std::time::Duration>,
    // The row count, so expandability and toggle don't re-walk the
    // preview string on every mouse move.
    preview_lines: usize,
    // The widest preview line: past the room a frame gives, it is cut.
    widest: usize,
    expanded: bool,
    hovered: bool,
    // Caches the last clip, keyed by width: painting the whole result
    // per row made redraws quadratic in the diff's size.
    painted: RefCell<Option<(usize, Vec<Line<'static>>)>>,
}

impl ToolOut {
    pub(super) fn new(outcome: render::Outcome, name: String, preview: String) -> Self {
        Self {
            outcome,
            name,
            place: None,
            took: None,
            preview_lines: preview.lines().count(),
            widest: (preview.lines())
                .map(pi_store::text::width)
                .max()
                .unwrap_or(0),
            preview,
            expanded: false,
            hovered: false,
            painted: RefCell::new(None),
        }
    }

    pub(super) fn len(&self) -> usize {
        if !render::folds(self.preview_lines) {
            self.preview_lines.max(1)
        } else if self.expanded {
            self.preview_lines + 1
        } else {
            render::SKETCHED_ROWS + 1
        }
    }

    pub(super) fn line(&self, i: usize, paint: &Paint, width: usize) -> Line<'static> {
        let mut painted = self.painted.borrow_mut();
        let rows = match &mut *painted {
            Some((w, rows)) if *w == width => rows,
            slot => {
                let rows = render::result_rows(
                    &self.finished(),
                    if self.expanded {
                        render::Shown::Open
                    } else {
                        render::Shown::Sketch
                    },
                    self.hovered,
                    paint,
                    width,
                );
                &mut slot.insert((width, rows)).1
            }
        };
        rows.get(i).cloned().unwrap_or_default()
    }

    fn finished(&self) -> render::Finished<'_> {
        render::Finished {
            outcome: self.outcome,
            name: &self.name,
            preview: &self.preview,
            place: self.place.as_deref(),
            took: self.took,
        }
    }

    // The pointer is only ever over a result that opens.
    pub(super) fn hover(&mut self, hovered: bool) {
        if self.hovered != hovered {
            self.hovered = hovered;
            *self.painted.get_mut() = None;
        }
    }
}

impl Fold for ToolOut {
    fn expandable(&self) -> bool {
        render::folds(self.preview_lines)
    }

    fn clipped(&self, width: usize) -> bool {
        self.widest > render::result_room(width)
    }

    // The head and the footer under the diff answer; the diff is for reading.
    fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        let edge = i == 0 || (self.expandable() && i + 1 == self.len());
        (edge && (self.expandable() || self.clipped(width))).then_some(0)
    }

    fn flip(&mut self) {
        self.expanded = !self.expanded;
        *self.painted.get_mut() = None;
    }

    // The whole preview, uncut: a page wraps what the row would clip.
    fn opened(&self, _i: usize, paint: &Paint) -> Option<Opened> {
        if self.expanded {
            return None;
        }
        let rows = render::result_rows(
            &self.finished(),
            render::Shown::Page,
            false,
            paint,
            usize::MAX,
        );
        let rows = rows.into_iter().map(|row| (PageLine::from(row), None));
        Some(rows.collect::<Opened>()).filter(|rows| !rows.is_empty())
    }
}
