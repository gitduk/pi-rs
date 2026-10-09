//! The whole terminal, via ratatui's diffed cell buffer. History rebuilds
//! from the transcript, not terminal scrollback, so a rewind can forget it.

use std::io::Stdout;
use std::io::Write;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

// Consume the rest of an escape sequence whose `\x1b` was just read.
fn skip_escape(chars: &mut std::str::Chars<'_>) {
    let mut esc = pi_store::text::Escape::new();
    for n in chars.by_ref() {
        if esc.closed(n) {
            break;
        }
    }
}

/// Wraps a line to rows of exactly `width`. Escape sequences embedded in
/// the content (tool output noise) take no columns and are dropped.
pub fn fit(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        let mut chars = span.content.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Outside noise (tool output): no columns, no cells.
                skip_escape(&mut chars);
                continue;
            }
            if c == '\n' {
                out.push(Line::from(std::mem::take(&mut row)));
                used = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if used + w > width && used > 0 {
                out.push(Line::from(std::mem::take(&mut row)));
                used = 0;
            }
            match row.last_mut() {
                Some(s) if s.style == style => s.content.to_mut().push(c),
                _ => row.push(Span::styled(c.to_string(), style)),
            }
            used += w;
        }
    }
    out.push(Line::from(row));
    out
}

/// One short of real width: terminals disagree on whether the last
/// cell already wrapped, so this keeps every row a hard break.
pub fn usable(width: u16) -> usize {
    width.saturating_sub(1).max(1) as usize
}

/// One view line: how many screen rows it takes at its sized width,
/// and the text — asked for only by the lines the window actually shows.
pub trait Piece {
    /// Screen rows this line takes at the width it was sized for.
    fn height(&self) -> usize;

    /// The line's screen rows, oldest first: one per row it wraps to. See
    /// `wrap`.
    fn pieces(self) -> Vec<Line<'static>>;
}

/// A line in hand, sized by wrapping at the width it was built for —
/// live-block text with no said-row border to repeat.
pub struct Ready<'a> {
    pub line: Line<'a>,
    pub width: usize,
}

impl Piece for Ready<'_> {
    fn height(&self) -> usize {
        fit(&self.line, self.width).len()
    }

    fn pieces(self) -> Vec<Line<'static>> {
        fit(&self.line, self.width)
    }
}

/// As far as the history goes: a scroll, or a step, no window is short of.
/// The clamp below brings it back to the oldest rows the area can hold.
pub const TOP: usize = usize::MAX;

/// `room` rows from `lines`, `scroll` back from the bottom — measured in
/// wrapped rows, not lines, or the newest rows fall off screen. Also the
/// scroll clamped, and how many rows of the top line fell off above.
pub fn window_tagged<T: Clone, P: Piece>(
    lines: impl DoubleEndedIterator<Item = (P, T)>,
    room: usize,
    scroll: usize,
) -> (Vec<(Line<'static>, T)>, usize, usize) {
    // A scroll of `TOP` must reach the clamp below, not overflow on the way.
    let want = room.saturating_add(scroll);
    // Backwards on the counts alone: a line the window will not show costs its
    // height here, not the text it would take to build.
    let mut pending: Vec<(P, T)> = Vec::new();
    let mut have = 0usize;
    for item in lines.rev() {
        if have >= want {
            break;
        }
        have += item.0.height();
        pending.push(item);
    }
    let scroll = scroll.min(have.saturating_sub(room));
    let mut back: Vec<(Line<'static>, T)> = Vec::new();
    let mut skip = scroll;
    let mut cut = 0;
    for (line, tag) in pending {
        if back.len() >= room {
            break;
        }
        // A line still fully held back is skipped by its count alone; the
        // one the skip stops inside is asked for its text.
        if skip > 0 {
            let height = line.height();
            if skip >= height {
                skip -= height;
                continue;
            }
        }
        // Newest row first (the walk's direction): held-back rows drop off
        // the front, remaining room caps the back.
        let mut rows = line.pieces();
        rows.reverse();
        let left = room - back.len();
        cut = rows.len().saturating_sub(skip).saturating_sub(left);
        back.extend(
            rows.into_iter()
                .skip(skip)
                .take(left)
                .map(|row| (row, tag.clone())),
        );
        skip = 0;
    }
    back.reverse();
    (back, scroll, cut)
}

/// A line's spans with the line's own style folded into each, for building
/// another line out of them without losing that style.
pub(crate) fn spans_of(line: &Line<'_>) -> Vec<Span<'static>> {
    line.spans
        .iter()
        .map(|s| Span::styled(s.content.to_string(), line.style.patch(s.style)))
        .collect()
}

/// A line's text without its styling.
pub(crate) fn plain(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
pub fn window<'a>(
    lines: impl DoubleEndedIterator<Item = Line<'a>>,
    width: usize,
    room: usize,
    scroll: usize,
) -> (Vec<Line<'static>>, usize) {
    let lines = lines.map(move |line| (Ready { line, width }, ()));
    let (rows, scroll, _) = window_tagged(lines, room, scroll);
    (rows.into_iter().map(|(r, ())| r).collect(), scroll)
}

/// Breaks a line into one-row pieces; a bordered line repeats its border
/// on each piece instead of cutting the rule at the first row.
pub fn wrap(border: Option<&Line<'_>>, line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let Some(border) = border else {
        return fit(line, width);
    };
    // Rule needs its columns, body needs at least one; a frame too narrow
    // drops the rule rather than overflow the row count `window` promised.
    let spare = width.checked_sub(border.width()).filter(|avail| *avail > 0);
    let Some(avail) = spare else {
        return fit(line, width);
    };
    fit(line, avail)
        .into_iter()
        .map(|piece| {
            let mut spans = spans_of(border);
            spans.extend(piece.spans);
            Line::from(spans)
        })
        .collect()
}

/// Lays one row in a band: color goes under every span (text keeps its
/// own foreground) and fills columns past the text into one rectangle.
pub fn banded(line: Line<'static>, band: RStyle, width: usize) -> Line<'static> {
    let used = line.width();
    let mut spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content, band.patch(s.style)))
        .collect();
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), band));
    }
    Line::from(spans)
}

// Write one fitted row into the buffer: one cell per character, styled by
// its span. Wide characters take two cells; combining marks decorate back.
fn write_line(line: &Line<'_>, x: u16, y: u16, buf: &mut Buffer) {
    let mut col = x;
    for span in &line.spans {
        let mut chars = span.content.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                skip_escape(&mut chars);
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
            buf.set_stringn(col, y, c.to_string(), w as usize, span.style);
            col += w;
        }
    }
}

/// Every row the screen shows, wrapped and styled into the cell buffer.
pub struct Rows<'a>(pub &'a [Line<'a>]);

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
                write_line(&piece, area.x, y, buf);
                y += 1;
            }
        }
    }
}

// The terminal this draws on. An enum, not a generic, keeps the backend
// out of `Screen`/`Ui`/`Tui`'s types — what makes the surface testable.
enum Term {
    Live(Terminal<CrosstermBackend<Stdout>>),
    // An in-memory grid: never enters raw mode or the alt screen, so
    // `leave` has nothing to undo, keeping tests off the real terminal.
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

// All-motion mouse reports every move (hover needs it); normal capture
// only reports press/release. Disabled in the panic hook and `leave`.
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

// Terminal-restoring panic hook: raw mode off, alt screen left, the
// prior hook chained behind. `leave` restores that prior hook.
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

        // A panic in raw mode leaves a terminal the user must `reset`,
        // the panic message itself unreadable. Prior hook saved for `leave`.
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

    /// What the in-memory terminal holds after the last draw.
    #[cfg(test)]
    pub fn test_buffer(&self) -> &Buffer {
        match &self.term {
            Term::Test(t) => t.backend().buffer(),
            Term::Live(_) => unreachable!("a test screen is the in-memory one"),
        }
    }

    pub fn usable(&self) -> usize {
        usable(self.width)
    }

    pub fn resized(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    /// Redraws one frame; the closure draws into ratatui's buffer, which
    /// diffs against the last so only changed rows reach the terminal.
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

    /// Shapes the caret for the mode: block for Normal, bar for Insert,
    /// `None` (vim off) hands the shape back. Live terminals only.
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

    /// Gives the terminal back: leaves the alt screen, restores raw mode.
    /// A no-op on a test screen — must not touch the runner's own terminal.
    pub fn leave(&mut self) {
        if !matches!(self.term, Term::Live(_)) {
            return;
        }
        // Straight to stdout, not the terminal's backend — the same
        // target and path the panic hook above uses to restore it.
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
    use super::{fit, plain, window};
    use ratatui::text::Line;

    // An escape the outside world left in the content takes no columns and
    // no cells: the count and the text agree on what a row holds.
    #[test]
    fn an_escape_in_the_content_is_noise_not_cells() {
        let line = Line::from("a\x1b[31mb");
        let rows = fit(&line, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(plain(&rows[0]), "ab");
    }

    // The window's rows, plain, for a history of `lines` at width `width`.
    fn shown(lines: &[&str], width: usize, room: usize, scroll: usize) -> Vec<String> {
        let owned: Vec<Line<'static>> = lines.iter().map(|l| Line::from(l.to_string())).collect();
        let (rows, _) = window(owned.iter().cloned(), width, room, scroll);
        rows.into_iter().map(|l| plain(&l)).collect()
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
        // "abcdef" wraps to two rows at width 3, so the window counts by rows.
        assert_eq!(shown(&["abcdef", "gh"], 3, 2, 0), vec!["def", "gh"]);
    }

    #[test]
    fn scrolling_up_holds_back_the_newest_rows() {
        assert_eq!(shown(&["a", "b", "c", "d"], 10, 2, 1), vec!["b", "c"]);
    }

    // The walk counts screen rows: a window whose oldest row starts inside
    // a wrapped line takes only the rows it reaches, not the line whole.
    #[test]
    fn a_window_that_starts_inside_a_wrapped_line_cuts_that_line() {
        // At width 2: "abcdef" is three rows, "gh" and "ij" one each.
        assert_eq!(shown(&["abcdef", "gh", "ij"], 2, 2, 1), vec!["ef", "gh"]);
    }

    #[test]
    fn scrolling_stops_at_the_oldest_row() {
        let owned: Vec<Line<'static>> = ["a", "b", "c"]
            .iter()
            .map(|l| Line::from(l.to_string()))
            .collect();
        let (rows, scroll) = window(owned.iter().cloned(), 10, 2, 99);
        assert_eq!(
            rows.iter().map(plain).collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(scroll, 1, "clamped, so one press down comes back");
    }

    #[test]
    fn an_empty_history_draws_nothing() {
        assert!(shown(&[], 10, 3, 0).is_empty());
    }
}
