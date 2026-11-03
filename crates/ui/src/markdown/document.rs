//! Block layout follows parsed structure, so literal Markdown-looking text and
//! code stay literal while nested blocks retain their own indentation.

use std::ops::Range;

use pulldown_cmark::{BlockQuoteKind, CodeBlockKind, Event, Parser, Tag, TextMergeWithOffset};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tui_markdown::{AlertKind, StyleSheet};

use super::{Format, table};
use crate::render::Prefix;

pub(crate) struct Row {
    pub line: Line<'static>,
    pub prefix: Option<Prefix>,
    pub units: crate::unit::Units,
    pub cells: Vec<crate::unit::CellFragment>,
}

impl From<Line<'static>> for Row {
    fn from(line: Line<'static>) -> Self {
        Self {
            line,
            prefix: None,
            units: Vec::new(),
            cells: Vec::new(),
        }
    }
}

struct Node<'a> {
    event: Event<'a>,
    range: Range<usize>,
    children: Vec<Node<'a>>,
}

fn tree<'a>(events: &mut impl Iterator<Item = (Event<'a>, Range<usize>)>) -> Vec<Node<'a>> {
    let mut nodes = Vec::new();
    while let Some((event, range)) = events.next() {
        if matches!(event, Event::End(_)) {
            break;
        }
        let children = if matches!(event, Event::Start(_)) {
            tree(events)
        } else {
            Vec::new()
        };
        nodes.push(Node {
            event,
            range,
            children,
        });
    }
    nodes
}

pub(super) fn render(text: &str, format: &Format, width: Option<usize>) -> Vec<Row> {
    let parser = Parser::new_ext(text, crate::unit::options());
    let nodes = tree(&mut TextMergeWithOffset::new(parser.into_offset_iter()));
    Document {
        text,
        format,
        width,
    }
    .blocks(&nodes, 0)
}

struct Document<'a> {
    text: &'a str,
    format: &'a Format,
    width: Option<usize>,
}

impl Document<'_> {
    fn indented(&self, columns: usize) -> Document<'_> {
        Document {
            text: self.text,
            format: self.format,
            width: self.width.map(|width| width.saturating_sub(columns).max(1)),
        }
    }

    fn blocks(&self, nodes: &[Node<'_>], depth: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut inline_start = 0;
        for (i, node) in nodes.iter().enumerate() {
            let block = matches!(
                &node.event,
                Event::Start(
                    Tag::Paragraph
                        | Tag::Heading { .. }
                        | Tag::List(_)
                        | Tag::BlockQuote(_)
                        | Tag::CodeBlock(_)
                        | Tag::Table(_)
                        | Tag::DefinitionList
                        | Tag::DefinitionListTitle
                        | Tag::DefinitionListDefinition
                        | Tag::FootnoteDefinition(_)
                        | Tag::HtmlBlock
                        | Tag::MetadataBlock(_),
                ) | Event::Rule
            );
            if !block {
                continue;
            }
            if inline_start < i {
                rows.extend(self.inline(&nodes[inline_start..i], Style::default(), true, depth));
            }
            let gap = matches!(
                node.event,
                Event::Start(
                    Tag::Paragraph
                        | Tag::Heading { .. }
                        | Tag::BlockQuote(_)
                        | Tag::CodeBlock(_)
                        | Tag::HtmlBlock
                        | Tag::MetadataBlock(_)
                ) | Event::Rule
            );
            if gap && !rows.is_empty() && rows.last().is_some_and(|r| !r.line.spans.is_empty()) {
                rows.push(Line::default().into());
            }
            rows.extend(self.block(node, depth));
            inline_start = i + 1;
        }
        if inline_start < nodes.len() {
            rows.extend(self.inline(&nodes[inline_start..], Style::default(), true, depth));
        }
        rows
    }

    fn block(&self, node: &Node<'_>, depth: usize) -> Vec<Row> {
        let sheet = &self.format.sheet;
        match &node.event {
            Event::Start(Tag::Paragraph) => {
                self.inline(&node.children, Style::default(), true, depth)
            }
            Event::Start(Tag::Heading {
                level,
                id,
                classes,
                attrs,
            }) => {
                let mut rows =
                    self.inline(&node.children, sheet.heading(*level as u8), true, depth);
                let mut metadata = Vec::new();
                if let Some(id) = id {
                    metadata.push(format!("#{id}"));
                }
                metadata.extend(classes.iter().map(|class| format!(".{class}")));
                metadata.extend(attrs.iter().map(|(key, value)| {
                    value
                        .as_ref()
                        .map_or_else(|| key.to_string(), |value| format!("{key}={value}"))
                }));
                if !metadata.is_empty()
                    && let Some(last) = rows.last_mut()
                {
                    last.line.spans.push(Span::styled(
                        format!(" {{{}}}", metadata.join(" ")),
                        sheet.heading_meta(),
                    ));
                }
                rows
            }
            Event::Start(Tag::List(start)) => {
                let loose = node.children.iter().any(|item| {
                    item.children
                        .iter()
                        .any(|n| matches!(n.event, Event::Start(Tag::Paragraph)))
                });
                let mut rows = Vec::new();
                for (i, item) in node.children.iter().enumerate() {
                    if i > 0 && loose {
                        rows.push(Line::default().into());
                    }
                    let first = item.children.first().and_then(|n| {
                        if matches!(n.event, Event::Start(Tag::Paragraph | Tag::Heading { .. })) {
                            n.children.first()
                        } else {
                            Some(n)
                        }
                    });
                    let marker = match first.map(|n| &n.event) {
                        Some(Event::TaskListMarker(true)) => "☑ ".to_string(),
                        Some(Event::TaskListMarker(false)) => "☐ ".to_string(),
                        _ => start.map_or_else(
                            || ["• ", "◦ ", "▪ "][depth % 3].to_string(),
                            |n| format!("{}. ", n.saturating_add(i as u64)),
                        ),
                    };
                    let mut body = self
                        .indented(pi_store::text::width(&marker))
                        .blocks(&item.children, depth + 1);
                    if body.is_empty() {
                        body.push(Line::default().into());
                    }
                    prefix(
                        &mut body,
                        Line::from(Span::styled(marker.clone(), sheet.heading_meta())),
                        Line::from(" ".repeat(pi_store::text::width(&marker))),
                    );
                    rows.extend(body);
                }
                rows
            }
            Event::Start(Tag::BlockQuote(kind)) => {
                let mut rows = self.indented(2).blocks(&node.children, depth);
                let style = match kind {
                    None => sheet.blockquote(),
                    Some(kind) => {
                        let kind = match kind {
                            BlockQuoteKind::Note => AlertKind::Note,
                            BlockQuoteKind::Tip => AlertKind::Tip,
                            BlockQuoteKind::Important => AlertKind::Important,
                            BlockQuoteKind::Warning => AlertKind::Warning,
                            BlockQuoteKind::Caution => AlertKind::Caution,
                        };
                        let style = sheet.alert(kind);
                        rows.insert(
                            0,
                            Line::from(Span::styled(
                                format!("{} {}", sheet.alert_icon(kind), sheet.alert_label(kind)),
                                style.add_modifier(Modifier::BOLD),
                            ))
                            .into(),
                        );
                        style
                    }
                };
                for row in &mut rows {
                    row.line.style = row.line.style.patch(style);
                }
                let bar = Line::from(Span::styled("│ ", style));
                prefix(&mut rows, bar.clone(), bar);
                rows
            }
            Event::Start(Tag::DefinitionList) => self.blocks(&node.children, depth),
            Event::Start(Tag::DefinitionListTitle) => {
                self.inline(&node.children, sheet.definition_term(), true, depth)
            }
            Event::Start(Tag::DefinitionListDefinition) => {
                let mut rows = self.indented(2).blocks(&node.children, depth);
                prefix(&mut rows, Line::from("  "), Line::from("  "));
                rows
            }
            Event::Start(Tag::FootnoteDefinition(label)) => {
                let marker = format!("[{label}]: ");
                let mut rows = self
                    .indented(pi_store::text::width(&marker))
                    .blocks(&node.children, depth);
                let rest = " ".repeat(pi_store::text::width(&marker));
                prefix(
                    &mut rows,
                    Line::from(Span::styled(marker, sheet.footnote_def())),
                    Line::from(rest),
                );
                rows
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                let body = plain(&node.children);
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.as_ref(),
                    CodeBlockKind::Indented => "",
                };
                let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
                let fence = "`".repeat((longest + 1).max(3));
                let newline = if body.ends_with('\n') { "" } else { "\n" };
                self.delegate(&format!("{fence}{info}\n{body}{newline}{fence}"))
            }
            Event::Start(Tag::Table(alignments)) => {
                let mut header = Vec::new();
                let mut body = Vec::new();
                let mut spillover = Vec::new();
                for row in &node.children {
                    let source = self.text[row.range.clone()].trim();
                    // Lenient table parsing can absorb the following paragraph.
                    if alignments.len() > 1
                        && !source.starts_with('|')
                        && !source.ends_with('|')
                        && crate::block::cells(source).len() == 1
                    {
                        spillover.extend(self.inline(&row.children, Style::default(), true, depth));
                        continue;
                    }
                    let cells = row
                        .children
                        .iter()
                        .map(|cell| {
                            table::Cell::new(
                                self.inline(&cell.children, Style::default(), true, depth)
                                    .into_iter()
                                    .map(|row| row.line)
                                    .collect(),
                                cell.range.start,
                            )
                        })
                        .collect();
                    if matches!(row.event, Event::Start(Tag::TableHead)) {
                        header = cells;
                    } else {
                        body.push(cells);
                    }
                }
                let mut rows = table::render(header, body, alignments, self.width, sheet);
                if !spillover.is_empty() {
                    rows.push(Line::default().into());
                }
                rows.extend(spillover);
                rows
            }
            Event::Start(Tag::MetadataBlock(_)) => self.delegate(&self.text[node.range.clone()]),
            Event::Start(Tag::HtmlBlock) => plain(&node.children)
                .lines()
                .map(|line| Line::from(Span::styled(line.to_string(), sheet.html())).into())
                .collect(),
            Event::Rule => vec![Row {
                line: Line::from(Span::styled(
                    "─".repeat(self.width.unwrap_or(80).clamp(1, 80)),
                    sheet.table_border(),
                )),
                prefix: Some(Prefix {
                    first: Line::default(),
                    rest: Line::default(),
                    rule: true,
                }),
                units: Vec::new(),
                cells: Vec::new(),
            }],
            _ => self.inline(&node.children, Style::default(), true, depth),
        }
    }

    fn delegate(&self, source: &str) -> Vec<Row> {
        let parsed = tui_markdown::from_str_with_options(source, &self.format.options);
        parsed
            .lines
            .into_iter()
            .map(|line| {
                let style = parsed.style.patch(line.style);
                Line::from(
                    line.spans
                        .into_iter()
                        .map(|s| Span::styled(s.content.into_owned(), style.patch(s.style)))
                        .collect::<Vec<_>>(),
                )
                .into()
            })
            .collect()
    }

    fn inline(&self, nodes: &[Node<'_>], style: Style, autolink: bool, depth: usize) -> Vec<Row> {
        let mut lines = vec![Line::default()];
        self.spans(nodes, style, autolink, depth, &mut lines);
        lines.into_iter().map(Row::from).collect()
    }

    fn spans(
        &self,
        nodes: &[Node<'_>],
        style: Style,
        autolink: bool,
        depth: usize,
        lines: &mut Vec<Line<'static>>,
    ) {
        let sheet = &self.format.sheet;
        for node in nodes {
            match &node.event {
                Event::Text(text) if autolink => {
                    for part in super::link_finder().spans(text) {
                        let style = if part.kind().is_some() {
                            style.patch(sheet.link())
                        } else {
                            style
                        };
                        push(lines, part.as_str(), style);
                    }
                }
                Event::Text(text) => push(lines, text, style),
                Event::Code(text) => push(lines, text, style.patch(sheet.code())),
                Event::SoftBreak => push(lines, " ", style),
                Event::HardBreak => lines.push(Line::default()),
                Event::InlineHtml(text) | Event::Html(text) => {
                    push(lines, text, style.patch(sheet.html()))
                }
                Event::InlineMath(text) => push(
                    lines,
                    &format!("${text}$"),
                    style.patch(sheet.math_inline()),
                ),
                Event::DisplayMath(text) => push(
                    lines,
                    &format!("$${text}$$"),
                    style.patch(sheet.math_display()),
                ),
                Event::FootnoteReference(label) => push(
                    lines,
                    &format!("[{label}]"),
                    style.patch(sheet.footnote_ref()),
                ),
                Event::TaskListMarker(checked) if depth == 0 => {
                    push(lines, if *checked { "☑ " } else { "☐ " }, style)
                }
                Event::TaskListMarker(_) => {}
                Event::Start(Tag::Link {
                    dest_url, title, ..
                }) => {
                    self.spans(
                        &node.children,
                        style.patch(sheet.link()),
                        false,
                        depth,
                        lines,
                    );
                    let label = plain(&node.children);
                    let distinct = label != dest_url.as_ref()
                        && format!("mailto:{label}") != dest_url.as_ref();
                    self.destination(lines, style, dest_url, title, distinct);
                }
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let alt = plain(&node.children).replace('\n', " ");
                    let description = if alt.is_empty() {
                        "[img]".to_string()
                    } else {
                        format!("[img] {alt}")
                    };
                    push(lines, &description, style.patch(sheet.image_alt()));
                    let distinct = !dest_url.is_empty() && alt != dest_url.as_ref();
                    self.destination(lines, style, dest_url, title, distinct);
                }
                Event::Start(tag) => {
                    let nested = match tag {
                        Tag::Emphasis => style.add_modifier(Modifier::ITALIC),
                        Tag::Strong => style.add_modifier(Modifier::BOLD),
                        Tag::Strikethrough => style.add_modifier(Modifier::CROSSED_OUT),
                        Tag::Superscript | Tag::Subscript => {
                            style.add_modifier(Modifier::DIM | Modifier::ITALIC)
                        }
                        _ => style,
                    };
                    self.spans(&node.children, nested, autolink, depth, lines);
                }
                Event::End(_) | Event::Rule => {}
            }
        }
    }

    fn destination(
        &self,
        lines: &mut Vec<Line<'static>>,
        style: Style,
        url: &str,
        title: &str,
        distinct: bool,
    ) {
        if !distinct && title.is_empty() {
            return;
        }
        push(lines, " (", style);
        if distinct {
            push(lines, url, style.patch(self.format.sheet.link()));
        }
        if !title.is_empty() {
            let space = if distinct { " " } else { "" };
            push(lines, &format!("{space}\"{title}\""), style);
        }
        push(lines, ")", style);
    }
}

fn push(lines: &mut Vec<Line<'static>>, text: &str, style: Style) {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        if !line.is_empty() {
            lines
                .last_mut()
                .expect("one line")
                .spans
                .push(Span::styled(line.to_string(), style));
        }
    }
}

fn plain(nodes: &[Node<'_>]) -> String {
    fn append(nodes: &[Node<'_>], text: &mut String) {
        for node in nodes {
            match &node.event {
                Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                    text.push_str(t)
                }
                Event::SoftBreak | Event::HardBreak => text.push('\n'),
                _ => append(&node.children, text),
            }
        }
    }
    let mut text = String::new();
    append(nodes, &mut text);
    text
}

fn prefix(rows: &mut [Row], first: Line<'static>, rest: Line<'static>) {
    for (i, row) in rows.iter_mut().enumerate() {
        let mut first = if i == 0 { first.clone() } else { rest.clone() };
        let mut rest = rest.clone();
        let mut rule = false;
        if let Some(inner) = row.prefix.take() {
            first.spans.extend(inner.first.spans);
            rest.spans.extend(inner.rest.spans);
            rule = inner.rule;
        }
        row.prefix = Some(Prefix { first, rest, rule });
    }
}

#[cfg(test)]
mod tests {
    use crate::render::{Coded, Paint, render_coded, render_markdown};

    fn lines(source: &str) -> Vec<String> {
        render_markdown(source, &Paint::new(true))
            .into_iter()
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn lists_show_nesting_and_task_state() {
        assert_eq!(
            lines(
                "- 第一项\n- 第二项\n    - 嵌套一项\n    - 嵌套二项\n        - 再套一层\n- 第三项"
            ),
            [
                "• 第一项",
                "• 第二项",
                "  ◦ 嵌套一项",
                "  ◦ 嵌套二项",
                "    ▪ 再套一层",
                "• 第三项"
            ]
        );
        assert_eq!(
            lines("- [x] 已完成\n- [ ] 未完成\n- [ ] 待定"),
            ["☑ 已完成", "☐ 未完成", "☐ 待定"]
        );
        assert_eq!(
            lines("9. first\n10. second\n    - nested"),
            ["9. first", "10. second", "    ◦ nested"]
        );
        // This combination used to panic in the underlying renderer when
        // heading markers were hidden.
        assert!(
            lines("- [ ] task\n---")
                .iter()
                .any(|line| line.contains("task"))
        );
    }

    #[test]
    fn definitions_and_quotes_keep_their_block_structure() {
        assert_eq!(
            lines("术语\n: 解释内容\n: 第二种解释"),
            ["术语", "  解释内容", "  第二种解释"]
        );
        let quotes = lines("> 这是一段引用。\n>\n>> 嵌套引用。\n>\n> —— 出处\n\n---");
        assert_eq!(
            &quotes[..6],
            [
                "│ 这是一段引用。",
                "│ ",
                "│ │ 嵌套引用。",
                "│ ",
                "│ —— 出处",
                ""
            ]
        );
        assert!(quotes[6].chars().all(|c| c == '─'));
    }

    #[test]
    fn tables_keep_their_layout_inside_containers() {
        for table in [
            "| a | b |\n|---|---|\n| x | y |",
            "a | b\n:--- | ---:\n**x** | `y`\\|z\n\\>literal | [site][ref]",
            "| a | b |\n|---|---|",
        ] {
            let references = "\n\n[ref]: https://example.com";
            let plain = lines(&format!("{table}{references}"));
            assert!(plain.iter().any(|line| line.contains('━')), "{plain:?}");
            for (first, rest, first_out, rest_out) in [
                ("> ", "> ", "│ ", "│ "),
                ("> > ", "> > ", "│ │ ", "│ │ "),
                ("- ", "  ", "• ", "  "),
                ("> - ", ">   ", "│ • ", "│   "),
                ("- > ", "  > ", "• │ ", "  │ "),
            ] {
                let nested = table
                    .lines()
                    .enumerate()
                    .map(|(i, line)| format!("{}{line}", if i == 0 { first } else { rest }))
                    .collect::<Vec<_>>()
                    .join("\n");
                let expected = plain
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}{line}", if i == 0 { first_out } else { rest_out }))
                    .collect::<Vec<_>>();
                assert_eq!(
                    lines(&format!("{nested}{references}")),
                    expected,
                    "{nested}"
                );
            }
        }
    }

    #[test]
    fn links_keep_destinations_titles_and_image_descriptions() {
        let source = "[inline](https://example.com) [title](https://example.com \"说明\") [reference][site] <https://example.com>\n\n![占位图](https://example.com/image.png \"图片标题\")\n\n[site]: https://example.com";
        let result = lines(source);
        assert_eq!(
            result[0],
            "inline (https://example.com) title (https://example.com \"说明\") reference (https://example.com) https://example.com"
        );
        assert_eq!(
            result[2],
            "[img] 占位图 (https://example.com/image.png \"图片标题\")"
        );
        assert_eq!(
            lines(
                "[https://example.com](https://example.com \"说明\") ![](https://example.com/image.png)"
            ),
            ["https://example.com (\"说明\") [img] (https://example.com/image.png)"]
        );
    }

    #[test]
    fn literal_markers_and_code_are_not_rewritten() {
        assert_eq!(lines("`- [x] > ---`"), ["- [x] > ---"]);
        assert_eq!(
            lines("\\- literal\n\n\\> literal"),
            ["- literal", "", "> literal"]
        );
        let code = "- [x] literal\n> quote\n---\n\necho `date`";
        let source = format!(
            "- item\n\n  ```sh\n{}\n  ```",
            code.lines()
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let rows = render_coded(&source, &Paint::new(true));
        assert!(
            rows.iter()
                .any(|row| row.coded == Coded::Badge(code.to_string()))
        );
    }
}
