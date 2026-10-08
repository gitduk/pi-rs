//! Blocks of an answer drawn for the width they are shown at — a closed
//! ```` ```mermaid ```` block, a table — or, too wide for it, shown as source.

use std::borrow::Cow;

use unicode_width::UnicodeWidthChar;

/// A stretch of an answer: markdown, or a block drawn per width.
pub enum Piece<'a> {
    Text(&'a str),
    Block(Form, &'a str),
}

/// What a block is: a mermaid block's source (fences off), or a table's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Mermaid,
    Table,
}

/// `text` cut at its closed mermaid blocks and its tables. An unclosed
/// mermaid block (still streaming) stays text, as does all of other code.
pub fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut lines = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        lines.push((at, line));
        at += line.len();
    }
    let end = |k: usize| lines[k].0 + lines[k].1.len();
    let mut out = Vec::new();
    let mut start = 0;
    let mut code = false;
    let mut i = 0;
    while i < lines.len() {
        let (at, line) = lines[i];
        let t = line.trim();
        if code || (t.starts_with("```") && t != "```mermaid") {
            code = code != t.starts_with("```");
            i += 1;
            continue;
        }
        let block = if t == "```mermaid" {
            let Some(close) = (i + 1..lines.len()).find(|&k| lines[k].1.trim() == "```") else {
                break;
            };
            Some((Form::Mermaid, &text[end(i)..lines[close].0], close + 1))
        } else if t.starts_with('|') && lines.get(i + 1).is_some_and(|(_, l)| delimiter(l)) {
            let last = (i + 2..lines.len())
                .take_while(|&k| lines[k].1.trim().starts_with('|'))
                .last()
                .unwrap_or(i + 1);
            Some((Form::Table, &text[at..end(last)], last + 1))
        } else {
            None
        };
        match block {
            Some((form, source, next)) => {
                if at > start {
                    out.push(Piece::Text(&text[start..at]));
                }
                out.push(Piece::Block(form, source));
                start = lines.get(next).map_or(text.len(), |(at, _)| *at);
                i = next;
            }
            None => i += 1,
        }
    }
    if start < text.len() {
        out.push(Piece::Text(&text[start..]));
    }
    out
}

// A table's second line: `|---|:--:|`, each cell dashes with optional colons.
fn delimiter(line: &str) -> bool {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.contains('-')
        && t.split('|').all(|cell| {
            let c = cell.trim();
            let c = c.strip_prefix(':').unwrap_or(c);
            let c = c.strip_suffix(':').unwrap_or(c);
            !c.is_empty() && c.chars().all(|ch| ch == '-')
        })
}

/// `text` with each closed mermaid block drawn as a code block when it fits
/// `width`; one too wide stays source, its fence saying what it needs.
pub fn drawn(text: &str, width: Option<usize>) -> Cow<'_, str> {
    if !text.contains("```mermaid") {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for piece in pieces(text) {
        match piece {
            Piece::Text(t) | Piece::Block(Form::Table, t) => out.push_str(t),
            Piece::Block(Form::Mermaid, source) => match draw(source, width) {
                Drawn::Fits(diagram) => out.push_str(&format!("```mermaid\n{diagram}\n```\n")),
                Drawn::Wide { needs, has } => {
                    out.push_str(&format!("```mermaid {}\n{source}```\n", note(needs, has)))
                }
                Drawn::Unread => out.push_str(&format!("```mermaid\n{source}```\n")),
            },
        }
    }
    Cow::Owned(out)
}

/// What a mermaid source comes to at a width.
pub enum Drawn {
    Fits(String),
    Wide { needs: usize, has: usize },
    Unread,
}

/// Said beside a diagram too wide to draw.
pub fn note(needs: usize, has: usize) -> String {
    format!("needs {needs} columns to draw, {has} here")
}

pub fn draw(source: &str, width: Option<usize>) -> Drawn {
    let Ok(raw) = mermaid_text::render_with_width(source, width) else {
        return Drawn::Unread;
    };
    let drawn = squeeze(raw.trim_end())
        // A quoted label (`A["x"]`) keeps its quotes in the box; a space each
        // keeps the box's width.
        .replace('"', " ");
    let needs = drawn
        .lines()
        .map(unicode_width::UnicodeWidthStr::width)
        .max()
        .unwrap_or(0);
    match width {
        Some(has) if needs > has => Drawn::Wide { needs, has },
        _ => Drawn::Fits(drawn),
    }
}

// mermaid-text writes a wide char's second cell as a char of its own (a space,
// or a border's `─`): one column too many. Recheck this on upgrading.
fn squeeze(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut filler = false;
    for c in s.chars() {
        if filler && c != '\n' {
            filler = false;
            continue;
        }
        filler = c.width() == Some(2);
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    // `squeeze` is only right while mermaid-text pads wide chars; once it
    // stops, this fails and `squeeze` has to go.
    #[test]
    fn mermaid_text_still_pads_wide_chars() {
        let raw = mermaid_text::render("graph LR; A[开始] --> B[x]").unwrap();
        assert!(raw.contains("开 始"), "{raw}");
        let drawn = super::drawn("```mermaid\ngraph LR; A[开始] --> B[x]\n```\n", None);
        assert!(drawn.contains("│ 开始 │"), "{drawn}");
        // In a border the second cell is the border's own char.
        let titled = "flowchart TB\n  subgraph S[入口]\n    a\n  end\n";
        let raw = mermaid_text::render(titled).unwrap();
        assert!(raw.contains("入─口"), "{raw}");
        let block = format!("```mermaid\n{titled}```\n");
        let drawn = super::drawn(&block, None);
        assert!(drawn.contains("入口"), "{drawn}");
    }
}
