//! An answer's copyable rows: a code block's badge, and a mermaid block or
//! table drawn whole. A click on either copies what it holds.

use std::cell::RefCell;
use std::time::Instant;

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::block::Form;
use crate::render::{self, Paint};

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
    // At which width, and whether a badge heads it.
    painted: RefCell<Option<(usize, Line<'static>, bool)>>,
    pub(super) note: Note,
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
        let (line, badged) = self.drawn(paint, width);
        let line = bolded(line, self.note.hovered && badged);
        // Beside a table's border it could wrap and split the table.
        beside(line, self.note.said().filter(|_| badged), paint)
    }

    pub(super) fn badged(&self, paint: &Paint, width: usize) -> bool {
        self.drawn(paint, width).1
    }

    fn drawn(&self, paint: &Paint, width: usize) -> (Line<'static>, bool) {
        let mut held = self.painted.borrow_mut();
        match held.as_ref() {
            Some((at, line, badged)) if *at == width => (line.clone(), *badged),
            _ => {
                let (line, badged) = block_at(self.form, &self.source, paint, width);
                *held = Some((width, line.clone(), badged));
                (line, badged)
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
fn block_at(form: Form, source: &str, paint: &Paint, width: usize) -> (Line<'static>, bool) {
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
    let source_under = |lang: &str, note: &str| {
        let mut spans = render::badge(lang, note, paint).spans;
        spans.push(Span::raw(format!("\n{source}")));
        Line::from(spans)
    };
    match form {
        Form::Mermaid => match draw(source, Some(width)) {
            Drawn::Fits(diagram) => {
                let mut spans = render::badge("mermaid", "", paint).spans;
                spans.push(Span::raw(format!("\n{diagram}")));
                (Line::from(spans), true)
            }
            Drawn::Wide { needs, has } => (source_under("mermaid", &note(needs, has)), true),
            Drawn::Unread => (source_under("mermaid", ""), true),
        },
        Form::Table => {
            let lines = render::render_markdown(source, paint);
            let needs = lines.iter().map(Line::width).max().unwrap_or(0);
            if needs <= width {
                (joined(lines), false)
            } else {
                (source_under("table", &note(needs, width)), true)
            }
        }
    }
}
