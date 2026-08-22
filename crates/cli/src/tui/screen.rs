//! The bottom few rows of the terminal, repainted in place.
//!
//! Everything finished is pushed *above* the live region and becomes ordinary
//! terminal scrollback — selectable, searchable, and still there after pi
//! exits. Only the part that is still changing — the open stream, the status
//! line, the prompt — is redrawn, by walking the cursor back over it.

use std::io::{Stdout, Write};

use unicode_width::UnicodeWidthChar;

const RESET: &str = "\x1b[0m";

/// Break a line into pieces that each occupy exactly one terminal row.
///
/// Repainting works by counting rows, so a line that wraps on its own would
/// throw the count off by however many times it wrapped. Escape sequences take
/// no columns and must not be counted; a break re-opens the next piece with no
/// styling, which is why each one is closed with a reset.
pub fn fit(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut used = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            piece.push(c);
            // The introducer has to be consumed before the scan: `[` and `O`
            // are themselves inside the final-byte range, so a scan that
            // started on one would stop on it and leave the parameters to be
            // counted as text.
            if let Some(intro @ ('[' | 'O')) = chars.peek().copied() {
                piece.push(intro);
                chars.next();
                for c in chars.by_ref() {
                    piece.push(c);
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        let w = c.width().unwrap_or(0);
        if used + w > width && !piece.is_empty() {
            out.push(std::mem::take(&mut piece) + RESET);
            used = 0;
        }
        piece.push(c);
        used += w;
    }
    out.push(piece);
    out
}

/// One column short of the real width, so nothing ever lands on the last cell.
///
/// Two reasons, and the second is the load-bearing one. Terminals disagree
/// about whether a character written to the last cell has already wrapped. And
/// a row that stops short carries no wrap flag, so a terminal that reflows on
/// resize will not join it to the next — which is what lets the region's row
/// count survive a resize at all. See [`Screen::resized`].
pub fn usable(width: u16) -> usize {
    width.saturating_sub(1).max(1) as usize
}

/// Where the caret should sit inside the live region: a row index, and a column
/// measured in terminal cells.
pub type Caret = (u16, u16);

pub struct Screen {
    out: Stdout,
    /// Rows the live region occupies right now.
    rows: u16,
    /// Which of those rows the caret was left on, so the next repaint knows how
    /// far back to walk.
    at: u16,
    pub width: u16,
    pub height: u16,
}

impl Screen {
    pub fn new() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        // So a pasted block arrives as one event instead of as keystrokes with
        // newlines in them, each of which would submit the line.
        let _ = write!(out, "\x1b[?2004h");
        let _ = out.flush();

        // A panic in raw mode otherwise leaves a terminal the user has to
        // `reset`, with the panic message itself unreadable.
        let prior = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = write!(std::io::stdout(), "\x1b[?2004l\r\n");
            prior(info);
        }));

        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
        Ok(Self {
            out,
            rows: 0,
            at: 0,
            width,
            height,
        })
    }

    pub fn usable(&self) -> usize {
        usable(self.width)
    }

    /// The row count survives a resize deliberately.
    ///
    /// A terminal reflows only lines it wrapped itself, and no row here is ever
    /// one of those — `usable` keeps every row a column short of the width, so
    /// each ends in a hard break the reflow leaves alone. The region therefore
    /// still occupies the rows it did, and the walk back over them still lands
    /// on its top. Forgetting the count instead leaves those rows on screen as
    /// a copy of the region, with the real one redrawn below it.
    pub fn resized(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    fn rewind(&self, buf: &mut String) {
        if self.at > 0 {
            buf.push_str(&format!("\x1b[{}A", self.at));
        }
        buf.push_str("\r\x1b[J");
    }

    /// Push `above` into scrollback and repaint the live region, in one write.
    ///
    /// Split into two writes it flickers, because the terminal shows the moment
    /// where the region has been erased and not yet redrawn.
    pub fn render(
        &mut self,
        above: &[String],
        live: &[String],
        caret: Caret,
    ) -> std::io::Result<()> {
        let mut buf = String::from("\x1b[?25l");
        self.rewind(&mut buf);
        for line in above {
            buf.push_str(line);
            buf.push_str("\r\n");
        }
        for (i, line) in live.iter().enumerate() {
            if i > 0 {
                buf.push_str("\r\n");
            }
            buf.push_str(line);
        }

        // The caret is left wherever the last row ended; walk it back to where
        // the editor's insertion point actually is.
        let last = live.len().saturating_sub(1) as u16;
        let (row, col) = (caret.0.min(last), caret.1);
        if last > row {
            buf.push_str(&format!("\x1b[{}A", last - row));
        }
        buf.push('\r');
        if col > 0 {
            buf.push_str(&format!("\x1b[{col}C"));
        }
        buf.push_str("\x1b[?25h");

        self.rows = live.len() as u16;
        self.at = row;
        self.out.write_all(buf.as_bytes())?;
        self.out.flush()
    }

    /// Wipe the screen and start the region again at the top.
    pub fn clear(&mut self) {
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        let _ = self.out.flush();
        self.rows = 0;
        self.at = 0;
    }

    /// Give the terminal back, with the cursor on a fresh line of its own.
    pub fn leave(&mut self) {
        let mut buf = String::new();
        self.rewind(&mut buf);
        buf.push_str("\x1b[?2004l\x1b[?25h");
        let _ = self.out.write_all(buf.as_bytes());
        let _ = self.out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
        self.rows = 0;
        self.at = 0;
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn a_row_always_stops_short_of_the_last_cell() {
        // Reaching it costs both ways: the terminal may or may not count it as
        // wrapped already, and a row that did reach it would be rejoined to the
        // next one by any terminal that reflows on resize — taking the row
        // count that `resized` relies on with it.
        for width in [2u16, 3, 40, 200] {
            assert!(super::usable(width) < width as usize, "width {width}");
        }
        // Never zero, or `fit` would have no room to make progress.
        assert_eq!(super::usable(0), 1);
        assert_eq!(super::usable(1), 1);
    }

    #[test]
    fn a_short_line_is_one_row() {
        assert_eq!(fit("hello", 20), vec!["hello"]);
    }

    #[test]
    fn a_long_line_breaks_at_the_width() {
        assert_eq!(fit("abcdef", 2), vec!["ab\x1b[0m", "cd\x1b[0m", "ef"]);
    }

    #[test]
    fn escape_sequences_take_no_columns() {
        // Otherwise a coloured line counts its own styling against the width
        // and breaks several rows early.
        let painted = "\x1b[2mabcd\x1b[0m";
        assert_eq!(fit(painted, 4).len(), 1);
    }

    #[test]
    fn a_wide_character_takes_two_cells() {
        // Counting chars rather than columns puts twice as much on the row as
        // fits, and every later row lands one line off.
        assert_eq!(fit("中文", 2), vec!["中\x1b[0m", "文"]);
        assert_eq!(fit("中文", 4), vec!["中文"]);
    }

    #[test]
    fn a_character_wider_than_the_room_still_makes_progress() {
        // Without the empty-piece guard this loops forever emitting blanks.
        assert_eq!(fit("中", 1), vec!["中"]);
    }

    #[test]
    fn an_empty_line_is_still_one_row() {
        assert_eq!(fit("", 10), vec![""]);
    }
}
