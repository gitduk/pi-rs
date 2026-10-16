//! A tool's result row: its head and a sketch, the whole preview opened.

use std::cell::RefCell;

use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

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
    // The widest preview line: past the room a frame gives, it is cut.
    widest: usize,
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
            widest: preview
                .lines()
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0),
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
        self.preview_lines > render::SKETCHED_ROWS
    }

    fn clipped(&self, width: usize) -> bool {
        self.widest > render::result_room(width)
    }

    fn click_line(&self, _i: usize, width: usize) -> Option<usize> {
        (self.expandable() || self.clipped(width)).then_some(0)
    }

    fn flip(&mut self) {
        self.expanded = !self.expanded;
        *self.painted.get_mut() = None;
    }

    // The whole preview, uncut: a page wraps what the row would clip.
    fn opening(&self, _i: usize, paint: &Paint, width: usize) -> Option<Opening> {
        if self.expanded || !(self.expandable() || self.clipped(width)) {
            return None;
        }
        let mut rows = render::result_rows(
            !self.ok,
            &self.name,
            &self.preview,
            render::Shown::Page,
            false,
            paint,
            usize::MAX,
        )
        .into_iter();
        Some((rows.next()?, rows.map(|line| (line, None)).collect()))
    }
}
