//! Content-aware columns, with vertical records when values no longer scan well.

use pi_store::text::width;
use pulldown_cmark::Alignment;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tui_markdown::StyleSheet;

use super::document::Row;

const CELL_PADDING: usize = 1;
const COLUMN_GAP: usize = 2;

#[derive(Default, Clone)]
pub(super) struct Cell {
    id: usize,
    lines: Vec<Line<'static>>,
    text: std::sync::Arc<str>,
}

impl Cell {
    pub(super) fn new(lines: Vec<Line<'static>>, id: usize) -> Self {
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            id,
            lines,
            text: text.into(),
        }
    }

    fn width(&self) -> usize {
        self.lines
            .iter()
            .map(|line| width(&line.to_string()))
            .max()
            .unwrap_or(0)
    }

    fn minimum_width(&self) -> usize {
        pi_store::text::graphemes(&self.text)
            .map(|(_, g)| width(g))
            .max()
            .unwrap_or(1)
            .max(1)
    }

    fn wrap(&self, columns: usize) -> Vec<Line<'static>> {
        let lines: Vec<_> = self
            .lines
            .iter()
            .flat_map(|line| crate::wrap::fit(line, columns))
            .collect();
        if lines.is_empty() {
            vec![Line::default()]
        } else {
            lines
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Token,
    Prose,
    Compact,
}

struct Column {
    kind: Kind,
    natural: usize,
    floor: usize,
}

fn columns(header: &[Cell], rows: &[Vec<Cell>]) -> Vec<Column> {
    header
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let mut natural = label.width().max(3);
            let (mut words, mut long, mut cells, mut total_width, mut token_width) =
                (0, 0, 0, 0, 0);
            for row in rows {
                let cell = &row[i];
                natural = natural.max(cell.width());
                let mut count = 0;
                for token in cell.text.split_whitespace() {
                    let w = width(token);
                    token_width = token_width.max(w);
                    long += usize::from(w >= 20);
                    count += 1;
                }
                if count > 0 {
                    words += count;
                    cells += 1;
                    total_width += width(&cell.text);
                }
            }
            let kind = if long > 0 && long >= words - long {
                Kind::Token
            } else if cells > 0 && (words >= 4 * cells || total_width >= 28 * cells) {
                Kind::Prose
            } else {
                Kind::Compact
            };
            let floor = match kind {
                Kind::Token | Kind::Prose => 16,
                Kind::Compact => label
                    .text
                    .split_whitespace()
                    .map(width)
                    .max()
                    .unwrap_or(0)
                    .max(token_width.min(16)),
            }
            .clamp(3, natural);
            Column {
                kind,
                natural,
                floor,
            }
        })
        .collect()
}

// Equalize spare width within each kind in bulk, including enormous URLs.
fn shrink(widths: &mut [usize], floors: &[usize], columns: &[Column], mut remove: usize) {
    for kind in [Kind::Token, Kind::Prose, Kind::Compact] {
        let slack = |i: usize, w: usize| {
            if columns[i].kind == kind {
                w.saturating_sub(floors[i])
            } else {
                0
            }
        };
        let total: usize = widths.iter().enumerate().map(|(i, &w)| slack(i, w)).sum();
        let take = remove.min(total);
        if take == 0 {
            continue;
        }
        let (mut low, mut high) = (
            0,
            widths
                .iter()
                .enumerate()
                .map(|(i, &w)| slack(i, w))
                .max()
                .unwrap_or(0),
        );
        while low < high {
            let cap = low + (high - low) / 2;
            let removed: usize = widths
                .iter()
                .enumerate()
                .map(|(i, &w)| slack(i, w).saturating_sub(cap))
                .sum();
            if removed > take {
                low = cap + 1;
            } else {
                high = cap;
            }
        }
        let mut removed = 0;
        for (i, w) in widths.iter_mut().enumerate() {
            let n = slack(i, *w).saturating_sub(low);
            *w -= n;
            removed += n;
        }
        let mut remainder = take - removed;
        for (i, w) in widths.iter_mut().enumerate() {
            if remainder == 0 {
                break;
            }
            if columns[i].kind == kind && slack(i, *w) == low {
                *w -= 1;
                remainder -= 1;
            }
        }
        remove -= take;
        if remove == 0 {
            break;
        }
    }
}

fn allocate(columns: &[Column], available: Option<usize>) -> Option<Vec<usize>> {
    let mut widths: Vec<_> = columns.iter().map(|column| column.natural).collect();
    let Some(available) = available else {
        return Some(widths);
    };
    let reserved = columns.len() * CELL_PADDING * 2 + columns.len().saturating_sub(1) * COLUMN_GAP;
    let budget = available.saturating_sub(reserved);
    if budget < 3 * columns.len() {
        return None;
    }
    let mut floors: Vec<_> = columns.iter().map(|column| column.floor).collect();
    let excess = floors.iter().sum::<usize>().saturating_sub(budget);
    shrink(&mut floors, &vec![3; columns.len()], columns, excess);
    let excess = widths.iter().sum::<usize>().saturating_sub(budget);
    shrink(&mut widths, &floors, columns, excess);
    Some(widths)
}

type WrappedRow = Vec<Vec<Line<'static>>>;

fn wrap_row(row: &[Cell], widths: &[usize]) -> WrappedRow {
    row.iter()
        .zip(widths)
        .map(|(cell, &w)| cell.wrap(w))
        .collect()
}

fn records_needed(
    rows: &[Vec<Cell>],
    wrapped: &[WrappedRow],
    widths: &[usize],
    columns: &[Column],
) -> bool {
    let affected = rows
        .iter()
        .zip(wrapped)
        .filter(|(row, wrapped)| {
            let mut tall = 0;
            for (((cell, lines), &w), column) in row.iter().zip(*wrapped).zip(widths).zip(columns) {
                let split = cell.text.split_whitespace().any(|token| width(token) > w);
                match column.kind {
                    Kind::Compact if split => return true,
                    Kind::Token if split && w < 12 => return true,
                    Kind::Prose if w < 12 && lines.len() >= 7 => return true,
                    _ => {}
                }
                tall += usize::from(column.kind != Kind::Compact && lines.len() >= 4);
            }
            tall >= 2
        })
        .count();
    let threshold = if rows.len() == 1 {
        1
    } else {
        2.max(rows.len().div_ceil(3))
    };
    affected >= threshold
}

pub(super) fn render(
    mut header: Vec<Cell>,
    mut rows: Vec<Vec<Cell>>,
    alignments: &[Alignment],
    available: Option<usize>,
    sheet: &impl StyleSheet,
) -> Vec<Row> {
    let count = alignments.len();
    if count == 0 {
        return Vec::new();
    }
    for row in std::iter::once(&mut header).chain(&mut rows) {
        row.truncate(count);
        row.resize(count, Cell::default());
    }
    let columns = columns(&header, &rows);
    let label_style = sheet.table_header().add_modifier(Modifier::BOLD);
    // Header colour takes precedence over inline code and link colours.
    for cell in &mut header {
        for line in &mut cell.lines {
            for span in &mut line.spans {
                span.style = span.style.patch(label_style);
            }
        }
    }
    let border_style = sheet.table_border();
    let Some(widths) = allocate(&columns, available) else {
        return records(
            &header,
            &rows,
            &columns,
            available,
            label_style,
            border_style,
        );
    };
    let wrapped: Vec<_> = rows.iter().map(|row| wrap_row(row, &widths)).collect();
    if records_needed(&rows, &wrapped, &widths, &columns) {
        return records(
            &header,
            &rows,
            &columns,
            available,
            label_style,
            border_style,
        );
    }
    let mut out = Vec::new();
    grid_row(
        &mut out,
        &wrap_row(&header, &widths),
        &header,
        &widths,
        alignments,
        label_style,
    );
    out.push(separator(&widths, '━', border_style));
    for (i, row) in wrapped.iter().enumerate() {
        if i > 0 {
            out.push(separator(&widths, '─', border_style));
        }
        grid_row(
            &mut out,
            row,
            &rows[i],
            &widths,
            alignments,
            sheet.table_cell(),
        );
    }
    out
}

fn separator(widths: &[usize], glyph: char, style: Style) -> Row {
    let segments = widths
        .iter()
        .map(|w| glyph.to_string().repeat(w + CELL_PADDING * 2))
        .collect::<Vec<_>>()
        .join(&" ".repeat(COLUMN_GAP));
    Line::from(Span::styled(segments, style)).into()
}

// Selection follows cell content, including in the vertical layout, and omits padding.
fn append_cell(row: &mut Row, line: &Line<'_>, style: Style, source: &Cell) {
    let text = line.to_string();
    let lead = text.len() - text.trim_start().len();
    let content = text.trim();
    if !content.is_empty() {
        let start = width(&row.line.to_string()) + width(&text[..lead]);
        let range = start..start + width(content);
        row.units.push(range.clone());
        row.cells.push(crate::unit::CellFragment {
            range,
            id: source.id,
            text: source.text.clone(),
        });
    }
    row.line.spans.extend(line.spans.iter().map(|span| {
        Span::styled(
            span.content.to_string(),
            style.patch(line.style).patch(span.style),
        )
    }));
}

fn grid_row(
    out: &mut Vec<Row>,
    cells: &WrappedRow,
    sources: &[Cell],
    widths: &[usize],
    alignments: &[Alignment],
    style: Style,
) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    for i in 0..height {
        let last_visible = cells
            .iter()
            .rposition(|cell| cell.get(i).is_some_and(|line| width(&line.to_string()) > 0));
        let mut row: Row = Line::default().into();
        let Some(last_visible) = last_visible else {
            out.push(row);
            continue;
        };
        for (column, ((cell, &w), alignment)) in cells
            .iter()
            .zip(widths)
            .zip(alignments)
            .enumerate()
            .take(last_visible + 1)
        {
            let line = cell.get(i).cloned().unwrap_or_default();
            let padding = w.saturating_sub(width(&line.to_string()));
            let left = match alignment {
                Alignment::Left | Alignment::None => 0,
                Alignment::Center => padding / 2,
                Alignment::Right => padding,
            };
            row.line
                .spans
                .push(Span::raw(" ".repeat(CELL_PADDING + left)));
            append_cell(&mut row, &line, style, &sources[column]);
            if column < last_visible {
                row.line.spans.push(Span::raw(
                    " ".repeat(padding - left + CELL_PADDING + COLUMN_GAP),
                ));
            }
        }
        out.push(row);
    }
}

fn records(
    header: &[Cell],
    rows: &[Vec<Cell>],
    columns: &[Column],
    available: Option<usize>,
    label_style: Style,
    border_style: Style,
) -> Vec<Row> {
    let label_width = header
        .iter()
        .map(|label| width(&label.text))
        .max()
        .unwrap_or(0);
    let minimum = if columns.iter().any(|c| c.kind != Kind::Compact) {
        24
    } else {
        12
    };
    let aligned = available.is_none_or(|w| 1 + label_width + 2 + minimum <= w);
    let mut out = Vec::new();
    if rows.is_empty() {
        for label in header {
            for line in label.wrap(available.unwrap_or(label_width).max(1)) {
                let mut row = Line::default().into();
                append_cell(&mut row, &line, label_style, label);
                out.push(row);
            }
        }
        return out;
    }
    for (i, record) in rows.iter().enumerate() {
        if i > 0 {
            let w = available.unwrap_or_else(|| {
                out.iter()
                    .map(|row| width(&row.line.to_string()))
                    .max()
                    .unwrap_or(0)
            });
            out.push(Line::from(Span::styled("─".repeat(w), border_style)).into());
        }
        for (label, value) in header.iter().zip(record) {
            let indent = if aligned {
                1 + label_width + 2
            } else {
                available.map_or(2, |w| w.saturating_sub(value.minimum_width()).min(2))
            };
            if !aligned {
                let leading = available.map_or(1, |w| usize::from(w > label.minimum_width()));
                let room =
                    available.map_or(label_width.max(1), |w| w.saturating_sub(leading).max(1));
                for line in label.wrap(room) {
                    let mut row = Line::from(" ".repeat(leading)).into();
                    append_cell(&mut row, &line, label_style, label);
                    out.push(row);
                }
            }
            let room =
                available.map_or_else(|| value.width().max(1), |w| w.saturating_sub(indent).max(1));
            for (n, line) in value.wrap(room).iter().enumerate() {
                let mut row: Row = Line::default().into();
                if aligned && n == 0 {
                    row.line.spans.push(Span::raw(" "));
                    append_cell(
                        &mut row,
                        &Line::from(label.text.to_string()),
                        label_style,
                        label,
                    );
                    row.line
                        .spans
                        .push(Span::raw(" ".repeat(label_width - width(&label.text) + 2)));
                } else {
                    row.line.spans.push(Span::raw(" ".repeat(indent)));
                }
                append_cell(&mut row, line, Style::default(), value);
                out.push(row);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::render::{Paint, Rendered, render_coded_at};
    use pi_store::text::width;
    use ratatui::style::Modifier;

    fn render(source: &str, columns: usize) -> Vec<Rendered> {
        let rows = render_coded_at(source, &Paint::new(true), Some(columns));
        for row in &rows {
            let line = row.full_line().to_string();
            assert!(width(&line) <= columns, "{columns} columns: {line:?}");
        }
        rows
    }

    fn lines(source: &str, columns: usize) -> Vec<String> {
        render(source, columns)
            .into_iter()
            .map(|row| row.full_line().to_string())
            .collect()
    }

    #[test]
    fn tables_use_open_columns_and_subdued_rules() {
        let source = "| Name | Value |\n|---|---|\n| foo | bar |\n| baz | qux |";
        let rows = render(source, 80);
        let text: Vec<_> = rows.iter().map(|row| row.full_line().to_string()).collect();
        assert_eq!(
            text,
            [
                " Name    Value",
                "━━━━━━  ━━━━━━━",
                " foo     bar",
                "──────  ───────",
                " baz     qux",
            ]
        );
        let header = rows[0]
            .line
            .spans
            .iter()
            .find(|span| span.content == "Name")
            .unwrap();
        assert!(header.style.fg.is_some());
        assert!(header.style.add_modifier.contains(Modifier::BOLD));
        for i in [1, 3] {
            assert!(
                rows[i].line.spans.iter().all(|span| span.style.fg.is_none()
                    && span.style.add_modifier.contains(Modifier::DIM))
            );
        }
        assert!(
            rows[2]
                .line
                .spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn open_columns_keep_center_and_right_alignment() {
        let source = "| Left | Center | Right |\n|:---|:---:|---:|\n| a | b | c |";
        assert_eq!(
            lines(source, 80),
            [
                " Left    Center    Right",
                "━━━━━━  ━━━━━━━━  ━━━━━━━",
                " a         b           c",
            ]
        );
    }

    #[test]
    fn minimum_width_grid_uses_its_full_budget_before_switching_to_records() {
        let source = "| A | B |\n|---|---|\n| 1 | 2 |";
        assert!(lines(source, 12).iter().any(|line| line.contains('━')));
        assert!(!lines(source, 11).iter().any(|line| line.contains('━')));
    }

    #[test]
    fn table_labels_follow_the_syntax_palette_in_grid_and_record_layouts() {
        use pi_store::theme::{PaletteColor, Syntax, Theme};
        use ratatui::style::Color;
        use std::sync::Arc;

        let source =
            "| Name | Value |\n|---|---|\n| firstlongid | ready |\n| secondlongid | done |";
        for syntax in [Syntax::CatppuccinMocha, Syntax::CatppuccinLatte] {
            let paint = Paint::with_theme(
                true,
                Arc::new(Theme {
                    syntax,
                    ..Theme::default()
                }),
            );
            let (r, g, b) = PaletteColor::Yellow.rgb(syntax);
            for columns in [9, 80] {
                let rows = render_coded_at(source, &paint, Some(columns));
                for label in ["Name", "Value"] {
                    let span = rows
                        .iter()
                        .flat_map(|row| &row.line.spans)
                        .find(|span| span.content == label)
                        .expect("table label");
                    assert_eq!(span.style.fg, Some(Color::Rgb(r, g, b)));
                    assert!(span.style.add_modifier.contains(Modifier::BOLD));
                }
                let rule = rows
                    .iter()
                    .find(|row| row.line.to_string().contains(['━', '─']))
                    .expect("table separator");
                assert!(rule.line.spans.iter().all(|span| span.style.fg.is_none()
                    && span.style.add_modifier.contains(Modifier::DIM)));
            }
        }
    }

    #[test]
    fn long_paths_yield_to_prose_and_short_values() {
        let path = "/very/long/path/".repeat(8);
        let source = format!(
            "| File | Notes | State |\n|---|---|---:|\n| {path} | This explanatory sentence remains readable beside the path. | Ready |\n| {path} | Another explanatory sentence remains readable beside the path. | Done |"
        );
        let got = lines(&source, 68);
        assert!(got.iter().any(|line| line.contains('━')), "{got:?}");
        // The path wraps, while prose keeps useful phrases and status values stay whole.
        assert!(
            got.iter()
                .any(|line| line.contains("This explanatory sentence")),
            "{got:?}"
        );
        assert!(
            got.iter().any(|line| line.trim_end().ends_with("Ready")),
            "{got:?}"
        );
        assert_eq!(got.iter().filter(|line| line.starts_with('─')).count(), 1);
    }

    #[test]
    fn one_outlier_keeps_the_grid_but_systemic_fragmentation_uses_records() {
        let one = "| Key | Date | State |\n|---|---|---|\n| short | 2025-01-01 | Ready |\n| verylongidentifier | 2025-02-02 | Ready |\n| final | 2025-03-03 | Done |";
        assert!(lines(one, 40).iter().any(|line| line.contains('━')));
        let several = "| Key | Notes |\n|---|---|\n| firstlongid | A readable explanatory sentence for this row. |\n| secondlongid | Another readable explanatory sentence for this row. |\n| short | A final readable explanatory sentence for this row. |";
        let got = lines(several, 17);
        assert!(!got.iter().any(|line| line.contains('━')), "{got:?}");
        assert!(got.iter().any(|line| line.trim() == "firstlongid"));
        assert!(got.iter().any(|line| line.trim() == "Notes"));
        assert_eq!(
            got.iter()
                .filter(|line| line.chars().all(|c| c == '─'))
                .count(),
            2
        );
    }

    #[test]
    fn tall_prose_uses_aligned_records_when_labels_leave_room() {
        let sentence = "A long explanatory sentence with several words and details. ".repeat(3);
        let source = format!("| Issue | Reason |\n|---|---|\n| {sentence} | {sentence} |");
        let got = lines(&source, 50);
        assert!(!got.iter().any(|line| line.contains('━')), "{got:?}");
        assert!(
            got[0]
                .trim_start()
                .starts_with("Issue   A long explanatory"),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|line| line.trim_start().starts_with("Reason  A long explanatory"))
        );
    }

    #[test]
    fn narrow_records_keep_unicode_inline_styles_and_destinations() {
        let source = "| 名称 | 内容 |\n|---|---|\n| 中文👩‍💻é | **加粗** `code` [网站][ref] \\| 空格 |\n\n[ref]: https://example.com";
        for columns in [2, 9, 17, 35, 80] {
            let rows = render(source, columns);
            let text: String = rows.iter().map(|row| row.full_line().to_string()).collect();
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            for content in [
                "中文👩‍💻é",
                "加粗",
                "code",
                "网站",
                "https://example.com",
                "|空格",
            ] {
                assert!(compact.contains(content), "{columns}: {compact}");
            }
            assert!(rows.iter().flat_map(|row| &row.line.spans).any(|span| {
                span.content.contains('加') && span.style.add_modifier.contains(Modifier::BOLD)
            }));
            assert!(rows.iter().any(|row| !row.units.is_empty()));
        }
    }

    #[test]
    fn containers_reserve_their_prefixes_before_laying_out_tables() {
        let table = "| Name | Notes |\n|---|---|\n| parser | A readable explanatory sentence for this row. |";
        for (first, rest) in [("> ", "> "), ("10. ", "    "), ("> 10. ", ">     ")] {
            let source = table
                .lines()
                .enumerate()
                .map(|(i, line)| format!("{}{line}", if i == 0 { first } else { rest }))
                .collect::<Vec<_>>()
                .join("\n");
            for columns in [17, 36, 80] {
                render(&source, columns);
            }
        }
    }

    #[test]
    fn header_only_empty_cells_and_following_prose_survive_narrow_layouts() {
        for columns in [2, 8, 30] {
            let got = lines("| Alpha | Beta |\n|---|---|", columns).join("");
            let compact: String = got.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                compact.contains("Alpha") && compact.contains("Beta"),
                "{compact}"
            );
            let got = lines("| A | B |\n|---|---|\n| | |", columns);
            assert!(!got.is_empty());
        }
        let got = lines("| A | B |\n|---|---|\n| x | y |\nFollowing prose", 40);
        assert_eq!(got.last().unwrap(), "Following prose");
        assert!(got.iter().any(|line| line.contains('━')));
    }
}
