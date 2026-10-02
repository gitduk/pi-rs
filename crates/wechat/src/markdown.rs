//! Markdown as the phone reads it: WeChat renders none of it, so every
//! construct the model emits gets reduced to the plainest text that still
//! says what it said. Nothing here touches the channel or the socket.

use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};

#[derive(Default)]
struct TableState {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
}

struct ListState {
    next_num: Option<u64>,
    depth: usize,
}

struct LinkEntry {
    url: String,
    text: String,
}

#[derive(Default)]
struct Formatter {
    out: String,
    link_stack: Vec<LinkEntry>,
    table: Option<TableState>,
    list_stack: Vec<ListState>,
    item_just_started: bool,
    in_heading: bool,
    in_code_block: bool,
    quote_depth: usize,
}

impl Formatter {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            out: String::with_capacity(capacity),
            ..Default::default()
        }
    }

    fn push(&mut self, s: &str) {
        if let Some(entry) = self.link_stack.last_mut() {
            entry.text.push_str(s);
        } else if let Some(table) = &mut self.table {
            table.current_cell.push_str(s);
        } else {
            self.out.push_str(s);
        }
    }

    fn ensure_blank_line(&mut self) {
        if !self.out.ends_with('\n') && !self.out.is_empty() {
            self.out.push('\n');
        }
        if !self.out.ends_with("\n\n") && !self.out.is_empty() {
            self.out.push('\n');
        }
    }

    fn ensure_newline(&mut self) {
        if !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn render_table(&mut self, table: TableState) {
        let headers: Vec<&str> = table.headers.iter().map(|s| s.trim()).collect();
        let has_headers = headers.iter().any(|h| !h.is_empty());
        let cols = if has_headers {
            headers.len()
        } else {
            table.rows.iter().map(|r| r.len()).max().unwrap_or(0)
        };

        if cols == 0 {
            return;
        }

        self.ensure_blank_line();
        if has_headers {
            self.out.push_str(&format!("【{}】\n", headers.join(" | ")));
        }

        if cols == 2 {
            for row in &table.rows {
                let c0 = row.first().map(|s| s.as_str()).unwrap_or("");
                let c1 = row.get(1).map(|s| s.as_str()).unwrap_or("");
                match (c0.is_empty(), c1.is_empty()) {
                    (false, false) => self.out.push_str(&format!("• {c0}: {c1}\n")),
                    (false, true) => self.out.push_str(&format!("• {c0}\n")),
                    (true, false) => self.out.push_str(&format!("• {c1}\n")),
                    (true, true) => {}
                }
            }
        } else if cols > 2 {
            for (i, row) in table.rows.iter().enumerate() {
                if i > 0 {
                    self.out.push('\n');
                }
                let mut started = false;
                for (idx, cell) in row.iter().enumerate() {
                    if cell.is_empty() {
                        continue;
                    }
                    let h = headers.get(idx).copied().unwrap_or("");
                    if !started {
                        if idx == 0 || h.is_empty() {
                            self.out.push_str(&format!("• {cell}\n"));
                        } else {
                            self.out.push_str(&format!("• {h}: {cell}\n"));
                        }
                        started = true;
                    } else if !h.is_empty() {
                        self.out.push_str(&format!("  {h}: {cell}\n"));
                    } else {
                        self.out.push_str(&format!("  {cell}\n"));
                    }
                }
            }
        } else {
            for row in &table.rows {
                if let Some(c) = row.first().filter(|s| !s.is_empty()) {
                    self.out.push_str(&format!("• {c}\n"));
                }
            }
        }
        self.out.push('\n');
    }
}

pub(super) fn format_markdown(input: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let parser = Parser::new_ext(input, opts);
    let mut f = Formatter::with_capacity(input.len());

    for event in parser {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    f.in_heading = true;
                    f.ensure_blank_line();
                    f.out.push('【');
                }
                Tag::Paragraph => {
                    if f.item_just_started {
                        f.item_just_started = false;
                    } else {
                        f.ensure_blank_line();
                        if f.quote_depth > 0 {
                            f.out.push_str("> ");
                        }
                    }
                }
                Tag::BlockQuote(_) => {
                    f.quote_depth += 1;
                }
                Tag::CodeBlock(kind) => {
                    f.ensure_blank_line();
                    let lang = match &kind {
                        CodeBlockKind::Fenced(l) => l.trim(),
                        CodeBlockKind::Indented => "",
                    };
                    if !lang.is_empty() {
                        f.out.push_str(&format!("─── 代码 ({lang}) ───\n"));
                    } else {
                        f.out.push_str("─── 代码 ───\n");
                    }
                    f.in_code_block = true;
                }
                Tag::List(start_num) => {
                    if f.list_stack.is_empty() {
                        f.ensure_blank_line();
                    }
                    let depth = f.list_stack.len() + 1;
                    f.list_stack.push(ListState {
                        next_num: start_num,
                        depth,
                    });
                }
                Tag::Item => {
                    f.item_just_started = true;
                    if let Some(state) = f.list_stack.last_mut() {
                        let indent = "  ".repeat(state.depth.saturating_sub(1));
                        if let Some(num) = state.next_num.as_mut() {
                            f.out.push_str(&format!("{indent}{num}. "));
                            *num += 1;
                        } else {
                            f.out.push_str(&format!("{indent}• "));
                        }
                    }
                }
                Tag::Table(_) => {
                    f.table = Some(TableState::default());
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(t) = &mut f.table {
                        t.current_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(t) = &mut f.table {
                        t.current_cell.clear();
                    }
                }
                Tag::Emphasis => {}
                Tag::Strong => {
                    if !f.in_heading {
                        f.push("「");
                    }
                }
                Tag::Strikethrough => {
                    f.push("~");
                }
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    f.link_stack.push(LinkEntry {
                        url: dest_url.to_string(),
                        text: String::new(),
                    });
                }
                _ => {}
            },
            MdEvent::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    f.in_heading = false;
                    f.out.push_str("】\n\n");
                }
                TagEnd::Paragraph => {
                    f.ensure_newline();
                }
                TagEnd::BlockQuote(_) => {
                    f.quote_depth = f.quote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    f.ensure_newline();
                    f.out.push_str("────────\n\n");
                    f.in_code_block = false;
                }
                TagEnd::Item => {
                    f.ensure_newline();
                    f.item_just_started = false;
                }
                TagEnd::List(_) => {
                    f.list_stack.pop();
                    if f.list_stack.is_empty() {
                        f.ensure_newline();
                    }
                }
                TagEnd::TableHead => {
                    if let Some(t) = &mut f.table {
                        t.headers = std::mem::take(&mut t.current_row);
                    }
                }
                TagEnd::TableRow => {
                    if let Some(t) = &mut f.table {
                        t.rows.push(std::mem::take(&mut t.current_row));
                    }
                }
                TagEnd::TableCell => {
                    if let Some(t) = &mut f.table {
                        let cell = std::mem::take(&mut t.current_cell).trim().to_string();
                        t.current_row.push(cell);
                    }
                }
                TagEnd::Table => {
                    if let Some(t) = f.table.take() {
                        f.render_table(t);
                    }
                }
                TagEnd::Emphasis => {}
                TagEnd::Strong => {
                    if !f.in_heading {
                        f.push("」");
                    }
                }
                TagEnd::Strikethrough => {
                    f.push("~");
                }
                TagEnd::Link => {
                    if let Some(entry) = f.link_stack.pop() {
                        let text = entry.text.trim();
                        let url = entry.url.trim();
                        let formatted = if text.is_empty() || text == url {
                            url.to_string()
                        } else if url.is_empty() {
                            text.to_string()
                        } else {
                            format!("{text} ({url})")
                        };
                        f.push(&formatted);
                    }
                }
                TagEnd::Image => {
                    if let Some(entry) = f.link_stack.pop() {
                        let alt = entry.text.trim();
                        let url = entry.url.trim();
                        let formatted = match (alt.is_empty(), url.is_empty()) {
                            (false, false) => format!("[图片: {alt} ({url})]"),
                            (false, true) => format!("[图片: {alt}]"),
                            (true, false) => format!("[图片: {url}]"),
                            (true, true) => "[图片]".to_string(),
                        };
                        f.push(&formatted);
                    }
                }
                _ => {}
            },
            MdEvent::Text(t) => {
                if f.in_code_block {
                    f.out.push_str(&t);
                } else {
                    f.item_just_started = false;
                    f.push(&t);
                }
            }
            MdEvent::Code(c) => {
                f.item_just_started = false;
                f.push(&format!("`{c}`"));
            }
            MdEvent::SoftBreak => {
                if f.in_code_block {
                    f.out.push('\n');
                } else if f.quote_depth > 0 {
                    f.out.push_str("\n> ");
                } else {
                    f.out.push('\n');
                }
            }
            MdEvent::HardBreak => {
                if f.quote_depth > 0 {
                    f.out.push_str("\n> ");
                } else {
                    f.out.push('\n');
                }
            }
            MdEvent::Rule => {
                f.ensure_blank_line();
                f.out.push_str("────────\n\n");
            }
            MdEvent::TaskListMarker(checked) => {
                let marker = if checked { "[x] " } else { "[ ] " };
                f.push(marker);
            }
            _ => {}
        }
    }

    f.out.trim().to_string()
}
