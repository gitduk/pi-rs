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
// A name column of the wide width, the three columns a table may carry and
// their gaps.
const MIN_COLUMN_WIDTH: usize = WIDE_WIDTH + 3 * (NUMERIC_WIDTH + NUMERIC_GAP);
const COLUMN_GAP: usize = 4;
const MARGIN: &str = "  ";

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
    let margin = if width >= 16 { MARGIN } else { "" };
    let body_width = width - 2 * margin.len();
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
    // An odd count leaves one section over, and the first spans the width for
    // it; an even one puts every section in a column of its own.
    let directories = match sections.len() % 2 {
        0 => sections.as_slice(),
        _ => {
            if let Some(summary) = sections.first() {
                out.push(Line::default());
                out.extend(render(summary, body_width, paint));
            }
            sections.get(1..).unwrap_or_default()
        }
    };
    // A report whose one table was taken as the summary has no columns left
    // to lay out.
    if directories.is_empty() {
        indent(&mut out, margin);
        return out;
    }
    // As many columns as hold a table, then rows of the same length: four
    // tables that fit in three columns read as two and two, not three and one.
    let fits = ((body_width + COLUMN_GAP) / (MIN_COLUMN_WIDTH + COLUMN_GAP)).max(1);
    let rows = directories.len().div_ceil(fits);
    let columns = directories.len().div_ceil(rows).max(1);
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
    indent(&mut out, margin);
    out
}

// Every line starts at the same margin, so a narrow terminal keeps its text
// off both edges.
fn indent(out: &mut [Line<'static>], margin: &'static str) {
    for line in out {
        line.spans.insert(0, Span::raw(margin));
    }
}

struct Columns {
    name: usize,
    // Every cell after the first is a column, right-aligned: most reports
    // carry two, and a listing that names what a number is may carry three.
    numeric: Vec<usize>,
}

impl Columns {
    fn of(rows: &[&Row], width: usize) -> Self {
        let cells = rows.iter().map(|row| row.cells.len()).max().unwrap_or(1);
        let numeric: Vec<usize> = (1..cells)
            .map(|at| {
                // A section row names the columns under it, so its own cells
                // count toward the width a heading has to fit in.
                rows.iter()
                    .filter(|row| matches!(row.role, Role::Budget | Role::Section))
                    .map(|row| text::width(cell(row, at)))
                    .max()
                    .unwrap_or(0)
                    .max(NUMERIC_WIDTH)
            })
            .collect();
        let used: usize = numeric.iter().sum::<usize>() + numeric.len() * NUMERIC_GAP;
        Self {
            name: width.saturating_sub(used),
            numeric,
        }
    }

    fn values(
        &self,
        spans: &mut Vec<Span<'static>>,
        cells: &[String],
        paint: Option<&Paint>,
        headings: bool,
    ) {
        let gap = " ".repeat(NUMERIC_GAP);
        for (at, width) in self.numeric.iter().enumerate() {
            let value = cells.get(at).map_or("", String::as_str);
            spans.push(Span::raw(format!(
                "{gap}{}",
                " ".repeat(width.saturating_sub(text::width(value)))
            )));
            // Only the first column carries the value read against its name;
            // the rest are asides, and so are all of a heading.
            let tone = match (at, headings) {
                (0, false) => Tone::Plain,
                _ => Tone::Muted,
            };
            spans.push(span(paint, tone, value));
        }
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
                columns.values(
                    &mut spans,
                    row.cells.get(1..).unwrap_or_default(),
                    paint,
                    true,
                );
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
                columns.values(
                    &mut spans,
                    row.cells.get(1..).unwrap_or_default(),
                    paint,
                    false,
                );
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

#[cfg(test)]
mod tests {
    use super::*;

    // A report carrying one table has it taken as the summary, which leaves
    // the column layout nothing to arrange: the lines must still be said.
    #[test]
    fn a_report_with_one_table_keeps_its_lines_and_its_margin() {
        let listing = Listing::report([
            Row::new(["Mask"]).with_role(Role::Title),
            Row::new(["NAME", "BY", "CHARS", "MASKED", "VALUE"]).with_role(Role::Section),
            Row::new(["TELEGRAM_BOT_TOKEN", "named", "46", "2", "7341…gH30"])
                .with_role(Role::Budget),
        ]);
        let out: Vec<String> = lines(&listing, 80, None)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            out.iter().any(|line| line.contains("TELEGRAM_BOT_TOKEN")),
            "{out:?}"
        );
        let margin = |line: &String| line.len() - line.trim_start().len();
        assert!(
            out.iter()
                .filter(|line| !line.trim().is_empty())
                .all(|line| margin(line) == margin(&out[0])),
            "{out:?}"
        );
    }
}
