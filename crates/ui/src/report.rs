//! Width-aware report typography, tables and directory columns.

use pi_store::listing::{Listing, Role, Row};
use pi_store::text;
use ratatui::text::{Line, Span};

use crate::{render::Paint, wrap};

// A numeric column is at least this wide, and set off by this many spaces.
const NUMERIC_WIDTH: usize = 6;
const NUMERIC_GAP: usize = 2;
// The width a table needs before it draws numeric columns beside its names.
const WIDE_WIDTH: usize = 28;
// A name column of the wide width, both numeric columns and their gaps.
const MIN_COLUMN_WIDTH: usize = WIDE_WIDTH + 2 * (NUMERIC_WIDTH + NUMERIC_GAP);
const COLUMN_GAP: usize = 4;

#[derive(Clone, Copy)]
enum Tone {
    Plain,
    Muted,
    Heading,
    Value,
}

fn span(paint: Option<&Paint>, tone: Tone, value: impl Into<String>) -> Span<'static> {
    let value = value.into();
    match (paint, tone) {
        (Some(paint), Tone::Muted) => paint.span(&paint.theme.muted, value),
        (Some(paint), Tone::Heading) => paint.span(&paint.theme.heading, value),
        (Some(paint), Tone::Value) => paint.span(&paint.theme.strong, value),
        _ => Span::raw(value),
    }
}

fn cell(row: &Row, index: usize) -> &str {
    row.cells.get(index).map_or("", String::as_str)
}

fn blank(row: &Row) -> bool {
    row.role == Role::Plain && row.cells.iter().all(|cell| cell.trim().is_empty())
}

pub(crate) fn lines(report: &Listing, width: usize, paint: Option<&Paint>) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let body_width = width.saturating_sub(if width >= 16 { 4 } else { 0 });
    let margin = (width - body_width) / 2;
    let mut lead = Vec::new();
    let mut sections: Vec<Vec<&Row>> = Vec::new();
    let mut footer = Vec::new();
    for row in &report.rows {
        match row.role {
            Role::Section => sections.push(vec![row]),
            Role::Footer => footer.push(row),
            _ => match sections.last_mut() {
                Some(section) => section.push(row),
                None => lead.push(row),
            },
        }
    }
    let mut out = render(&lead, body_width, paint);
    if let Some(summary) = sections.first() {
        out.push(Line::default());
        out.extend(render(summary, body_width, paint));
    }
    let directories = sections.get(1..).unwrap_or_default();
    let columns = ((body_width + COLUMN_GAP) / (MIN_COLUMN_WIDTH + COLUMN_GAP))
        .min(directories.len())
        .max(1);
    let column_width = (body_width - COLUMN_GAP * (columns - 1)) / columns;
    for group in directories.chunks(columns) {
        out.push(Line::default());
        let mut rendered: Vec<_> = group
            .iter()
            .map(|section| render(section, column_width, paint).into_iter())
            .collect();
        while rendered.iter().any(|lines| lines.len() > 0) {
            let mut line = Line::default();
            let mut used = 0;
            for (column, lines) in rendered.iter_mut().enumerate() {
                if let Some(other) = lines.next() {
                    let start = column * (column_width + COLUMN_GAP);
                    let pad = start.saturating_sub(used);
                    used = start + other.width();
                    line.spans.push(Span::raw(" ".repeat(pad)));
                    line.spans.extend(other.spans);
                }
            }
            out.push(line);
        }
    }
    if !footer.is_empty() {
        out.push(Line::default());
        out.extend(render(&footer, body_width, paint));
    }
    for line in &mut out {
        line.spans.insert(0, Span::raw(" ".repeat(margin)));
    }
    out
}

struct Columns {
    name: usize,
    tokens: usize,
    share: usize,
}

impl Columns {
    fn of(rows: &[&Row], width: usize) -> Self {
        let tokens = rows
            .iter()
            .filter(|row| row.role == Role::Budget)
            .map(|row| text::width(cell(row, 1)))
            .max()
            .unwrap_or(0)
            .max(NUMERIC_WIDTH);
        let share = rows
            .iter()
            .filter(|row| row.role == Role::Budget)
            .map(|row| text::width(cell(row, 2)))
            .max()
            .unwrap_or(0)
            .max(NUMERIC_WIDTH);
        Self {
            name: width.saturating_sub(tokens + share + 2 * NUMERIC_GAP),
            tokens,
            share,
        }
    }

    fn values(
        &self,
        spans: &mut Vec<Span<'static>>,
        tokens: &str,
        share: &str,
        paint: Option<&Paint>,
        headings: bool,
    ) {
        let gap = " ".repeat(NUMERIC_GAP);
        spans.push(Span::raw(format!(
            "{gap}{}",
            " ".repeat(self.tokens.saturating_sub(text::width(tokens)))
        )));
        spans.push(span(
            paint,
            if headings { Tone::Muted } else { Tone::Plain },
            tokens,
        ));
        spans.push(Span::raw(format!(
            "{gap}{}",
            " ".repeat(self.share.saturating_sub(text::width(share)))
        )));
        spans.push(span(paint, Tone::Muted, share));
    }
}

fn label(row: &Row, width: usize, paint: Option<&Paint>, heading: bool) -> Vec<Span<'static>> {
    let name = if heading {
        cell(row, 0).to_uppercase()
    } else {
        cell(row, 0).to_string()
    };
    let mut spans = vec![span(
        paint,
        if heading { Tone::Heading } else { Tone::Plain },
        &name,
    )];
    let mut used = text::width(&name);
    if let Some(note) = &row.note
        && width > used + 3
    {
        let note = text::clip(note, width - used - 2);
        used += 2 + text::width(&note);
        spans.push(span(paint, Tone::Muted, format!("  {note}")));
    }
    let gap = width.saturating_sub(used);
    spans.push(span(
        paint,
        Tone::Muted,
        if heading && gap > 2 {
            format!("  {}", "─".repeat(gap - 2))
        } else {
            " ".repeat(gap)
        },
    ));
    spans
}

fn render(rows: &[&Row], width: usize, paint: Option<&Paint>) -> Vec<Line<'static>> {
    let end = rows
        .iter()
        .rposition(|row| !blank(row))
        .map_or(0, |last| last + 1);
    let columns = Columns::of(&rows[..end], width);
    let mut out = Vec::new();
    for row in &rows[..end] {
        let line = match row.role {
            Role::Title => {
                let title = cell(row, 0).to_uppercase();
                let note = text::clip(
                    row.note.as_deref().unwrap_or_default(),
                    width.saturating_sub(text::width(&title) + 2),
                );
                Line::from(vec![
                    span(paint, Tone::Heading, &title),
                    Span::raw(
                        " ".repeat(width.saturating_sub(text::width(&title) + text::width(&note))),
                    ),
                    span(paint, Tone::Muted, note),
                ])
            }
            Role::Metric => {
                let mut spans = vec![
                    span(paint, Tone::Muted, format!("{}  ", cell(row, 0))),
                    span(paint, Tone::Value, cell(row, 1)),
                    span(paint, Tone::Muted, format!(" {}", cell(row, 2))),
                ];
                let used: usize = spans.iter().map(Span::width).sum();
                let ratio = cell(row, 3);
                if !ratio.is_empty() {
                    spans.push(Span::raw(
                        " ".repeat(width.saturating_sub(used + text::width(ratio)).max(2)),
                    ));
                    spans.push(span(paint, Tone::Value, ratio));
                }
                Line::from(spans)
            }
            Role::Progress { used, total } => {
                let filled = used
                    .min(total)
                    .saturating_mul(width)
                    .saturating_add(total / 2)
                    .checked_div(total)
                    .unwrap_or(0);
                Line::from(vec![
                    span(paint, Tone::Heading, "━".repeat(filled)),
                    span(paint, Tone::Muted, "─".repeat(width - filled)),
                ])
            }
            Role::Section if width >= WIDE_WIDTH => {
                let mut spans = label(row, columns.name, paint, true);
                columns.values(&mut spans, "TOKENS", "WINDOW", paint, true);
                Line::from(spans)
            }
            Role::Budget if width >= WIDE_WIDTH => {
                let mut spans = if text::width(cell(row, 0)) > columns.name {
                    let name = Line::from(span(paint, Tone::Plain, cell(row, 0)));
                    out.extend(wrap::fit(&name, width));
                    vec![Span::raw(" ".repeat(columns.name))]
                } else {
                    label(row, columns.name, paint, false)
                };
                columns.values(&mut spans, cell(row, 1), cell(row, 2), paint, false);
                Line::from(spans)
            }
            Role::Hint | Role::Footer => Line::from(span(paint, Tone::Muted, row.cells.join("  "))),
            Role::Section => Line::from(span(paint, Tone::Heading, cell(row, 0).to_uppercase())),
            Role::Budget | Role::Plain => {
                let mut spans = vec![span(paint, Tone::Muted, cell(row, 0))];
                if row.cells.len() > 1 {
                    spans.push(Span::raw(format!("  {}", row.cells[1..].join("  "))));
                }
                if let Some(note) = &row.note {
                    spans.push(span(paint, Tone::Muted, format!("  {note}")));
                }
                Line::from(spans)
            }
        };
        out.extend(wrap::fit(&line, width));
    }
    out
}
