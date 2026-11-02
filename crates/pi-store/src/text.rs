//! Cutting and measuring text the terminal will show, and the wording of a
//! run's spend. Shared by every layer, so it sits under all of them.
//!
//! Columns, not characters: a line of Chinese fits half as many characters in
//! the same width, and an escape sequence occupies none.

use llm::figures::{in_out, short};
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::icons;

pub const RESET: &str = "\x1b[0m";

pub struct Escape {
    kind: Kind,
}

// What shape the sequence is, once its first character has said.
enum Kind {
    // Nothing after the `\x1b` yet.
    Start,
    // `ESC [`, `ESC O` — parameters and intermediates, then a byte in
    // 0x40-0x7e.
    Control,
    // `ESC P/X/]/^/_` (DCS/SOS/OSC/PM/APC): arbitrary text closed by BEL
    // or ST (`ESC \`), not by a byte range — content may contain those bytes.
    // `st`: the last character was `\x1b`, so a `\` now closes it.
    Str { st: bool },
    // A bare `ESC` with an intermediate byte, ending on a byte in 0x30-0x7e
    // — wider than a control sequence's, which is where `ESC 7` lives.
    Bare,
    // The first character is itself the final byte (e.g. `ESC 7`/`ESC 8`,
    // save/restore cursor) — outside Control's range, so it needs its own case.
    Done,
}

impl Escape {
    /// The state directly after an `\x1b`. Feed it every character that
    /// follows; it answers true on the one that closes the sequence.
    pub fn new() -> Self {
        Self { kind: Kind::Start }
    }

    // The state after a C1 control that opens a sequence on its own — the
    // 8-bit spelling of `ESC [`, `ESC ]` and the rest.
    fn after_c1(c: char) -> Option<Self> {
        let kind = match c {
            '\u{9b}' | '\u{8f}' => Kind::Control,
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => Kind::Str { st: false },
            _ => return None,
        };
        Some(Self { kind })
    }

    pub fn closed(&mut self, c: char) -> bool {
        match &mut self.kind {
            Kind::Start => {
                // `[` and `O` are also final bytes by the range test, so they
                // must come first to select Control instead of Done.
                self.kind = match c {
                    '[' | 'O' => Kind::Control,
                    'P' | 'X' | ']' | '^' | '_' => Kind::Str { st: false },
                    c if ('\x30'..='\x7e').contains(&c) => Kind::Done,
                    _ => Kind::Bare,
                };
                matches!(self.kind, Kind::Done)
            }
            Kind::Done => true,
            Kind::Control => ('\x40'..='\x7e').contains(&c),
            Kind::Bare => ('\x30'..='\x7e').contains(&c),
            Kind::Str { st } => {
                let closed = c == '\x07' || c == '\u{9c}' || (*st && c == '\\');
                *st = c == '\x1b';
                closed
            }
        }
    }
}

impl Default for Escape {
    fn default() -> Self {
        Self::new()
    }
}

/// The shared wording for a spend, used for both a session's totals and a
/// run's. Cost is omitted (not shown as $0) when the model is unpriced.
pub fn spent(t: &llm::Totals) -> String {
    let mut parts = vec![in_out(t.usage.input, t.usage.output)];
    if t.usage.cache_read > 0 {
        parts.push(format!("{} cached", short(t.usage.cache_read)));
    }
    if t.cost > 0.0 {
        parts.push(llm::figures::dollars(t.cost));
    }
    parts.join(icons::PART_SEP)
}

/// How many times something came up, as every count beside it is written.
pub fn times(n: usize) -> String {
    format!("×{n}")
}

/// A file's size, in the largest unit that keeps it above one.
pub fn size(bytes: u64) -> String {
    match bytes {
        0..1_024 => format!("{bytes} B"),
        1_024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1_024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

/// Columns between tab stops, as terminals set them.
pub const TAB_STOP: usize = 8;

/// Columns of one scalar. Use `grapheme_width` for displayed text.
pub fn cols_at(c: char, col: usize) -> usize {
    match c {
        '\t' => TAB_STOP - col % TAB_STOP,
        c => unicode_width::UnicodeWidthChar::width(c).unwrap_or(0),
    }
}

/// Extended graphemes and their byte offsets in the original string.
pub fn graphemes(s: &str) -> impl DoubleEndedIterator<Item = (usize, &str)> + Clone {
    s.grapheme_indices(true)
}

pub fn grapheme_width(s: &str, col: usize) -> usize {
    match s {
        "\t" => TAB_STOP - col % TAB_STOP,
        _ if s.chars().all(char::is_control) => 0,
        _ => UnicodeWidthStr::width(s),
    }
}

pub fn floor_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    graphemes(s)
        .map(|(i, _)| i)
        .take_while(|&i| i <= at)
        .last()
        .unwrap_or(0)
}

pub fn ceil_boundary(s: &str, at: usize) -> usize {
    graphemes(s)
        .map(|(i, _)| i)
        .find(|&i| i >= at)
        .unwrap_or(s.len())
}

// Strip escape sequences before grouping: a style change can fall inside
// one grapheme. Keep the source offsets so clipping preserves complete ANSI.
struct DisplayText<'a> {
    text: Cow<'a, str>,
    source: Vec<(usize, usize)>,
}

impl<'a> DisplayText<'a> {
    fn new(s: &'a str) -> Self {
        if !s
            .chars()
            .any(|c| c == '\x1b' || Escape::after_c1(c).is_some())
        {
            return Self {
                text: Cow::Borrowed(s),
                source: Vec::new(),
            };
        }
        let mut text = String::new();
        let mut source = Vec::new();
        let mut esc = None;
        for (i, c) in s.char_indices() {
            if !escaping(&mut esc, c) {
                source.push((text.len(), i));
                text.push(c);
            }
        }
        source.push((text.len(), s.len()));
        Self {
            text: Cow::Owned(text),
            source,
        }
    }

    fn source_at(&self, at: usize) -> usize {
        if self.source.is_empty() {
            return at;
        }
        let i = self.source.partition_point(|&(visible, _)| visible < at);
        self.source[i].1
    }
}

/// Byte offset of the grapheme covering `col`, or the end of the text.
pub fn byte_at_col(s: &str, col: usize) -> usize {
    let shown = DisplayText::new(s);
    let mut used = 0;
    for (at, g) in graphemes(&shown.text) {
        let w = grapheme_width(g, used);
        if used + w > col {
            return shown.source_at(at);
        }
        used += w;
    }
    s.len()
}

/// Whether `c` belongs to an escape sequence, opening or inside one; `esc`
/// carries the one open between calls.
pub fn escaping(esc: &mut Option<Escape>, c: char) -> bool {
    if let Some(open) = esc {
        if open.closed(c) {
            *esc = None;
        }
        return true;
    }
    if c == '\x1b' {
        *esc = Some(Escape::new());
        return true;
    }
    *esc = Escape::after_c1(c);
    esc.is_some()
}

/// `s` with every run of whitespace, newlines included, one space.
pub fn collapsed(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Display columns of one line, with escapes omitted and graphemes kept whole.
pub fn width(s: &str) -> usize {
    let shown = DisplayText::new(s);
    graphemes(&shown.text).fold(0, |col, (_, g)| col + grapheme_width(g, col))
}

/// `s` with each tab spelled as the spaces it runs to; a `\n` starts over.
pub fn expand_tabs(s: &str) -> String {
    let shown = DisplayText::new(s);
    let (mut from, mut col) = (0, 0);
    let mut out = String::with_capacity(s.len());
    for (at, g) in graphemes(&shown.text) {
        let start = shown.source_at(at);
        // A source boundary includes any escapes before the next grapheme.
        let end = shown.source_at(at + g.len());
        out.push_str(&s[from..start]);
        let w = grapheme_width(g, col);
        if g == "\t" {
            out.extend(std::iter::repeat_n(' ', w));
            out.push_str(&s[start + 1..end]);
        } else {
            out.push_str(&s[start..end]);
        }
        col = if g.contains('\n') { 0 } else { col + w };
        from = end;
    }
    out.push_str(&s[from..]);
    out
}

/// One line in at most `max` display columns (see module doc), the `…` that
/// marks a cut counted among them. A cut inside an escape's reach closes it.
pub fn clip(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    if width(&one) <= max {
        return one;
    }
    if max == 0 {
        return String::new();
    }
    let shown = DisplayText::new(&one);
    let mut used = 0;
    for (i, g) in graphemes(&shown.text) {
        used += grapheme_width(g, used);
        if used >= max {
            return marked(&one[..shown.source_at(i)]);
        }
    }
    one
}

/// `clip`, with the `…` also owed when `more` follows a line that fits whole:
/// the mark sits on the text, never apart from it.
pub fn clip_more(s: &str, max: usize, more: bool) -> String {
    let cut = clip(s, max);
    if !more || cut != s {
        return cut;
    }
    match width(s) < max {
        true => marked(s),
        false => clip(s, max.saturating_sub(1)),
    }
}

// `s` closed by `…`, trailing spaces trimmed but not the escapes among them:
// a style set there still has to reach its reset.
fn marked(s: &str) -> String {
    // `end`: just past the last visible non-space, where no escape is open.
    let (mut esc, mut styled, mut end) = (None, false, 0);
    for (i, c) in s.char_indices() {
        if escaping(&mut esc, c) {
            styled = true;
        } else if !c.is_whitespace() {
            end = i + c.len_utf8();
        }
    }
    let mut esc = None;
    let after: String = s[end..]
        .chars()
        .filter(|&c| escaping(&mut esc, c))
        .collect();
    let reset = if styled { RESET } else { "" };
    format!("{}{after}{reset}{}", &s[..end], icons::ELLIPSIS)
}

/// Plain `s` from at most `keep` columns before byte `at` on, the `…` that
/// marks a cut counted among them. `at` must lie on a char boundary.
pub fn clip_left(s: &str, at: usize, keep: usize) -> String {
    let shown = DisplayText::new(s);
    let at = graphemes(&shown.text)
        .map(|(i, _)| shown.source_at(i))
        .chain([s.len()])
        .take_while(|&i| i <= at)
        .last()
        .unwrap_or(0);
    let before = &s[..at];
    if width(before) <= keep {
        return s.to_string();
    }
    let mut used = width(icons::ELLIPSIS);
    let mut from = at;
    for (i, g) in graphemes(&shown.text)
        .rev()
        .filter(|(i, _)| shown.source_at(*i) < at)
    {
        used += grapheme_width(g, 0);
        if used > keep {
            break;
        }
        from = shown.source_at(i);
    }
    format!("{}{}", icons::ELLIPSIS, &s[from..])
}

/// Where `query` is in `text`, as byte ranges: the one way a typed query is
/// looked for. Smart case: a capital in the query asks for an exact match;
/// otherwise ASCII is folded, which keeps the byte offsets. Empty finds none.
pub fn found(text: &str, query: &str) -> Vec<std::ops::Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let exact = query.chars().any(char::is_uppercase);
    let (hay, needle) = if exact {
        (text.to_string(), query.to_string())
    } else {
        (text.to_ascii_lowercase(), query.to_ascii_lowercase())
    };
    let boundaries: Vec<_> = graphemes(text)
        .map(|(at, _)| at)
        .chain([text.len()])
        .collect();
    let mut found: Vec<_> = hay
        .match_indices(&needle)
        .map(|(b, m)| {
            let start = boundaries.partition_point(|&at| at <= b) - 1;
            let end = boundaries.partition_point(|&at| at < b + m.len());
            boundaries[start]..boundaries[end]
        })
        .collect();
    found.dedup();
    found
}

/// Whether a list filtered by `query` keeps `text`: all of it while nothing
/// is typed, then what [`found`] finds.
pub fn matches(text: &str, query: &str) -> bool {
    query.is_empty() || !found(text, query).is_empty()
}

/// The last line of `s` with anything on it, as a terminal leaves it: what
/// follows its last `\r`, escapes and other controls dropped.
pub fn last_line(s: &str) -> String {
    (s.lines().rev())
        .flat_map(|line| line.rsplit('\r'))
        .map(bare)
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .unwrap_or_default()
}

/// A printed line as a terminal leaves it: what follows its last `\r` that
/// has anything, tabs as spaces, escapes and other controls dropped.
pub fn screen_line(line: &str) -> String {
    let shown = (line.rsplit('\r'))
        .find(|part| !part.is_empty())
        .unwrap_or_default();
    bare(&expand_tabs(shown))
}

fn bare(s: &str) -> String {
    let mut esc = None;
    (s.chars())
        .filter(|&c| !escaping(&mut esc, c) && !c.is_control())
        .collect()
}

/// The widest of `cells` as `pad` measures them: the column it lines them up to.
pub fn widest<'a>(cells: impl IntoIterator<Item = &'a str>) -> usize {
    cells.into_iter().map(width).max().unwrap_or(0)
}

/// Pad a string to a display width with trailing spaces, so a column of
/// mixed-width (CJK) text lines up where `{:width$}` would only count chars.
pub fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(self::width(s))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_folds_case_unless_a_capital_asks() {
        assert_eq!(found("中文 Foo foo", "foo"), vec![7..10, 11..14]);
        assert_eq!(found("中文 Foo foo", "Foo"), vec![7..10]);
        assert!(found("Foo", "").is_empty());
        assert!(matches("Foo", ""));
    }

    // The mark a cut leaves is counted: callers lay out to `max`, not past it.
    #[test]
    fn a_clip_fits_its_columns_mark_and_all() {
        assert_eq!(clip("abcdef", 6), "abcdef");
        assert_eq!(clip("abcdefg", 6), "abcde…");
        assert_eq!(width(&clip("中文中文中文", 5)), 5);
        assert_eq!(clip("\x1b[31mabcdefg", 6), "\x1b[31mabcde\x1b[0m…");
        // A space an escape follows is still trailing on screen.
        assert_eq!(clip("hi \x1b[1mXYZ", 4), "hi\x1b[1m\x1b[0m…");
    }

    // Measured by the rule `pad` fills by, or the column it pads to is off.
    #[test]
    fn widest_measures_as_pad_fills() {
        assert_eq!(widest(["ab", "中文", "\x1b[1mxyz"]), 4);
        assert_eq!(widest([]), 0);
        assert_eq!(width(&pad("\x1b[1mx", widest(["中文"]))), 4);
    }

    // A left cut lands on visible text, never inside an escape.
    #[test]
    fn a_left_clip_steps_over_escapes() {
        let s = "AAAAA\x1b[31mBBBBB";
        assert_eq!(clip_left(s, s.len(), 8), "…AA\x1b[31mBBBBB");
        assert_eq!(clip_left("abcdef", 6, 4), "…def");
    }

    // A C1 control opens a sequence as its `ESC` spelling does.
    #[test]
    fn a_c1_sequence_is_an_escape() {
        assert_eq!(width("\u{9b}31mab"), 2);
        assert_eq!(screen_line("\u{9b}31mred\u{9b}0m"), "red");
        assert_eq!(screen_line("\u{9d}8;;http://x\u{9c}link"), "link");
        assert_eq!(screen_line("\x1b]8;;http://x\u{9c}link"), "link");
        assert_eq!(clip("\u{9b}1mabcdefg", 4), "\u{9b}1mabc\x1b[0m…");
    }

    // Whole text with more after it takes the same mark a cut would.
    #[test]
    fn more_marks_whole_text_as_a_cut_does() {
        assert_eq!(clip_more("abc", 6, false), "abc");
        assert_eq!(clip_more("abc ", 6, true), "abc…");
        assert_eq!(clip_more("done \x1b[32m", 8, true), "done\x1b[32m\x1b[0m…");
        assert_eq!(clip_more("abcdef", 6, true), "abcd…");
        assert_eq!(clip_more("abcdefg", 6, true), "abcde…");
    }

    // One tab rule for every surface: to the next stop, from where it starts.
    #[test]
    fn a_tab_runs_to_its_stop() {
        assert_eq!(width("\tx"), TAB_STOP + 1);
        assert_eq!(width("ab\tx"), TAB_STOP + 1);
        assert_eq!(
            expand_tabs("ab\tx\n\ty"),
            format!("ab{}x\n{}y", " ".repeat(6), " ".repeat(8))
        );
        assert_eq!(width("\x1b[31m\tx"), TAB_STOP + 1);
    }
}
