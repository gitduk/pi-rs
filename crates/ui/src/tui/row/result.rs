//! A tool's result row: its head and a sketch, the whole preview opened.

use std::cell::RefCell;

use ratatui::text::Line;

use super::{Fold, Opening};
use crate::render::{self, Paint};

// Kept as its parts; clipped only when a frame needs it, since a row
// clipped to its landing width could never regrow.
pub(super) struct ToolOut {
    pub(super) ok: bool,
    pub(super) name: String,
    pub(super) preview: String,
    // The row count, so expandability and toggle don't re-walk the
    // preview string on every mouse move.
    preview_lines: usize,
    expanded: bool,
    hovered: bool,
    // Caches the last clip, keyed by width: painting the whole result
    // per row made redraws quadratic in the diff's size.
    painted: RefCell<Option<(usize, Vec<Line<'static>>)>>,
}

impl ToolOut {
    pub(super) fn new(ok: bool, name: String, preview: String) -> Self {
        Self {
            ok,
            name,
            preview_lines: preview.lines().count(),
            preview,
            expanded: false,
            hovered: false,
            painted: RefCell::new(None),
        }
    }

    pub(super) fn len(&self) -> usize {
        if self.preview_lines <= render::SKETCHED_ROWS {
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
                    !self.ok,
                    &self.name,
                    &self.preview,
                    self.expanded,
                    self.hovered,
                    paint,
                    width,
                );
                &mut slot.insert((width, rows)).1
            }
        };
        rows.get(i).cloned().unwrap_or_default()
    }

    // Only a result that opens is lit under the pointer.
    pub(super) fn hover(&mut self, hovered: bool) {
        if self.expandable() && self.hovered != hovered {
            self.hovered = hovered;
            *self.painted.get_mut() = None;
        }
    }
}

impl Fold for ToolOut {
    fn expandable(&self) -> bool {
        self.preview_lines > render::SKETCHED_ROWS
    }

    fn flip(&mut self) {
        self.expanded = !self.expanded;
        *self.painted.get_mut() = None;
    }

    // The whole preview, its head as the closed row reads.
    fn opening(&self, _i: usize, paint: &Paint, width: usize) -> Option<Opening> {
        if self.expanded || !self.expandable() {
            return None;
        }
        let mut rows = render::result_rows(
            !self.ok,
            &self.name,
            &self.preview,
            true,
            false,
            paint,
            width,
        )
        .into_iter();
        Some((rows.next()?, rows.map(|line| (line, None)).collect()))
    }
}
