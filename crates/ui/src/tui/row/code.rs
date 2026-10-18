//! An answer's copyable rows: a code block's badge, and a mermaid block or
//! table drawn whole. A click on either copies what it holds.

use std::cell::RefCell;
use std::time::Instant;

use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::{Line, Span};

use crate::block::{self, Form};
use crate::render::{self, Paint};
use crate::sgr::style_to_ratatui;
use crate::unit::Units;

// A code block's badge, holding the block's text for a click to copy.
pub(super) struct Code {
    pub(super) badge: Line<'static>,
    pub(super) code: String,
    pub(super) note: Note,
}

// A closed mermaid block or a table, drawn anew per width (its source when
// it no longer fits); one logical line whose `\n`s the wrap breaks.
pub(super) struct Block {
    form: Form,
    pub(super) source: String,
    // At which width: the line, whether a badge heads it, and the cells
    // that read as one, counted from where the source starts.
    painted: RefCell<Option<(usize, Painted)>>,
    pub(super) note: Note,
}

#[derive(Clone)]
struct Painted {
    line: Line<'static>,
    badged: bool,
    units: Units,
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

    pub(super) fn badged(&self, paint: &Paint, width: usize) -> bool {
        self.drawn(paint, width).badged
    }

    // The cells of `line` that read as one: past the badge row as drawn
    // now, whatever note stands beside it.
    pub(super) fn units(&self, paint: &Paint, width: usize) -> Units {
        let line = crate::tui::screen::plain(&self.line(paint, width));
        let head = pi_store::text::width(line.split('\n').next().unwrap_or_default());
        let units = self.drawn(paint, width).units;
        units
            .into_iter()
            .map(|u| u.start + head..u.end + head)
            .collect()
    }

    fn drawn(&self, paint: &Paint, width: usize) -> Painted {
        let mut held = self.painted.borrow_mut();
        match held.as_ref() {
            Some((at, painted)) if *at == width => painted.clone(),
            _ => {
                let painted = block_at(self.form, &self.source, paint, width);
                *held = Some((width, painted.clone()));
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

// A block row at `width`: drawn when it fits, else its source under a badge
// saying what it would need. Whether a badge heads it: a drawn table has none.
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
            let lines = render::render_markdown(source, paint);
            let needs = lines.iter().map(Line::width).max().unwrap_or(0);
            if needs <= width {
                return Painted {
                    line: joined(lines),
                    badged: false,
                    units: Vec::new(),
                };
            }
            let (body, units) = table_source(source, paint);
            under("table", &note(needs, width), body, units)
        }
    }
}

// A table as its source, read across a wrap: each column's text in a colour
// of its own, the pipes and the delimiter row muted, the header bold.
fn table_source(source: &str, paint: &Paint) -> (Vec<Span<'static>>, Units) {
    let palette: Vec<RStyle> = paint.theme.table.iter().map(style_to_ratatui).collect();
    let muted = |text: &str| paint.span(&paint.theme.muted, text);
    let (mut spans, mut units, mut col) = (Vec::new(), Units::new(), 0);
    for (i, row) in source.lines().enumerate() {
        spans.push(Span::raw("\n"));
        // Only the second line: a body row of dashes is cells like any other.
        if i == 1 {
            spans.push(muted(row));
            col += pi_store::text::width(row);
            continue;
        }
        let mut from = 0;
        for (k, cell) in block::cells(row).into_iter().enumerate() {
            let gap = &row[from..cell.start];
            spans.push(muted(gap));
            col += pi_store::text::width(gap);
            let text = &row[cell.clone()];
            let mut style = palette
                .get(k % palette.len().max(1))
                .copied()
                .unwrap_or_default();
            if i == 0 {
                style = style.add_modifier(Modifier::BOLD);
            }
            let w = pi_store::text::width(text);
            if w > 0 {
                spans.push(Span::styled(text.to_string(), style));
                units.push(col..col + w);
            }
            col += w;
            from = cell.end;
        }
        spans.push(muted(&row[from..]));
        col += pi_store::text::width(&row[from..]);
    }
    spans.retain(|s| !s.content.is_empty());
    (spans, units)
}
