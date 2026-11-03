//! Content anchors survive viewport movement and width-dependent layout.

use super::search::Spot;
use super::select::{Point, Source};
use super::{Ui, View, screen};

impl Ui {
    fn selection_rows(&self, view: &View, spot: Spot, width: usize) -> (usize, Vec<String>) {
        let (line, border) = match spot {
            Spot::Row(row, line) => match view.surface.scrollback.get(row) {
                Some(row) => row.line(line, &self.paint, width),
                None => return (0, Vec::new()),
            },
            Spot::Text(line) => match self.page().and_then(|page| page.get(line)) {
                Some(line) => (line.line.clone(), None),
                None => return (0, Vec::new()),
            },
            Spot::Live(_) => {
                return (
                    0,
                    self.row_targets
                        .iter()
                        .zip(&self.drawn)
                        .filter(|(target, _)| Spot::of(**target) == Some(spot))
                        .map(|(_, text)| text.clone())
                        .collect(),
                );
            }
        };
        let lead = border.as_ref().map_or(0, |border| border.width());
        let rows = screen::wrap(border.as_ref(), &line, width)
            .iter()
            .map(|line| super::select::cols(&screen::plain(line), lead, usize::MAX))
            .collect();
        (lead, rows)
    }

    fn selection_anchor(&self, view: &View, point: Point) -> Option<(Spot, usize)> {
        let spot = Spot::of(*self.row_targets.get(point.row)?)?;
        if matches!(spot, Spot::Live(_)) {
            return None;
        }
        let (lead, rows) = self.selection_rows(view, spot, self.drawn_width);
        let row = *self.row_of.get(point.row)?;
        let before: usize = rows
            .iter()
            .take(row)
            .map(|line| pi_store::text::width(line))
            .sum();
        let width = rows.get(row).map_or(0, |line| pi_store::text::width(line));
        if width == 0 {
            return None;
        }
        Some((
            spot,
            before + point.col.saturating_sub(lead).min(width.saturating_sub(1)),
        ))
    }

    pub(super) fn pin_selection(&mut self, view: &View) {
        let Some(selection) = &self.selection else {
            return;
        };
        if selection.source.is_some() {
            return;
        }
        let source = self
            .selection_anchor(view, selection.anchor)
            .zip(self.selection_anchor(view, selection.head))
            .map(|(anchor, head)| Source::Text { anchor, head })
            .or_else(|| self.pin_rendered_selection(selection));
        if let Some(selection) = &mut self.selection {
            selection.source = source;
        }
    }

    pub(super) fn reflow_selection(&mut self, view: &View, width: usize) {
        let Some(source) = self
            .selection
            .as_ref()
            .and_then(|selection| selection.source.clone())
        else {
            return;
        };
        if let Source::Rendered {
            region,
            text,
            occurrence,
        } = &source
        {
            let (base, end) = self.selection_region(*region);
            let flat: String = self.drawn[base..end].concat();
            let range =
                flat.match_indices(text.as_str())
                    .nth(*occurrence)
                    .map(|(byte, matched)| {
                        let start = pi_store::text::width(&flat[..byte]);
                        start..start + pi_store::text::width(matched)
                    });
            let mut offset = 0;
            let mut fragments = Vec::new();
            for (row, line) in self.drawn.iter().enumerate().take(end).skip(base) {
                let next = offset + pi_store::text::width(line);
                if let Some(range) = &range {
                    let from = offset.max(range.start);
                    let to = next.min(range.end);
                    if from < to {
                        fragments.push((row, from - offset..to - offset));
                    }
                }
                offset = next;
            }
            self.place_selection(fragments);
            return;
        }
        let cell_ranges = match &source {
            Source::Cell {
                at: Spot::Row(index, _),
                id,
            } => view
                .surface
                .scrollback
                .get(*index)
                .map(|row| row.cell_ranges(&self.paint, width, *id))
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut fragments = Vec::new();
        let mut cache = std::collections::BTreeMap::new();
        for (at, (target, &row)) in self.row_targets.iter().zip(&self.row_of).enumerate() {
            let Some(spot) = Spot::of(*target) else {
                continue;
            };
            let (lead, lines) = cache
                .entry(spot)
                .or_insert_with(|| self.selection_rows(view, spot, width));
            let Some(line) = lines.get(row) else {
                continue;
            };
            let start: usize = lines
                .iter()
                .take(row)
                .map(|line| pi_store::text::width(line))
                .sum();
            let end = start + pi_store::text::width(line);
            let ranges = match &source {
                Source::Cell { at: owner, .. } if *owner == spot => cell_ranges.clone(),
                Source::Cell { .. } | Source::Rendered { .. } => Vec::new(),
                Source::Text { anchor, head } => {
                    let (first, last) = if anchor <= head {
                        (anchor, head)
                    } else {
                        (head, anchor)
                    };
                    if spot < first.0 || spot > last.0 {
                        continue;
                    }
                    let from = if spot == first.0 { first.1 } else { 0 };
                    let to = if spot == last.0 {
                        last.1.saturating_add(1)
                    } else {
                        usize::MAX
                    };
                    std::iter::once(from..to).collect()
                }
            };
            for range in ranges {
                let from = range.start.max(start);
                let to = range.end.min(end);
                if from < to {
                    fragments.push((at, from - start + *lead..to - start + *lead));
                }
            }
        }
        self.place_selection(fragments);
    }

    fn place_selection(&mut self, fragments: Vec<(usize, std::ops::Range<usize>)>) {
        if let Some(selection) = &mut self.selection {
            if let Some((row, range)) = fragments.first() {
                selection.anchor = Point {
                    row: *row,
                    col: range.start,
                };
            }
            if let Some((row, range)) = fragments.last() {
                selection.head = Point {
                    row: *row,
                    col: range.end - 1,
                };
            }
            selection.fragments = fragments;
        }
    }

    fn selection_region(&self, region: Option<usize>) -> (usize, usize) {
        let regions = self.regions.readable();
        match region {
            Some(i) => {
                let base = regions[i].1.min(self.drawn.len());
                let end = regions
                    .get(i + 1)
                    .map_or(self.drawn.len(), |(_, base)| *base)
                    .min(self.drawn.len());
                (base, end.max(base))
            }
            None => (0, self.drawn.len()),
        }
    }

    fn pin_rendered_selection(&self, selection: &super::select::Selection) -> Option<Source> {
        let (first, last) = if selection.anchor <= selection.head {
            (selection.anchor, selection.head)
        } else {
            (selection.head, selection.anchor)
        };
        let regions = self.regions.readable();
        let owner = |row| (0..regions.len()).rev().find(|&i| row >= regions[i].1);
        let region = owner(first.row).filter(|i| Some(*i) == owner(last.row));
        let (base, end) = self.selection_region(region);
        let flat = self.drawn[base..end].concat();
        let position = |point: Point, inclusive: bool| {
            let before: usize = self
                .drawn
                .iter()
                .take(point.row)
                .skip(base)
                .map(|line| pi_store::text::width(line))
                .sum();
            let width = self
                .drawn
                .get(point.row)
                .map_or(0, |line| pi_store::text::width(line));
            before + point.col.saturating_add(usize::from(inclusive)).min(width)
        };
        let start = pi_store::text::byte_at_col(&flat, position(first, false));
        let end = pi_store::text::byte_at_col(&flat, position(last, true));
        let text = flat.get(start..end)?.to_string();
        if text.is_empty() {
            return None;
        }
        let occurrence = flat[..start].match_indices(text.as_str()).count();
        Some(Source::Rendered {
            region,
            text,
            occurrence,
        })
    }
}
