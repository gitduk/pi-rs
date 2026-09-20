//! The whole terminal, rendered through ratatui's cell buffer.
//!
//! Every row Pi shows is written into the buffer each frame and diffed by
//! ratatui against the previous frame, so only what changed reaches the
//! terminal. History is part of the conversation and has to be rebuildable
//! when it changes — a rewind forgets a turn, and the screen has to forget
//! it too — so the buffer is rebuilt from the transcript rather than kept
//! as terminal scrollback.

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
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

/// Break a styled line into lines that each occupy exactly one terminal row.
///
/// Repainting works by counting rows, so a line that wraps on its own would
/// throw the count off by however many times it wrapped. Styles ride on the
/// spans, so a wrapped coloured line keeps its colour past the first row
/// without anything re-opening an escape sequence. Escape sequences found in
/// the content itself — outside noise a tool's output carried in — take no
/// columns and no cells.
pub fn fit(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        let style = span.style;
        let mut chars = span.content.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // An escape in the content is outside noise: no columns, no
                // cells, nothing the spans do not already say. Consume it.
                let mut esc = crate::render::Escape::new();
                for n in chars.by_ref() {
                    if esc.closed(n) {
                        break;
                    }
                }
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

/// One column short of the real width, so nothing ever lands on the last cell.
///
/// Terminals disagree about whether a character written to the last cell has
/// already wrapped, so a row that reaches it may be joined to the next one on
/// resize. Stopping a column short keeps every row a hard break, and the row
/// count stays stable across a resize.
pub fn usable(width: u16) -> usize {
    width.saturating_sub(1).max(1) as usize
}

/// One line of the view as the window walks it: how many screen rows it takes
/// at the width it was sized for, and the text itself.
///
/// The text is asked for only by the lines the window shows. A scrolled view
/// walks back over everything above where it starts, so a line it passes has
/// to cost its count and nothing else — wrapping every one of them on the way
/// is what made scrolling cost what had been scrolled.
pub trait Piece {
    /// Screen rows this line takes at the width it was sized for.
    fn height(&self) -> usize;

    /// The line's screen rows, oldest first: one per row it wraps to. See
    /// `wrap`.
    fn pieces(self) -> Vec<Line<'static>>;
}

/// A line in hand, sized by wrapping it at the width it was built for: the
/// live block's lines, which are nothing but text, and what the layout tests
/// hand the window — where the scrollback hands it a row's line it would
/// rather not build. A live line is not a said one, so there is no border for
/// its wrapped rows to repeat.
///
/// Counted by wrapping rather than taken as the one row a fitted line usually
/// is: the count the window walks on and the rows it goes on to take have to
/// agree about every line, and only the wrap makes them do it whatever the
/// live block hands over.
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

/// The window of rows to show: the last `room` rows of `lines`, with `scroll`
/// rows held back from the bottom. The clamped scroll comes back with them.
///
/// A line is not a row — anything wider than the terminal wraps — so the
/// window has to be measured after wrapping. Measuring it in lines instead
/// puts more rows in the area than fit and the newest ones fall off the
/// bottom, out of sight below the input. The walk starts from the newest line
/// and stops as soon as the window is full, so a long history is not built in
/// full on every frame; each line says how many rows it takes, so the rows the
/// window scrolls past are counted and dropped, unwrapped and unbuilt.
pub fn window_tagged<T: Clone, P: Piece>(
    lines: impl DoubleEndedIterator<Item = (P, T)>,
    room: usize,
    scroll: usize,
) -> (Vec<(Line<'static>, T)>, usize) {
    let want = room + scroll;
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
    for (line, tag) in pending {
        if back.len() >= room {
            break;
        }
        // A line the window still has rows to hold back is passed over by its
        // count alone; from the one it stops inside, nothing is asked but the
        // text.
        if skip > 0 {
            let height = line.height();
            if skip >= height {
                skip -= height;
                continue;
            }
        }
        // Newest screen row first, the direction the walk came from, with the
        // rows held back dropped off the front of it and the window's remaining
        // room taken off the back.
        let mut rows = line.pieces();
        rows.reverse();
        let left = room - back.len();
        back.extend(
            rows.into_iter()
                .skip(skip)
                .take(left)
                .map(|row| (row, tag.clone())),
        );
        skip = 0;
    }
    back.reverse();
    (back, scroll)
}

/// A line's text without its styling, for tests that assert on layout
/// rather than colour.
#[cfg(test)]
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
    let (rows, scroll) = window_tagged(lines, room, scroll);
    (rows.into_iter().map(|(r, ())| r).collect(), scroll)
}

/// Break a line into pieces that each occupy exactly one terminal row — a
/// bordered line repeats its border on every piece, so a said line keeps its
/// rule unbroken down the rows it wraps to instead of cutting it at the first.
pub fn wrap(border: Option<&Line<'_>>, line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let Some(border) = border else {
        return fit(line, width);
    };
    // The rule needs its columns and the body needs at least one. A frame too
    // narrow to spare both drops the rule rather than overflowing: a row wider
    // than `width` wraps again under whatever paints it, and `window` counted
    // the rows on the promise that none of them would.
    let spare = width.checked_sub(border.width()).filter(|avail| *avail > 0);
    let Some(avail) = spare else {
        return fit(line, width);
    };
    fit(line, avail)
        .into_iter()
        .map(|piece| {
            let mut spans: Vec<Span<'static>> = border
                .spans
                .iter()
                .map(|s| Span::styled(s.content.to_string(), s.style))
                .collect();
            spans.extend(piece.spans);
            Line::from(spans)
        })
        .collect()
}

// Write one fitted row into the buffer: one cell per character, styled by
// its span. Wide characters take two cells; combining marks decorate back.
fn write_line(line: &Line<'_>, x: u16, y: u16, buf: &mut Buffer) {
    let mut col = x;
    for span in &line.spans {
        let mut chars = span.content.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                // Same deal as `fit`: outside noise, not a cell. Consume it.
                let mut esc = crate::render::Escape::new();
                for n in chars.by_ref() {
                    if esc.closed(n) {
                        break;
                    }
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
        // The bug this replaced counted lines: "abcdef" is one line and two
        // rows at width 3, so a two-row window that took two lines drew four
        // rows into it and the newest two landed below the area, under the input.
        assert_eq!(shown(&["abcdef", "gh"], 3, 2, 0), vec!["def", "gh"]);
    }

    #[test]
    fn scrolling_up_holds_back_the_newest_rows() {
        assert_eq!(shown(&["a", "b", "c", "d"], 10, 2, 1), vec!["b", "c"]);
    }

    // The walk counts screen rows, so a window whose oldest row starts inside a
    // wrapped line takes the rows of that line it reaches and cuts the rest —
    // the line is not taken whole, and the rows held back are not taken at all.
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
