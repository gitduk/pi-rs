//! Syntax colours and change backgrounds over the tool's numbered diff.

use super::Paint;
use crate::markdown::Code;
use pi_store::text::{clip, escaping};
use pi_store::theme::Syntax;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::ops::Range;

#[derive(Clone, Copy)]
struct Source<'a> {
    head: &'a str,
    body: &'a str,
    mark: char,
}

struct Row<'a> {
    text: &'a str,
    source: Option<Source<'a>>,
    change: Option<Range<usize>>,
}

impl<'a> Row<'a> {
    fn parse(text: &'a str) -> Self {
        Self {
            text,
            source: Source::of(text),
            change: None,
        }
    }

    fn mark(&self) -> char {
        self.source.map_or(' ', |source| source.mark)
    }
}

impl<'a> Source<'a> {
    // The head is the row's own text before what it holds, so a clipped row
    // still separates its number from its code.
    fn of(text: &'a str) -> Option<Self> {
        let tool::row::Kind::Line {
            sign, text: body, ..
        } = tool::row::kind(text)
        else {
            return None;
        };
        Some(Self {
            head: &text[..text.len() - body.len()],
            body,
            mark: sign.mark(),
        })
    }
}

/// Terminal escapes are display noise, not source token offsets.
fn source_text(text: &str) -> String {
    let mut escape = None;
    text.chars()
        .filter(|&c| !escaping(&mut escape, c))
        .collect()
}

pub(super) fn content(
    text: &[&str],
    p: &Paint,
    room: usize,
    emphasize_changes: bool,
) -> Vec<Line<'static>> {
    let text: Vec<_> = text.iter().map(|line| source_text(line)).collect();
    if !p.color {
        return text.iter().map(|row| Line::from(clip(row, room))).collect();
    }
    let mut rows: Vec<_> = text.iter().map(|text| Row::parse(text)).collect();
    if emphasize_changes {
        replacements(&mut rows);
    }
    // Each file a block names re-reads the syntax: a hunk of another file
    // cannot continue this one's parser state.
    let mut path = String::new();
    let mut streams = Streams::new(&path, p.theme.syntax);
    rows.iter()
        .map(|row| {
            if let tool::row::Kind::File {
                path: named,
                plus,
                minus,
            } = tool::row::kind(row.text)
            {
                path = named.to_string();
                streams = Streams::new(&path, p.theme.syntax);
                let head = format!("{named} +{plus} -{minus}");
                return Line::from(Span::styled(
                    clip(&head, room),
                    super::style_to_ratatui(&p.theme.muted),
                ));
            }
            let tokens = streams.line(row.source, &path, p.theme.syntax);
            draw(row, tokens, p, room)
        })
        .collect()
}

#[cfg(test)]
pub(super) fn rows(
    text: &[&str],
    p: &Paint,
    room: usize,
    emphasize_changes: bool,
) -> Vec<Line<'static>> {
    content(text, p, room, emphasize_changes)
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::raw(super::RESULT_INDENT)];
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        })
        .collect()
}

struct Streams {
    old: Code,
    new: Code,
}

impl Streams {
    fn new(path: &str, syntax: Syntax) -> Self {
        Self {
            old: Code::new(path, syntax),
            new: Code::new(path, syntax),
        }
    }

    fn line(
        &mut self,
        source: Option<Source<'_>>,
        path: &str,
        syntax: Syntax,
    ) -> Vec<Span<'static>> {
        let Some(source) = source else {
            // Omitted context cannot carry a reliable multiline parser state.
            *self = Self::new(path, syntax);
            return Vec::new();
        };
        match source.mark {
            '-' => self.old.line(source.body),
            '+' => self.new.line(source.body),
            _ => {
                self.old.line(source.body);
                self.new.line(source.body)
            }
        }
    }
}

fn single_replacement(rows: &[Row<'_>], i: usize) -> bool {
    rows[i].mark() == '-'
        && rows[i + 1].mark() == '+'
        && (i == 0 || rows[i - 1].mark() != '-')
        && (i + 2 == rows.len() || rows[i + 2].mark() != '+')
}

fn replacements(rows: &mut [Row<'_>]) {
    for i in 0..rows.len().saturating_sub(1) {
        if single_replacement(rows, i) {
            let was = rows[i].source.expect("numbered deletion");
            let is = rows[i + 1].source.expect("numbered addition");
            let (was, is) = diverged(was.body, is.body);
            rows[i].change = Some(was);
            rows[i + 1].change = Some(is);
        }
    }
}

fn background(added: bool, strong: bool, p: &Paint) -> Color {
    let base = p
        .background
        .unwrap_or_else(|| crate::markdown::background(p.theme.syntax));
    let tint = if added {
        (50, 180, 105)
    } else {
        (229, 115, 115)
    };
    let weight = if strong { 32 } else { 12 };
    let (r, g, b) = crate::theme::blend(tint, base, weight);
    Color::Rgb(r, g, b)
}

fn draw(row: &Row<'_>, tokens: Vec<Span<'static>>, p: &Paint, room: usize) -> Line<'static> {
    let visible = clip(row.text, room);
    let muted = super::style_to_ratatui(&p.theme.muted);
    let Some(source) = row.source else {
        return Line::from(vec![Span::styled(visible, muted)]);
    };
    let base = match source.mark {
        '+' | '-' => Style::new().bg(background(source.mark == '+', false, p)),
        _ => Style::default(),
    };
    let marker = match source.mark {
        '+' => super::style_to_ratatui(&p.theme.diff.add),
        '-' => super::style_to_ratatui(&p.theme.diff.del),
        _ => muted,
    };
    if !visible.starts_with(source.head) {
        return Line::from(vec![Span::styled(visible, base.patch(marker))]);
    }
    let body = &visible[source.head.len()..];
    let cut = visible != row.text;
    let end = if cut {
        body.strip_suffix('…')
            .expect("clipped source has an ellipsis")
            .len()
    } else {
        body.len()
    };
    let mut spans = vec![Span::styled(source.head.to_string(), marker)];
    spans.extend(body_spans(
        tokens,
        end,
        row.change.as_ref(),
        background(source.mark == '+', true, p),
    ));
    if cut {
        spans.push(Span::raw("…"));
    }
    for span in &mut spans {
        span.style = base.patch(span.style);
    }
    Line::from(spans)
}

fn body_spans(
    tokens: Vec<Span<'static>>,
    end: usize,
    change: Option<&Range<usize>>,
    changed: Color,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for token in tokens {
        let stop = (offset + token.content.len()).min(end);
        if stop <= offset {
            break;
        }
        let mut edges = vec![offset, stop];
        if let Some(range) = change {
            edges.extend(
                [range.start, range.end]
                    .into_iter()
                    .filter(|&at| at > offset && at < stop),
            );
        }
        edges.sort_unstable();
        edges.dedup();
        for edge in edges.windows(2) {
            let style = match change.is_some_and(|range| range.contains(&edge[0])) {
                true => token.style.bg(changed),
                false => token.style,
            };
            spans.push(Span::styled(
                token.content[edge[0] - offset..edge[1] - offset].to_string(),
                style,
            ));
        }
        offset += token.content.len();
    }
    spans
}

/// The run two rows do not share, as byte ranges into each: what is left of
/// them once the head and the tail they have in common are set aside.
fn diverged(was: &str, is: &str) -> (Range<usize>, Range<usize>) {
    let head = was
        .chars()
        .zip(is.chars())
        .take_while(|(a, b)| a == b)
        .count();
    // Cut the head off first and count the tail in what is left: the two runs
    // then cannot overlap, however alike the rows are.
    let at = |s: &str| s.char_indices().nth(head).map_or(s.len(), |(i, _)| i);
    let (was_at, is_at) = (at(was), at(is));
    let (was_rest, is_rest) = (&was[was_at..], &is[is_at..]);
    let tail = was_rest
        .chars()
        .rev()
        .zip(is_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let cut = |rest: &str| match tail {
        0 => rest.len(),
        n => {
            rest.char_indices()
                .rev()
                .nth(n - 1)
                .expect("counted from here")
                .0
        }
    };
    (was_at..was_at + cut(was_rest), is_at..is_at + cut(is_rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_store::theme::Syntax;

    #[test]
    fn escaped_source_is_clipped_without_slicing_changed_text() {
        let p = Paint::new(true);
        for room in 0..50 {
            let lines = rows(&["+1 \x1b[31mhello     你好世界中文"], &p, room, true);
            assert_eq!(
                lines[0].to_string(),
                format!("  {}", clip("+1 hello     你好世界中文", room))
            );
        }
    }

    fn style_at(line: &Line<'_>, needle: &str) -> Style {
        let at = line.to_string().find(needle).unwrap();
        let mut offset = 0;
        for span in &line.spans {
            if (offset..offset + span.content.len()).contains(&at) {
                return line.style.patch(span.style);
            }
            offset += span.content.len();
        }
        panic!("missing span for {needle}");
    }

    #[test]
    fn backgrounds_match_b_and_ignore_the_input_band() {
        use pi_store::theme::Color as Config;
        for input in [
            Config::Rgb(20, 30, 40),
            Config::Indexed(17),
            Config::Basic(44),
        ] {
            let mut theme = pi_store::theme::Theme::default();
            theme.prompt.panel.input = input;
            let p = Paint::with_theme(true, std::sync::Arc::new(theme))
                .with_background(Some((24, 24, 24)));
            let lines = rows(&["-1 value = 123", "+1 value = 456"], &p, 80, true);
            for (line, normal, strong, number) in [
                (
                    &lines[0],
                    Color::Rgb(48, 34, 34),
                    Color::Rgb(89, 53, 53),
                    "123",
                ),
                (
                    &lines[1],
                    Color::Rgb(27, 42, 33),
                    Color::Rgb(32, 73, 49),
                    "456",
                ),
            ] {
                assert_eq!(line.style.bg, None);
                assert_eq!(line.spans[0].style.bg, None);
                assert_eq!(style_at(line, "value").bg, Some(normal));
                assert_eq!(style_at(line, number).bg, Some(strong));
            }
        }
    }

    #[test]
    fn changes_keep_syntax_colours_on_a_row_and_on_its_page() {
        use ratatui::style::Modifier;
        let p = Paint::new(true);
        let preview = "sample.py +1 -1\n-1 value = foo('old')\n+1 value = bar(123)";
        let call = super::super::Finished {
            outcome: super::super::Outcome::Done,
            name: "write",
            preview,
            place: None,
            took: None,
        };
        let lines = super::super::result_rows(&call, super::super::Shown::Open, false, &p, 80);
        let page = super::super::result_rows(&call, super::super::Shown::Page, false, &p, 80);
        for (inline, page) in lines[1..].iter().zip(&page[1..]) {
            assert_eq!(inline.to_string(), page.to_string());
            for needle in [
                "value",
                if inline.to_string().contains("old") {
                    "old"
                } else {
                    "123"
                },
            ] {
                assert_eq!(style_at(inline, needle).fg, style_at(page, needle).fg);
            }
            // The page reads what the row showed: the change keeps its ground.
            for (inline, page) in inline.spans.iter().zip(&page.spans) {
                assert_eq!(inline.style.bg, page.style.bg);
            }
            assert!(
                inline
                    .spans
                    .iter()
                    .all(|span| !span.style.add_modifier.contains(Modifier::REVERSED))
            );
        }
    }

    #[test]
    fn both_background_levels_follow_the_terminal_in_dark_and_light_palettes() {
        for (syntax, terminal) in [
            (Syntax::CatppuccinMocha, (24, 24, 24)),
            (Syntax::CatppuccinLatte, (245, 245, 245)),
        ] {
            let theme = pi_store::theme::Theme {
                syntax,
                ..Default::default()
            };
            let p =
                Paint::with_theme(true, std::sync::Arc::new(theme)).with_background(Some(terminal));
            let shifted = Paint::with_theme(true, p.theme.clone()).with_background(Some((
                terminal.0 + 5,
                terminal.1,
                terminal.2,
            )));
            for added in [false, true] {
                for strong in [false, true] {
                    let (Color::Rgb(r, g, b), Color::Rgb(sr, sg, sb)) = (
                        background(added, strong, &p),
                        background(added, strong, &shifted),
                    ) else {
                        panic!("diff background is RGB");
                    };
                    assert!(sr > r);
                    assert_eq!((g, b), (sg, sb));
                }
            }
        }
        for syntax in [Syntax::CatppuccinMocha, Syntax::CatppuccinLatte] {
            let theme = pi_store::theme::Theme {
                syntax,
                ..Default::default()
            };
            let unknown = Paint::with_theme(true, std::sync::Arc::new(theme));
            let fallback = Paint::with_theme(true, unknown.theme.clone())
                .with_background(Some(crate::markdown::background(syntax)));
            assert_eq!(
                background(true, false, &unknown),
                background(true, false, &fallback)
            );
        }
    }

    #[test]
    fn multiline_state_is_separate_for_each_side_and_resets_at_gaps() {
        let p = Paint::new(true);
        let lines = rows(
            &[
                "sample.py +2 -2",
                "-1 text = '''",
                "+1 text = 1",
                " 2 import os",
                "-3 '''",
                "+3 print('hi')",
                "  ⋮",
                " 9 import os",
            ],
            &p,
            80,
            false,
        );
        // The block's head names the file; the rows it names follow it.
        let lines = &lines[1..];
        let mut code = Code::new("sample.py", p.theme.syntax);
        let expected = code.line("import os");
        let import = expected
            .iter()
            .find(|s| s.content.contains("import"))
            .unwrap()
            .style
            .fg;
        assert_eq!(style_at(&lines[2], "import").fg, import);
        assert_eq!(style_at(&lines[6], "import").fg, import);
        // The old side is still inside a string when its closing quote arrives.
        let mut old = Code::new("sample.py", p.theme.syntax);
        old.line("text = '''");
        old.line("import os");
        assert_eq!(style_at(&lines[3], "'''").fg, old.line("'''")[0].style.fg);
    }

    // A command's own line that reads like a diff row keeps its reading: only
    // a row a tool wrote carries a sign.
    #[test]
    fn output_that_looks_like_a_diff_row_is_drawn_as_output() {
        let p = Paint::new(true);
        let printed = rows(&["· +3 import os"], &p, 80, true);
        assert_eq!(printed[0].to_string(), "  · +3 import os");
        for span in &printed[0].spans {
            assert_eq!(span.style.bg, None, "{:?}", span.style);
        }

        let row = rows(&["+3 import os"], &p, 80, true);
        assert!(
            row[0].spans.iter().any(|span| span.style.bg.is_some()),
            "a row of a file is a change"
        );
    }

    #[test]
    fn colouring_preserves_clipping_unicode_and_unknown_source() {
        let source = [
            "- 9 名字 = '旧值'",
            "+10 名字 = '新值'",
            "+11 ",
            "  ⋮",
            "unstructured output",
        ];
        for path in ["sample.py", "sample.unknown"] {
            let head = format!("{path} +2 -2");
            let block: Vec<&str> = std::iter::once(head.as_str()).chain(source).collect();
            for room in [0, 1, 3, 6, 12, 80] {
                let plain = rows(&block, &Paint::new(false), room, true);
                let coloured = rows(&block, &Paint::new(true), room, true);
                for ((plain, coloured), original) in plain.iter().zip(&coloured).zip(&block) {
                    assert_eq!(plain.to_string(), format!("  {}", clip(original, room)));
                    assert_eq!(plain.to_string(), coloured.to_string());
                }
            }
        }
    }
}
