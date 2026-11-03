//! An answer's copyable rows: a code block's badge, and a mermaid block or
//! table drawn whole. A click on a badge copies what it holds; a table's
//! cells are selected individually.

use std::cell::RefCell;
use std::time::Instant;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::block::Form;
use crate::render::{self, Paint};
use crate::unit::Units;

// A code block's badge, holding the block's text for a click to copy.
pub(super) struct Code {
    pub(super) badge: Line<'static>,
    pub(super) code: String,
    pub(super) note: Note,
}

// A closed mermaid block or a table, drawn anew per width; one logical line
// whose `\n`s the wrap breaks.
pub(super) struct Block {
    form: Form,
    pub(super) source: String,
    // At which width: the line, whether a badge heads it, and the cells
    // that read as one, counted from where the source starts.
    painted: RefCell<Option<(usize, Box<Painted>)>>,
    pub(super) note: Note,
}

#[derive(Clone)]
struct Painted {
    line: Line<'static>,
    badged: bool,
    units: Units,
    cells: Vec<crate::unit::CellFragment>,
}

// What both say of themselves: lit under the pointer, and what a copy did.
#[derive(Default)]
pub(super) struct Note {
    hovered: bool,
    copied: Option<(String, Instant)>,
}

impl Note {
    pub(super) fn copied(&mut self, said: String) {
        self.copied = Some((said, Instant::now()));
    }

    // Lit or not; whether a stale copy note went, which moves a height.
    pub(super) fn update(&mut self, hovered: bool) -> bool {
        self.hovered = hovered;
        let stale = self
            .copied
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= crate::tui::FLASH);
        if stale {
            self.copied = None;
        }
        stale
    }

    fn said(&self) -> Option<&str> {
        self.copied.as_ref().map(|(said, _)| said.as_str())
    }
}

impl Code {
    pub(super) fn line(&self, paint: &Paint) -> Line<'static> {
        beside(
            bolded(self.badge.clone(), self.note.hovered),
            self.note.said(),
            paint,
        )
    }
}

impl Block {
    pub(super) fn new(form: Form, source: String) -> Self {
        Self {
            form,
            source,
            painted: RefCell::new(None),
            note: Note::default(),
        }
    }

    pub(super) fn line(&self, paint: &Paint, width: usize) -> Line<'static> {
        let Painted { line, badged, .. } = self.drawn(paint, width);
        let line = bolded(line, self.note.hovered && badged);
        // Beside a table's border it could wrap and split the table.
        beside(line, self.note.said().filter(|_| badged), paint)
    }

    pub(super) fn cell(
        &self,
        paint: &Paint,
        width: usize,
        col: usize,
    ) -> Option<(usize, std::sync::Arc<str>, Units)> {
        let painted = self.drawn(paint, width);
        let clicked = painted
            .cells
            .iter()
            .find(|cell| cell.range.contains(&col))?;
        let text = clicked.text.clone();
        let id = clicked.id;
        let ranges = painted
            .cells
            .iter()
            .filter(|cell| cell.id == id)
            .map(|cell| cell.range.clone())
            .collect();
        Some((id, text, ranges))
    }

    pub(super) fn cell_ranges(&self, paint: &Paint, width: usize, id: usize) -> Units {
        self.drawn(paint, width)
            .cells
            .iter()
            .filter(|cell| cell.id == id)
            .map(|cell| cell.range.clone())
            .collect()
    }

    pub(super) fn badged(&self, paint: &Paint, width: usize) -> bool {
        self.drawn(paint, width).badged
    }

    // The cells of `line` that read as one, past any badge row as drawn
    // now, whatever note stands beside it.
    pub(super) fn units(&self, paint: &Paint, width: usize) -> Units {
        let painted = self.drawn(paint, width);
        let head = if painted.badged {
            let line = crate::tui::screen::plain(&self.line(paint, width));
            pi_store::text::width(line.split('\n').next().unwrap_or_default())
        } else {
            0
        };
        painted
            .units
            .into_iter()
            .map(|u| u.start + head..u.end + head)
            .collect()
    }

    fn drawn(&self, paint: &Paint, width: usize) -> Painted {
        let mut held = self.painted.borrow_mut();
        match held.as_ref() {
            Some((at, painted)) if *at == width => painted.as_ref().clone(),
            _ => {
                let painted = block_at(self.form, &self.source, paint, width);
                *held = Some((width, Box::new(painted.clone())));
                painted
            }
        }
    }
}

// The badge a hovered row leads with, in bold: bold is no wider.
fn bolded(mut line: Line<'static>, hovered: bool) -> Line<'static> {
    if hovered && let Some(badge) = line.spans.first_mut() {
        badge.style = badge.style.add_modifier(Modifier::BOLD);
    }
    line
}

// `said`, muted, closing the first line, as a block's note does.
fn beside(mut line: Line<'static>, said: Option<&str>, paint: &Paint) -> Line<'static> {
    if let Some(said) = said {
        let end = line
            .spans
            .iter()
            .position(|s| s.content.starts_with('\n'))
            .unwrap_or(line.spans.len());
        line.spans
            .insert(end, paint.span(&paint.theme.muted, format!(" {said}")));
    }
    line
}

// Tables adapt their layout to `width`. Diagrams that need more space show
// their source under a badge explaining the width they need.
fn block_at(form: Form, source: &str, paint: &Paint, width: usize) -> Painted {
    use crate::block::{Drawn, draw, note};
    let joined = |lines: Vec<Line<'static>>| {
        let mut spans = Vec::new();
        for (at, line) in lines.into_iter().enumerate() {
            if at > 0 {
                spans.push(Span::raw("\n"));
            }
            spans.extend(line.spans);
        }
        Line::from(spans)
    };
    let under = |lang: &str, note: &str, body: Vec<Span<'static>>, units: Units| {
        let mut spans = render::badge(lang, note, paint).spans;
        spans.extend(body);
        Painted {
            line: Line::from(spans),
            badged: true,
            units,
            cells: Vec::new(),
        }
    };
    let plain = || vec![Span::raw(format!("\n{source}"))];
    match form {
        Form::Mermaid => match draw(source, Some(width)) {
            Drawn::Fits(diagram) => under(
                "mermaid",
                "",
                vec![Span::raw(format!("\n{diagram}"))],
                Vec::new(),
            ),
            Drawn::Wide { needs, has } => under("mermaid", &note(needs, has), plain(), Vec::new()),
            Drawn::Unread => under("mermaid", "", plain(), Vec::new()),
        },
        Form::Table => {
            let rendered = render::render_coded_at(source, paint, Some(width));
            let (mut units, mut offset) = (Units::new(), 0);
            let mut cells = Vec::new();
            let mut lines = Vec::with_capacity(rendered.len());
            for row in rendered {
                units.extend(
                    row.full_units()
                        .into_iter()
                        .map(|u| u.start + offset..u.end + offset),
                );
                let lead = row.prefix.as_ref().map_or(0, |p| p.first.width());
                cells.extend(row.cells.iter().cloned().map(|mut cell| {
                    cell.range = cell.range.start + offset + lead..cell.range.end + offset + lead;
                    cell
                }));
                let line = row.full_line();
                offset += pi_store::text::width(&line.to_string());
                lines.push(line);
            }
            Painted {
                line: joined(lines),
                badged: false,
                units,
                cells,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_reflows_from_its_source_after_each_resize() {
        let paint = Paint::new(true);
        let source = "| Key | Notes |\n|---|---|\n| firstlongid | A readable explanatory sentence for this row. |\n| secondlongid | Another readable explanatory sentence. |";
        let block = Block::new(Form::Table, source.to_string());
        let wide = crate::tui::screen::plain(&block.line(&paint, 100));
        assert!(wide.contains('━'));
        let narrow = crate::tui::screen::plain(&block.line(&paint, 17));
        assert!(!narrow.contains('━'));
        assert!(
            narrow.lines().all(|line| pi_store::text::width(line) <= 17),
            "{narrow:?}"
        );
        assert!(narrow.contains("firstlongid"));
        assert!(!block.badged(&paint, 17));
        assert!(!block.units(&paint, 17).is_empty());
        assert_eq!(crate::tui::screen::plain(&block.line(&paint, 100)), wide);
    }

    #[test]
    fn tables_without_outer_pipes_and_in_containers_keep_reference_links_on_redraw() {
        let paint = Paint::new(true);
        for prefix in ["", "> ", "- "] {
            let source = [
                "Name | Notes",
                "--- | ---",
                "[docs][ref] | A readable explanatory sentence.",
            ]
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let lead = if prefix == "- " && i > 0 {
                    "  "
                } else {
                    prefix
                };
                format!("{lead}{line}")
            })
            .collect::<Vec<_>>()
            .join("\n")
                + "\n\n[ref]: https://example.com";
            let rows = super::super::Row::answer(&source, &paint);
            for columns in [100, 17, 60] {
                let mut text = String::new();
                for row in &rows {
                    let (line, border) = row.line(0, &paint, columns);
                    for piece in crate::tui::screen::wrap(border.as_ref(), &line, columns) {
                        let line = piece.to_string();
                        assert!(pi_store::text::width(&line) <= columns, "{line}");
                        text.push_str(&line);
                    }
                }
                let compact: String = text
                    .chars()
                    .filter(|c| !c.is_whitespace() && *c != '│')
                    .collect();
                assert!(compact.contains("https://example.com"), "{columns}: {text}");
                assert!(!text.contains("needs "));
            }
        }
    }
}
