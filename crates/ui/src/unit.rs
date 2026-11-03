//! The stretches of a rendered answer that read as one — inline code, a
//! link's text, its destination — so a double click takes each whole.
//!
//! Inline stretches are marked in the source by a pair of private-use chars,
//! then recovered from the rendered lines as the marks are taken out.
//! Tables attach their cell ranges during layout.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd, TextMergeWithOffset};
use ratatui::text::{Line, Span};

/// The cell ranges of a line that read as one.
pub type Units = Vec<Range<usize>>;

/// One displayed fragment and the complete cell it copies.
#[derive(Debug, Clone)]
pub struct CellFragment {
    pub id: usize,
    pub range: Range<usize>,
    pub text: std::sync::Arc<str>,
}

// Private-use chars no model writes, as the code fence's stand-in is.
const OPEN: char = '\u{E001}';
const CLOSE: char = '\u{E002}';

/// `text` with every stretch that reads as one between `OPEN` and `CLOSE`;
/// tables supply their own cell ranges after layout.
pub fn mark(text: &str) -> Cow<'_, str> {
    // Skip parsing when there is no code or link to mark. Text holding the
    // marks already (an icon font's glyph) is left as is.
    if !text.contains('`') && !text.contains("](") && !text.contains("://") && !text.contains('@')
        || text.contains([OPEN, CLOSE])
    {
        return Cow::Borrowed(text);
    }
    let mut at: Vec<(usize, char)> = Vec::new();
    let mut table = false;
    let mut code = false;
    let mut image = false;
    let mut metadata = false;
    // The link open, if one is (links never nest): whether it is inline,
    // and the bytes its text spans.
    let mut link: Option<(bool, Option<Range<usize>>)> = None;
    for (event, range) in
        TextMergeWithOffset::new(Parser::new_ext(text, options()).into_offset_iter())
    {
        let edge = matches!(
            event,
            Event::Start(Tag::Link { .. }) | Event::End(TagEnd::Link)
        );
        if !edge && let Some((_, inner)) = &mut link {
            let r = range.clone();
            *inner = Some(
                inner
                    .take()
                    .map_or(r.clone(), |i| i.start.min(r.start)..i.end.max(r.end)),
            );
        }
        match event {
            Event::Start(Tag::Table(_)) => table = true,
            Event::End(TagEnd::Table) => table = false,
            Event::Start(Tag::CodeBlock(_)) => code = true,
            Event::End(TagEnd::CodeBlock) => code = false,
            Event::Start(Tag::Image { .. }) => image = true,
            Event::End(TagEnd::Image) => image = false,
            Event::Start(Tag::MetadataBlock(_)) => metadata = true,
            Event::End(TagEnd::MetadataBlock(_)) => metadata = false,
            _ if table => {}
            Event::Text(_) if !code && !image && !metadata && link.is_none() => {
                for found in crate::markdown::link_finder().links(&text[range.clone()]) {
                    at.extend([
                        (range.start + found.start(), OPEN),
                        (range.start + found.end(), CLOSE),
                    ]);
                }
            }
            Event::Code(_) => at.extend([(range.start, OPEN), (range.end, CLOSE)]),
            // Only an inline link's text is its own: another's names its
            // reference too, and a mark there would break the lookup.
            Event::Start(Tag::Link { link_type, .. }) if link_type != LinkType::Inline => {
                link = Some((false, None));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link = Some((true, None));
                // The destination as written, after the text's `](`.
                let tail = &text[range.clone()];
                if let Some(close) = tail.rfind("](")
                    && let Some(off) = tail[close..].find(dest_url.as_ref())
                    && !dest_url.is_empty()
                {
                    let start = range.start + close + off;
                    at.extend([(start, OPEN), (start + dest_url.len(), CLOSE)]);
                }
            }
            Event::End(TagEnd::Link) => {
                if let Some((true, Some(inner))) = link.take() {
                    at.extend([(inner.start, OPEN), (inner.end, CLOSE)]);
                }
            }
            _ => {}
        }
    }
    if at.is_empty() {
        return Cow::Borrowed(text);
    }
    // A close before an open at one place: two stretches side by side.
    at.sort_by_key(|&(pos, c)| (pos, c == OPEN));
    let mut out = String::with_capacity(text.len() + at.len() * OPEN.len_utf8());
    let mut from = 0;
    for (pos, c) in at {
        out.push_str(&text[from..pos]);
        out.push(c);
        from = pos;
    }
    out.push_str(&text[from..]);
    Cow::Owned(out)
}

// The renderer's own, so what is code here is code there.
// Math stays off: a pair of `$` around prices parses as a formula and swallows
// the prose between them.
pub(crate) fn options() -> Options {
    Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_SUPERSCRIPT
        | Options::ENABLE_SUBSCRIPT
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_GFM
        | Options::ENABLE_TABLES
}

/// The lines with the marks `mark` put in taken out, each with the cell
/// ranges they held; only for a text `mark` changed.
/// A stretch the renderer broke over lines is a stretch on each.
pub fn strip(lines: Vec<Line<'static>>) -> Vec<(Line<'static>, Units)> {
    let mut open = 0usize;
    lines
        .into_iter()
        .map(|line| {
            if open == 0
                && !line
                    .spans
                    .iter()
                    .any(|span| span.content.contains([OPEN, CLOSE]))
            {
                return (line, Vec::new());
            }
            let mut starts = vec![0; open];
            let mut ranges = Vec::new();
            let mut text = String::new();
            let spans = line
                .spans
                .into_iter()
                .filter_map(|span| {
                    if !span.content.contains([OPEN, CLOSE]) {
                        text.push_str(&span.content);
                        return Some(span);
                    }
                    let mut kept = String::new();
                    for c in span.content.chars() {
                        match c {
                            OPEN => starts.push(text.len()),
                            CLOSE => {
                                if let Some(start) = starts.pop() {
                                    ranges.push(start..text.len());
                                }
                            }
                            _ => {
                                kept.push(c);
                                text.push(c);
                            }
                        }
                    }
                    (!kept.is_empty()).then(|| Span::styled(kept, span.style))
                })
                .collect::<Vec<_>>();
            open = starts.len();
            ranges.extend(starts.into_iter().map(|start| start..text.len()));
            let units = ranges
                .into_iter()
                .filter_map(|range| {
                    let start = pi_store::text::floor_boundary(&text, range.start);
                    let end = pi_store::text::ceil_boundary(&text, range.end);
                    let from = pi_store::text::width(&text[..start]);
                    let to = pi_store::text::width(&text[..end]);
                    (from < to).then_some(from..to)
                })
                .collect();
            (Line::from(spans).style(line.style), units)
        })
        .collect()
}

/// The innermost of `units` holding cell `col`.
pub fn at(units: &[Range<usize>], col: usize) -> Option<Range<usize>> {
    (units.iter())
        .filter(|u| u.contains(&col))
        .min_by_key(|u| u.len())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Through the renderer as an answer goes, the stretches it gives back.
    fn units(text: &str) -> Vec<(String, Units)> {
        let paint = crate::render::Paint::new(true);
        crate::render::render_coded(text, &paint)
            .into_iter()
            .map(|r| {
                (
                    r.line
                        .spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>(),
                    r.units,
                )
            })
            .filter(|(text, _)| !text.is_empty())
            .collect()
    }

    fn taken(text: &str, units: &Units) -> Vec<String> {
        let cells: Vec<char> = text.chars().collect();
        units
            .iter()
            .map(|u| cells[u.clone()].iter().collect())
            .collect()
    }

    #[test]
    fn code_and_links_come_back_whole() {
        let got = units("run `ls -al` or see [the docs](http://x/y) now");
        let [(text, u)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(text, "run ls -al or see the docs (http://x/y) now");
        assert_eq!(taken(text, u), ["ls -al", "the docs", "http://x/y"]);
    }

    // The two parses agree only by keeping one option set: where they part,
    // marks show on the screen.
    #[test]
    fn no_mark_is_left_on_screen() {
        let text = "# `h` head\n\n- [x] `done` [ref][r] and [in](http://a)\n\n\
                    term\n: `def`\n\n$`m`$ and a note[^n]\n\n[^n]: `foot`\n\n\
                    [r]: http://r\n\n> `quoted` *`em`*\n";
        let got = units(text);
        assert!(
            got.iter().all(|(t, _)| !t.contains([OPEN, CLOSE])),
            "{got:?}"
        );
        let all: Vec<String> = got.iter().flat_map(|(t, u)| taken(t, u)).collect();
        for want in ["h", "done", "in", "http://a", "def", "foot", "quoted", "em"] {
            assert!(all.iter().any(|a| a == want), "{want} in {all:?}");
        }
    }

    #[test]
    fn a_glyph_like_a_mark_stays_on_screen() {
        let got = units("icon \u{E001} and `code`");
        assert_eq!(got[0].0, "icon \u{E001} and code");
        assert!(got[0].1.is_empty());
    }

    #[test]
    fn a_table_keeps_no_marks() {
        let got = units("| a | b |\n|---|---|\n| `x` | y |\n");
        assert!(got.iter().all(|(t, _)| !t.contains([OPEN, CLOSE])));
        let cells: Vec<_> = got
            .iter()
            .flat_map(|(text, ranges)| taken(text, ranges))
            .collect();
        assert_eq!(cells, ["a", "b", "x", "y"]);
    }

    #[test]
    fn backticks_inside_a_code_block_are_left_alone() {
        let got = units("```sh\necho `date`\n```\n");
        assert!(got.iter().all(|(_, u)| u.is_empty()), "{got:?}");
    }

    #[test]
    fn bare_urls_and_emails_are_whole_selection_units() {
        let got = units("See https://example.com/a_(b), or mail user@example.com.");
        let [(text, ranges)] = &got[..] else {
            panic!("{got:?}")
        };
        assert_eq!(
            taken(text, ranges),
            ["https://example.com/a_(b)", "user@example.com"]
        );
        let code = units("```text\nhttps://example.com\n```");
        assert!(code.iter().all(|(_, units)| units.is_empty()));
    }
}
