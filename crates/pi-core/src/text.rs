//! Cutting and measuring text the terminal will show, and the wording of a
//! run's spend. Shared by every layer, so it sits under all of them.
//!
//! Columns, not characters: a line of Chinese fits half as many characters in
//! the same width, and an escape sequence occupies none.

use llm::figures::{in_out, short};

use crate::store::icons;

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
pub fn spent(t: &agent::Totals) -> String {
    let mut parts = vec![in_out(t.usage.input, t.usage.output)];
    if t.usage.cache_read > 0 {
        parts.push(format!("{} cached", short(t.usage.cache_read)));
    }
    if t.cost > 0.0 {
        parts.push(format!("${:.4}", t.cost));
    }
    parts.join(icons::PART_SEP)
}

/// One line, cut to `max` display columns, not characters (see module doc).
pub fn clip(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    let mut used = 0;
    // An escape is stepped over, not counted: it is a dozen printable
    // characters and zero columns. A cut inside one's reach closes the style.
    let mut esc: Option<Escape> = None;
    let mut styled = false;
    for (i, c) in one.char_indices() {
        if let Some(open) = &mut esc {
            if open.closed(c) {
                esc = None;
            }
            continue;
        }
        if c == '\x1b' {
            (esc, styled) = (Some(Escape::new()), true);
            continue;
        }
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > max {
            let cut = one[..i].trim_end();
            return match styled {
                true => format!("{cut}{RESET}{}", icons::ELLIPSIS),
                false => format!("{cut}{}", icons::ELLIPSIS),
            };
        }
    }
    one
}

/// Pad a string to a display width with trailing spaces, so a column of
/// mixed-width (CJK) text lines up where `{:width$}` would only count chars.
pub fn pad(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}
