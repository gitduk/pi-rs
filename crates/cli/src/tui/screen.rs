//! The whole terminal, rendered through ratatui's cell buffer.
//!
//! Every row Pi shows is written into the buffer each frame and diffed by
//! ratatui against the previous frame, so only what changed reaches the
//! terminal. History is part of the conversation and has to be rebuildable
//! when it changes — a rewind forgets a turn, and the screen has to forget
//! it too — so the buffer is rebuilt from the transcript rather than kept
//! as terminal scrollback.

use std::borrow::Cow;
use std::io::Stdout;
use std::io::Write;
use std::str::Chars;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

use crate::render::parse_sgr;

const RESET: &str = "\x1b[0m";

/// Break a line into pieces that each occupy exactly one terminal row.
///
/// Repainting works by counting rows, so a line that wraps on its own would
/// throw the count off by however many times it wrapped. Escape sequences take
/// no columns and must not be counted; every broken piece is closed with a
/// reset and re-opens with the styling still in force, so a wrapped coloured
/// line keeps its colour past the first row.
pub fn fit(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut piece = String::new();
    let mut used = 0usize;
    // A break re-opens the SGR in force, or the rest of a coloured line
    // would come out plain: the style lives at the head of the line.
    let mut sgr = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(params) = eat_escape(&mut chars, &mut piece) {
                let mut parts = params.split(';').peekable();
                while let Some(p) = parts.next() {
                    match p {
                        "0" | "" => sgr.clear(),
                        "38" | "48" => {
                            // The mode and its payload are data, not codes:
                            // `5;n` or `2;r;g;b`, where a zero is a colour
                            // component, never a reset.
                            push_sgr(&mut sgr, p);
                            match parts.peek().copied() {
                                Some("5") => {
                                    push_sgr(&mut sgr, "5");
                                    parts.next();
                                    if let Some(n) = parts.next() {
                                        push_sgr(&mut sgr, n);
                                    }
                                }
                                Some("2") => {
                                    push_sgr(&mut sgr, "2");
                                    parts.next();
                                    for _ in 0..3 {
                                        if let Some(v) = parts.next() {
                                            push_sgr(&mut sgr, v);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => push_sgr(&mut sgr, p),
                    }
                }
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        let w = c.width().unwrap_or(0);
        if c == '\n' || (used + w > width && used > 0) {
            out.push(std::mem::take(&mut piece) + RESET);
            if !sgr.is_empty() {
                piece.push('\x1b');
                piece.push('[');
                piece.push_str(&sgr);
                piece.push('m');
            }
            used = 0;
            if c == '\n' {
                continue;
            }
        }
        piece.push(c);
        used += w;
    }
    out.push(piece);
    out
}

// The escape sequence at the head of `chars`, appended to `out`; its SGR
// parameters when it is one. The introducer is consumed before the scan:
// `[` and `O` are themselves inside the final-byte range, so a scan that
// started on one would stop on it and leave the parameters to be counted
// as text.
fn eat_escape<'a, 'b>(chars: &mut Chars<'a>, out: &'b mut String) -> Option<&'b str> {
    let at = out.len();
    out.push('\x1b');
    let mut esc = crate::render::Escape::new();
    for c in chars.by_ref() {
        out.push(c);
        if esc.closed(c) {
            break;
        }
    }
    out[at..]
        .strip_prefix("\x1b[")
        .and_then(|s| s.strip_suffix('m'))
}

// Append one SGR parameter, separated from the ones before it.
fn push_sgr(sgr: &mut String, p: &str) {
    if !sgr.is_empty() {
        sgr.push(';');
    }
    sgr.push_str(p);
}

/// One column short of the real width, so nothing ever lands on the last cell.
///
/// Terminals disagree about whether a character written to the last cell has
/// already wrapped, so a row that reaches it may be joined to the next one on
/// resize. Stopping a column short keeps every row a hard break, and the row
/// count stays stable across a resize.
pub fn usable(width: u16) -> usize {
    width.saturating_sub(1).max(1) as usize
}

/// The window of rows to show: the last `room` rows of `lines`, with `scroll`
/// rows held back from the bottom. The clamped scroll comes back with them.
///
/// A line is not a row — anything wider than the terminal wraps — so the
/// window has to be measured after wrapping. Measuring it in lines instead
/// puts more rows in the area than fit and the newest ones fall off the
/// bottom, out of sight below the input. The walk starts from the newest line
/// and stops as soon as the window is full, so a long history is not wrapped
/// in full on every frame.
///
/// Each line carries the border its wraps repeat, if it has one: a said
/// line's rule must run down every row it wraps to, or the bar is cut at the
/// first one. See `wrap`.
pub fn window_tagged<'a, T: Clone>(
    lines: impl DoubleEndedIterator<Item = ((Cow<'a, str>, Option<&'a str>), T)>,
    width: usize,
    room: usize,
    scroll: usize,
) -> (Vec<(String, T)>, usize) {
    let want = room + scroll;
    let mut back: Vec<(String, T)> = Vec::new();
    for ((line, border), tag) in lines.rev() {
        if back.len() >= want {
            break;
        }
        for piece in wrap(border, &line, width).into_iter().rev() {
            back.push((piece, tag.clone()));
        }
    }
    let scroll = scroll.min(back.len().saturating_sub(room));
    let mut rows: Vec<(String, T)> = back.into_iter().skip(scroll).take(room).collect();
    rows.reverse();
    (rows, scroll)
}

#[cfg(test)]
pub fn window<'a>(
    lines: impl DoubleEndedIterator<Item = (Cow<'a, str>, Option<&'a str>)>,
    width: usize,
    room: usize,
    scroll: usize,
) -> (Vec<String>, usize) {
    let (rows, scroll) = window_tagged(lines.map(|l| (l, ())), width, room, scroll);
    (rows.into_iter().map(|(r, ())| r).collect(), scroll)
}

/// Break a line into pieces that each occupy exactly one terminal row — a
/// bordered line repeats its border on every piece, so a said line keeps its
/// rule unbroken down the rows it wraps to instead of cutting it at the first.
pub fn wrap(border: Option<&str>, line: &str, width: usize) -> Vec<String> {
    let Some(border) = border else {
        return fit(line, width);
    };
    // The rule needs its columns and the body needs at least one. A frame too
    // narrow to spare both drops the rule rather than overflowing: a row wider
    // than `width` wraps again under whatever paints it, and `window` counted
    // the rows on the promise that none of them would.
    let spare = width
        .checked_sub(crate::render::visible_width(border))
        .filter(|avail| *avail > 0);
    let Some(avail) = spare else {
        return fit(line, width);
    };
    fit(line, avail)
        .into_iter()
        .map(|piece| format!("{border}{piece}"))
        .collect()
}

// Write one fitted row into the buffer: style from the SGR escapes, one
// cell per character.
fn write_piece(piece: &str, x: u16, y: u16, buf: &mut Buffer) {
    let mut style = Style::default();
    let mut col = x;
    let mut chars = piece.chars();
    let mut seq = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            seq.clear();
            if let Some(sgr) = eat_escape(&mut chars, &mut seq) {
                style = parse_sgr(sgr, style);
            }
            continue;
        }
        let w = c.width().unwrap_or(0) as u16;
        if w == 0 {
            if c.is_control() {
                continue;
            }
            // A combining mark decorates the cell before it — skipping the
            // blank second cell a wide character leaves behind.
            let mut prev = col;
            while prev > x {
                prev -= 1;
                let symbol = buf[(prev, y)].symbol().to_string();
                if !symbol.is_empty() && symbol != " " {
                    buf[(prev, y)].set_symbol(&format!("{symbol}{c}"));
                    break;
                }
            }
            continue;
        }
        buf.set_stringn(col, y, c.to_string(), w as usize, style);
        col += w;
    }
}

/// Every row the screen shows, wrapped and styled into the cell buffer.
pub struct Rows<'a>(pub &'a [String]);

impl Widget for Rows<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = usable(area.width);
        let mut y = area.y;
        for line in self.0 {
            if y >= area.y + area.height {
                break;
            }
            for piece in fit(line, width) {
                if y >= area.y + area.height {
                    break;
                }
                write_piece(&piece, area.x, y, buf);
                y += 1;
            }
        }
    }
}

// The terminal this screen draws on. An enum rather than a generic so the
// backend stays out of `Screen`'s type — and out of `Ui`'s and `Tui`'s with
// it, which is the whole reason the surface was untestable.
enum Term {
    Live(Terminal<CrosstermBackend<Stdout>>),
    // An in-memory grid. It never enters raw mode or the alternate screen, so
    // `leave` has nothing to undo — which is what keeps a test off the
    // terminal the test runner itself is using.
    #[cfg(test)]
    Test(Terminal<ratatui::backend::TestBackend>),
}

pub struct Screen {
    term: Term,
    pub width: u16,
    pub height: u16,
}

// Raw mode and the three modes the surface draws under. One function so `new`
// and `resume` claim exactly what `leave` gives back, rather than nearly.
fn enter(stdout: &mut Stdout) -> std::io::Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    if let Err(e) = crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    ) {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(e);
    }
    stdout.write_all(b"\x1b[?1003h")?;
    stdout.flush()
}

// All-motion mouse reports every move, which is what hover needs; the
// normal capture mode only reports press and release. Off again whenever
// the surface is given back, in the panic hook and in `leave`.
fn disable_all_motion() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?1003l");
    let _ = out.flush();
}

// The hook that held the process's panics before `new` replaced it; `leave`
// puts it back, so the escape cleanup dies with the surface that needed it.
type PriorHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;
static PRIOR_HOOK: std::sync::Mutex<Option<PriorHook>> = std::sync::Mutex::new(None);

fn prior_hook() -> std::sync::MutexGuard<'static, Option<PriorHook>> {
    PRIOR_HOOK.lock().unwrap_or_else(|p| p.into_inner())
}

// The terminal-restoring panic hook: raw mode off, alternate screen left, the
// process's former hook chained behind. `leave` hands that former hook back,
// so anything that re-enters the terminal must set this again.
fn set_escape_hook() {
    std::panic::set_hook(Box::new(|info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        disable_all_motion();
        let prior = prior_hook();
        if let Some(prior) = prior.as_ref() {
            prior(info);
        }
    }));
}

impl Screen {
    pub fn new() -> std::io::Result<Self> {
        let mut stdout = std::io::stdout();
        enter(&mut stdout)?;

        // A panic in raw mode otherwise leaves a terminal the user has to
        // `reset`, with the panic message itself unreadable. The hook that
        // was there before is kept above for `leave` to put back.
        *prior_hook() = Some(std::panic::take_hook());
        set_escape_hook();
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let size = terminal.size()?;
        Ok(Self {
            term: Term::Live(terminal),
            width: size.width,
            height: size.height,
        })
    }

    /// A screen backed by an in-memory grid, for tests that drive the surface
    /// without a terminal to drive it on.
    #[cfg(test)]
    pub fn test(width: u16, height: u16) -> Self {
        let backend = ratatui::backend::TestBackend::new(width, height);
        Self {
            term: Term::Test(Terminal::new(backend).expect("an in-memory terminal")),
            width,
            height,
        }
    }

    pub fn usable(&self) -> usize {
        usable(self.width)
    }

    pub fn resized(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Redraw one frame. The closure draws into ratatui's buffer; ratatui
    /// diffs it against the last frame, so only the rows that changed reach
    /// the terminal.
    pub fn draw(&mut self, f: impl FnOnce(&mut ratatui::Frame<'_>)) -> std::io::Result<()> {
        match &mut self.term {
            Term::Live(t) => {
                t.draw(|frame| f(frame))?;
            }
            // Drawing into memory cannot fail: its error type is `Infallible`.
            #[cfg(test)]
            Term::Test(t) => {
                let _ = t.draw(|frame| f(frame));
            }
        }
        Ok(())
    }

    /// Wipe the screen and start again at the top.
    pub fn clear(&mut self) {
        match &mut self.term {
            Term::Live(t) => {
                let _ = t.clear();
            }
            #[cfg(test)]
            Term::Test(t) => {
                let _ = t.clear();
            }
        }
    }

    /// Shape the caret to say which mode is up: a block commands, a bar types,
    /// and `None` — vim off — hands the shape back, so nobody who never asked
    /// for modal keys ends up with a caret they did not choose. The one part
    /// of the mode no repaint carries: the caret is the terminal's to draw,
    /// not ratatui's.
    ///
    /// Live terminals only — a test screen has none to shape, and writing to
    /// stdout there would mark the runner's own caret.
    pub fn cursor_shape(&mut self, normal: Option<bool>) {
        if !matches!(self.term, Term::Live(_)) {
            return;
        }
        use crossterm::cursor::SetCursorStyle as Shape;
        let style = match normal {
            Some(true) => Shape::SteadyBlock,
            Some(false) => Shape::SteadyBar,
            None => Shape::DefaultUserShape,
        };
        let _ = crossterm::execute!(std::io::stdout(), style);
    }

    /// Give the terminal back: leave the alternate screen and restore raw.
    /// Nothing to give back when nothing was taken, so a test screen is a
    /// no-op here — it must not disable raw mode on the runner's own terminal.
    pub fn leave(&mut self) {
        if !matches!(self.term, Term::Live(_)) {
            return;
        }
        // Straight to stdout rather than through the terminal's backend, which
        // is the same thing it writes to and the same way the panic hook above
        // restores it.
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::cursor::Show,
            // The caret is the terminal's, not the alternate screen's: a
            // block left behind would follow the user into their shell.
            crossterm::cursor::SetCursorStyle::DefaultUserShape,
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        );
        disable_all_motion();
        let _ = crossterm::terminal::disable_raw_mode();
        // `take` makes this idempotent: `leave` runs once from a caller and
        // once more from `Drop`, and the second finds nothing to restore.
        if let Some(prior) = prior_hook().take() {
            std::panic::set_hook(prior);
        }
    }

    /// Take the terminal back after `leave` gave it to a child. Errors are
    /// returned, not swallowed: an absent surface must not be a surprise.
    pub fn resume(&mut self) -> std::io::Result<()> {
        // Asked for rather than remembered: a resize while the child held the
        // terminal raised no event this process could have seen.
        let size = match &self.term {
            Term::Live(t) => {
                enter(&mut std::io::stdout())?;
                // `leave` gave the process's hook back; reclaim it, or a
                // panic after this point lands on a raw alternate screen.
                set_escape_hook();
                t.size()?
            }
            // Nothing was taken from a test screen, so there is nothing to
            // claim back — and raw mode here is the test runner's own.
            #[cfg(test)]
            Term::Test(_) => return Ok(()),
        };
        self.width = size.width;
        self.height = size.height;
        // Re-entering gives back a screen ratatui's diff no longer describes,
        // so the next frame has to be a whole one.
        self.clear();
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::window;
    use std::borrow::Cow;

    // The window's rows, plain, for a history of `lines` at width `width`.
    fn shown(lines: &[&str], width: usize, room: usize, scroll: usize) -> Vec<String> {
        let owned: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let (rows, _) = window(
            owned.iter().map(|s| (Cow::Borrowed(s.as_str()), None)),
            width,
            room,
            scroll,
        );
        rows
    }

    #[test]
    fn the_window_ends_on_the_newest_row() {
        assert_eq!(shown(&["a", "b", "c"], 10, 2, 0), vec!["b", "c"]);
    }

    #[test]
    fn a_short_history_is_shown_whole() {
        assert_eq!(shown(&["a", "b"], 10, 5, 0), vec!["a", "b"]);
    }

    #[test]
    fn a_wrapped_line_counts_as_the_rows_it_takes() {
        // The bug this replaced counted lines: "abcdef" is one line and two
        // rows at width 3, so a two-row window that took two lines drew four
        // rows into it and the newest two landed below the area, under the input.
        assert_eq!(shown(&["abcdef", "gh"], 3, 2, 0), vec!["def", "gh"]);
    }

    #[test]
    fn scrolling_up_holds_back_the_newest_rows() {
        assert_eq!(shown(&["a", "b", "c", "d"], 10, 2, 1), vec!["b", "c"]);
    }

    #[test]
    fn scrolling_stops_at_the_oldest_row() {
        let owned: Vec<String> = ["a", "b", "c"].iter().map(|l| l.to_string()).collect();
        let (rows, scroll) = window(
            owned.iter().map(|s| (Cow::Borrowed(s.as_str()), None)),
            10,
            2,
            99,
        );
        assert_eq!(rows, vec!["a", "b"]);
        assert_eq!(scroll, 1, "clamped, so one press down comes back");
    }

    #[test]
    fn an_empty_history_draws_nothing() {
        assert!(shown(&[], 10, 3, 0).is_empty());
    }
}
