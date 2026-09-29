//! Cutting and measuring text the terminal will show, and the wording of a
//! run's spend. Shared by every layer, so it sits under all of them.
//!
//! Columns, not characters: a line of Chinese fits half as many characters in
//! the same width, and an escape sequence occupies none.

use llm::figures::{in_out, short};

use crate::store::icons;

pub(crate) const RESET: &str = "\x1b[0m";

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
    // `ESC P`, `ESC X`, `ESC ]`, `ESC ^`, `ESC _` — DCS, SOS, OSC, PM, APC.
    // Arbitrary text closed by BEL or by ST (`ESC \`), not by any byte
    // range: an OSC setting the window title carries a `;` and the title,
    // and a scanner reading it as a control sequence stops on the first
    // letter and draws the rest of the title.
    Str,
    // A bare `ESC` with an intermediate byte, ending on a byte in 0x30-0x7e
    // — wider than a control sequence's, which is where `ESC 7` lives.
    Bare,
    // Over already: the first character was itself the final byte. `ESC 7`
    // saves the cursor and `ESC 8` restores it — what `less`, `vim` and
    // every progress bar emit most — and 0x37 is outside a control
    // sequence's range, so reading one as a control sequence leaves it
    // looking unfinished and eats the character after it.
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
            // The first character decides the shape, and for a two-byte
            // sequence it is also the last. `[` and `O` are final bytes by
            // the range test too, so they are matched before it.
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

// The characters a terminal actually shows, escapes stepped over: they cost
// a dozen bytes and zero columns, so anything measuring or reproducing what
// is on screen has to skip them the same way.
fn visible(s: &str) -> impl Iterator<Item = char> + '_ {
    let mut chars = s.chars();
    std::iter::from_fn(move || {
        loop {
            let c = chars.next()?;
            if c != '\x1b' {
                return Some(c);
            }
            let mut esc = Escape::new();
            for c in chars.by_ref() {
                if esc.closed(c) {
                    break;
                }
            }
        }
    })
}

/// The columns a painted string occupies, which is what a layout has to
/// budget for — not its byte or character count.
pub fn visible_width(s: &str) -> usize {
    visible(s)
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// A spend in the one wording every place that says it uses: `/status` hands
/// it a session's figures, and a status line composes the same two out of its
/// `in_out`, `cache` and `cost` segments, which say a run's. The cost is shown
/// only when the model is priced — an unpriced model reports no cost rather
/// than $0.
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

/// One line, cut to `max` columns.
///
/// Columns rather than characters: what overflows a terminal is columns, and a
/// line of Chinese fits half as many characters in the same width. Counting
/// characters let a `grep` pattern or a refusal written in Chinese run to twice
/// the intended width and wrap.
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
