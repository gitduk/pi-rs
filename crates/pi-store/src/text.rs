//! Cutting and measuring text the terminal will show, and the wording of a
//! run's spend. Shared by every layer, so it sits under all of them.
//!
//! Columns, not characters: a line of Chinese fits half as many characters in
//! the same width, and an escape sequence occupies none.

use llm::figures::{in_out, short};

use crate::icons;

pub const RESET: &str = "\x1b[0m";

pub struct Escape {
    // What shape it is, once the first character has said. `None` until then.
    kind: Option<Kind>,
    // Inside a control string: the last character was `\x1b`, so a `\` now
    // closes it.
    st: bool,
}

enum Kind {
    // `ESC [`, `ESC O` — parameters and intermediates, then a byte in
    // 0x40-0x7e.
    Control,
    // `ESC P/X/]/^/_` (DCS/SOS/OSC/PM/APC): arbitrary text closed by BEL
    // or ST (`ESC \`), not by a byte range — content may contain those bytes.
    Str,
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
        Self {
            kind: None,
            st: false,
        }
    }

    pub fn closed(&mut self, c: char) -> bool {
        let Some(kind) = &self.kind else {
            // `[` and `O` are also final bytes by the range test, so they
            // must come first to select Control instead of Done.
            let kind = match c {
                '[' | 'O' => Kind::Control,
                'P' | 'X' | ']' | '^' | '_' => Kind::Str,
                c if ('\x30'..='\x7e').contains(&c) => Kind::Done,
                _ => Kind::Bare,
            };
            let done = matches!(kind, Kind::Done);
            self.kind = Some(kind);
            return done;
        };
        match kind {
            Kind::Done => true,
            Kind::Control => ('\x40'..='\x7e').contains(&c),
            Kind::Bare => ('\x30'..='\x7e').contains(&c),
            Kind::Str => {
                let closed = c == '\x07' || (self.st && c == '\\');
                self.st = c == '\x1b';
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
        parts.push(format!("${:.4}", t.cost));
    }
    parts.join(icons::PART_SEP)
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

/// Columns `c` takes when it starts at column `col`: a tab runs to the next
/// stop, a control takes none. The one rule every surface measures by.
pub fn cols_at(c: char, col: usize) -> usize {
    match c {
        '\t' => TAB_STOP - col % TAB_STOP,
        c => unicode_width::UnicodeWidthChar::width(c).unwrap_or(0),
    }
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
    false
}

/// Display columns of one line by `cols_at`, escapes taking none.
pub fn width(s: &str) -> usize {
    let mut esc = None;
    s.chars()
        .filter(|&c| !escaping(&mut esc, c))
        .fold(0, |col, c| col + cols_at(c, col))
}

/// `s` with each tab spelled as the spaces it runs to; a `\n` starts over.
pub fn expand_tabs(s: &str) -> String {
    let (mut esc, mut col) = (None, 0);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if escaping(&mut esc, c) {
            out.push(c);
            continue;
        }
        let w = cols_at(c, col);
        match c {
            '\t' => out.extend(std::iter::repeat_n(' ', w)),
            '\n' => {
                out.push(c);
                col = 0;
                continue;
            }
            c => out.push(c),
        }
        col += w;
    }
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
    let (mut esc, mut used, mut styled) = (None, 0, false);
    for (i, c) in one.char_indices() {
        if escaping(&mut esc, c) {
            styled = true;
            continue;
        }
        used += cols_at(c, used);
        if used >= max {
            let cut = one[..i].trim_end();
            let reset = if styled { RESET } else { "" };
            return format!("{cut}{reset}{}", icons::ELLIPSIS);
        }
    }
    one
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

/// Pad a string to a display width with trailing spaces, so a column of
/// mixed-width (CJK) text lines up where `{:width$}` would only count chars.
pub fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(self::width(s))))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The mark a cut leaves is counted: callers lay out to `max`, not past it.
    #[test]
    fn a_clip_fits_its_columns_mark_and_all() {
        assert_eq!(clip("abcdef", 6), "abcdef");
        assert_eq!(clip("abcdefg", 6), "abcde…");
        assert_eq!(width(&clip("中文中文中文", 5)), 5);
        assert_eq!(clip("\x1b[31mabcdefg", 6), "\x1b[31mabcde\x1b[0m…");
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
