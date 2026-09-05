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
use std::str::Chars;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;

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
        let w = c.width().unwrap_or(0);
        if used + w > width && used > 0 {
            out.push(std::mem::take(&mut piece) + RESET);
            if !sgr.is_empty() {
                piece.push('\x1b');
                piece.push('[');
                piece.push_str(&sgr);
                piece.push('m');
            }
            used = 0;
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
pub fn window<'a>(
    lines: impl DoubleEndedIterator<Item = (Cow<'a, str>, Option<&'a str>)>,
    width: usize,
    room: usize,
    scroll: usize,
) -> (Vec<String>, usize) {
    let want = room + scroll;
    // Newest row first, so the walk can stop without knowing the total.
    let mut back: Vec<String> = Vec::new();
    for (line, border) in lines.rev() {
        if back.len() >= want {
            break;
        }
        back.extend(wrap(border, &line, width).into_iter().rev());
    }
    // Clamp so the window never starts before the first row; when the walk
    // reached the top (back has fewer rows than want), this is the only place
    // the scroll can be corrected.
    let scroll = scroll.min(back.len().saturating_sub(room));
    let mut rows: Vec<String> = back.into_iter().skip(scroll).take(room).collect();
    rows.reverse();
    (rows, scroll)
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

/// The SGR parameters between `\x1b[` and `m`, applied to a style. Invalid
/// parameters silently reset the style to default (SGR 0) rather than being
/// ignored, matching the resilience the terminal itself provides.
pub fn parse_sgr(params: &str, mut style: Style) -> Style {
    let mut it = params
        .split(';')
        .map(|p| p.parse::<u8>().unwrap_or(0))
        .peekable();
    while let Some(p) = it.next() {
        match p {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            8 => style = style.add_modifier(Modifier::HIDDEN),
            9 => style = style.add_modifier(Modifier::CROSSED_OUT),
            38 | 48 => {
                let fg = p == 38;
                match it.next() {
                    Some(5) => {
                        let n = it.next().unwrap_or(0);
                        let color = Color::Indexed(n);
                        style = if fg { style.fg(color) } else { style.bg(color) };
                    }
                    Some(2) => {
                        let r = it.next().unwrap_or(0);
                        let g = it.next().unwrap_or(0);
                        let b = it.next().unwrap_or(0);
                        let color = Color::Rgb(r, g, b);
                        style = if fg { style.fg(color) } else { style.bg(color) };
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    style
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

/// The terminal this screen draws on. An enum rather than a generic so the
/// backend stays out of `Screen`'s type — and out of `Ui`'s and `Tui`'s with
/// it, which is the whole reason the surface was untestable.
enum Term {
    Live(Terminal<CrosstermBackend<Stdout>>),
    /// An in-memory grid. It never enters raw mode or the alternate screen, so
    /// `leave` has nothing to undo — which is what keeps a test off the
    /// terminal the test runner itself is using.
    #[cfg(test)]
    Test(Terminal<ratatui::backend::TestBackend>),
}

pub struct Screen {
    term: Term,
    pub width: u16,
    pub height: u16,
}

impl Screen {
    pub fn new() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;

        // A panic in raw mode otherwise leaves a terminal the user has to
        // `reset`, with the panic message itself unreadable.
        let prior = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = crossterm::execute!(
                std::io::stdout(),
                LeaveAlternateScreen,
                DisableBracketedPaste,
                DisableMouseCapture
            );
            prior(info);
        }));
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        let size = terminal.size()?;
        Ok(Self {
            term: Term::Live(terminal),
            width: size.width,
            height: size.height,
        })
    }

    /// What the in-memory screen currently holds, one string per row with
    /// trailing blanks trimmed — the drawn frame, for a test to read back.
    #[cfg(test)]
    pub fn painted(&self) -> Vec<String> {
        let Term::Test(t) = &self.term else {
            return Vec::new();
        };
        let buf = t.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
                row.trim_end().to_string()
            })
            .collect()
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
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

#[cfg(test)]
mod tests {
    use super::{Rows, parse_sgr};
    use super::{fit, window, wrap};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::Widget;
    use std::borrow::Cow;

    #[test]
    fn a_row_always_stops_short_of_the_last_cell() {
        // wrapped already, and a row that did reach it would be rejoined to the
        // next one by any terminal that reflows on resize.
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
    fn a_wrapped_styled_line_stays_dim_on_every_row() {
        // Without re-opening the SGR at the break, the continuation rows of a
        // coloured line would come out plain: long reasoning lines used to
        // show their wrapped tail as white among grey rows.
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        Rows(&["\x1b[2mabcdef\x1b[0m".into()]).render(Rect::new(0, 0, 3, 3), &mut buf);
        for y in 0..3 {
            assert!(
                buf[(0, y)].style().add_modifier.contains(Modifier::DIM),
                "row {y} lost its style"
            );
        }
    }

    #[test]
    fn the_style_carried_over_a_break_is_the_one_in_force_at_it() {
        // A reset before the width boundary ends the style for good; the tail
        // rows are plain even though the line opened dimmed.
        assert_eq!(
            fit("\x1b[2mab\x1b[0mcd", 2),
            vec!["\x1b[2mab\x1b[0m\x1b[0m", "cd"]
        );
    }

    #[test]
    fn a_zero_colour_component_is_not_a_reset() {
        // `38;2;r;g;b` and `38;5;n` carry zeroes as data; treated as SGR
        // codes they would wipe the style at the break and the tail rows
        // would come out plain.
        let rgb = "\x1b[38;2;255;0;0mabcdef\x1b[0m";
        assert_eq!(
            fit(rgb, 2),
            vec![
                "\x1b[38;2;255;0;0mab\x1b[0m",
                "\x1b[38;2;255;0;0mcd\x1b[0m",
                "\x1b[38;2;255;0;0mef\x1b[0m",
            ]
        );
        let indexed = "\x1b[38;5;0mabcdef\x1b[0m";
        assert_eq!(
            fit(indexed, 2),
            vec![
                "\x1b[38;5;0mab\x1b[0m",
                "\x1b[38;5;0mcd\x1b[0m",
                "\x1b[38;5;0mef\x1b[0m"
            ]
        );
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

    #[test]
    fn sgr_parameters_become_style() {
        let plain = Style::default();
        assert_eq!(parse_sgr("2", plain), plain.add_modifier(Modifier::DIM));
        assert_eq!(
            parse_sgr("38;2;88;166;255", plain),
            plain.fg(Color::Rgb(88, 166, 255))
        );
        assert_eq!(
            parse_sgr("7", plain),
            plain.add_modifier(Modifier::REVERSED)
        );
        // A combined list applies each in turn.
        assert_eq!(
            parse_sgr("1;3;8;38;2;255;136;0", plain).add_modifier,
            Modifier::BOLD | Modifier::ITALIC | Modifier::HIDDEN
        );
        assert_eq!(
            parse_sgr("1;3;8;38;2;255;136;0", plain).fg,
            Some(Color::Rgb(255, 136, 0))
        );
    }

    #[test]
    fn rows_land_styled_cells_in_the_buffer() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        Rows(&["\x1b[2mhello\x1b[0m".into(), "world".into()])
            .render(Rect::new(0, 0, 10, 3), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert!(buf[(0, 0)].style().add_modifier.contains(Modifier::DIM));
        assert_eq!(buf[(4, 1)].symbol(), "d");
    }

    #[test]
    fn rows_wrap_at_the_width_and_stop_at_the_area() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        Rows(&["abcdef".into(), "ghi".into()]).render(Rect::new(0, 0, 4, 2), &mut buf);
        assert_eq!(buf[(2, 0)].symbol(), "c");
        assert_eq!(buf[(0, 1)].symbol(), "d");
        // The area is 2 rows tall; the second line is cut off entirely.
    }

    #[test]
    fn a_combining_mark_attaches_to_its_base_cell() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        Rows(&["cafe\u{301}".into()]).render(Rect::new(0, 0, 10, 1), &mut buf);
        assert_eq!(buf[(3, 0)].symbol(), "e\u{301}");
        assert_eq!(buf[(4, 0)].symbol(), " ");
    }

    #[test]
    fn a_combining_mark_after_a_wide_char_finds_its_base() {
        // A wide character leaves a blank second cell; the mark has to skip
        // it and land on the character itself.
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        Rows(&["中\u{301}".into()]).render(Rect::new(0, 0, 10, 1), &mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "中\u{301}");
    }

    /// The window's rows, plain, for a history of `lines` at width `width`.
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

    /// A bordered line repeats its border on every row it wraps to, so a said
    /// line keeps its rule unbroken instead of cutting it at the first wrap.
    #[test]
    fn a_bordered_line_repeats_its_border_on_every_wrapped_row() {
        // `fit` closes each cut piece with a reset; the border rides on top.
        assert_eq!(
            wrap(Some("▌ "), "abcdef", 4),
            ["▌ ab\x1b[0m", "▌ cd\x1b[0m", "▌ ef"]
        );
        // A painted border spends its escape bytes, not its columns.
        assert_eq!(
            wrap(Some("\x1b[38;2;0;255;255m▌\x1b[0m "), "abcdef", 4),
            [
                "\x1b[38;2;0;255;255m▌\x1b[0m ab\x1b[0m",
                "\x1b[38;2;0;255;255m▌\x1b[0m cd\x1b[0m",
                "\x1b[38;2;0;255;255m▌\x1b[0m ef"
            ]
        );
    }

    /// The three escape scanners — `fit` here, `clip` and `visible_width` in
    /// `render` — each need something different out of a sequence, so they
    /// stay three functions. Nothing structural makes them agree on where one
    /// ends, which is what this is for.
    #[test]
    fn the_escape_scanners_agree_on_where_a_sequence_ends() {
        for s in [
            "\u{1b}7ab",              // two-byte: ESC and a final byte
            "\u{1b}(Bab",             // an intermediate byte before the final one
            "\u{1b}[1mab\u{1b}[0m",    // the ordinary SGR shape
            "\u{1b}[38;5;9mab",
            "\u{1b}]0;title\u{7}ab",   // a control string closed by BEL
            "\u{1b}]0;title\u{1b}\\ab", // and one closed by ST
        ] {
            let width = crate::render::visible_width(s);
            assert_eq!(width, 2, "visible_width disagrees on {s:?}");
            assert_eq!(crate::render::clip(s, width), s, "clip disagrees on {s:?}");
            assert_eq!(fit(s, width).len(), 1, "fit disagrees on {s:?}");
        }
    }

    /// A bordered line that fits gets one row, its border and all.
    #[test]
    fn a_bordered_line_that_fits_stays_one_row() {
        assert_eq!(wrap(Some("▌ "), "abc", 6), ["▌ abc"]);
    }

    /// Every row `wrap` returns fits the width it was given — the promise
    /// `window` counts rows on. A frame with no room for the rule and a column
    /// of text both gives up the rule, not the promise.
    #[test]
    fn a_frame_too_narrow_for_the_rule_drops_it_rather_than_overflowing() {
        for width in 1..=4 {
            for row in wrap(Some("▌ "), "abcdef", width) {
                assert!(
                    crate::render::visible_width(&row) <= width,
                    "{row:?} is wider than {width}"
                );
            }
        }
        assert_eq!(wrap(Some("▌ "), "ab", 2), ["ab"], "the text keeps the frame");
    }

    #[test]
    fn an_empty_history_draws_nothing() {
        assert!(shown(&[], 10, 3, 0).is_empty());
    }
}
