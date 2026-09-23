//! What a slash command answered: the lines it printed, drawn over the menu
//! until they are closed.
//!
//! The only place a command's own words go. Its output is neither a notice in
//! a transcript — none of it is in the session, and none of it reaches the
//! model — nor a flash, which is one row for a second: it is something the
//! user asked for and has not finished reading.
//!
//! It rides the menu's plumbing, like the panel it shares the region with:
//! `MenuNext` / `MenuPrevious` move, `MenuDismiss` and `MenuAccept` close, and
//! the browsing keys `j` / `k` / `q` sit beside them as a fixed vocabulary.

use crossterm::event::KeyEvent;
use ratatui::text::Line;

use super::screen::fit;
use super::{Asked, Deed, Ui};
use crate::store::icons;
use crate::store::keys::{self, Action};

/// The lines a command printed, and where the window over them starts.
pub struct Reply {
    lines: Vec<Line<'static>>,
    // The first line drawn. Clamped against the room the menu has — told to
    // the reply at each press, and again at each draw — so a resize or a
    // shorter terminal cannot leave the window past the end of the reply.
    first: usize,
}

impl Reply {
    pub fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines, first: 0 }
    }

    /// The rows a reply takes on screen: a line wider than the surface is a
    /// row it wraps to, and the window is measured in what the drawing counts.
    /// Fitted here rather than left to `Rows`, as the panel's rows are, because
    /// the room this is given is a row count — a wrapped line is one the count
    /// would miss, and the menu would then draw over the bar.
    fn rows(&self, width: usize) -> Vec<Line<'static>> {
        self.lines
            .iter()
            .flat_map(|line| fit(line, width))
            .collect()
    }

    /// The window's rows: what the reply has to show of itself in `room` rows,
    /// the last of them counting what is above and below when it does not all
    /// fit. A reply that fits is drawn whole and says nothing about keys.
    pub fn view(&self, room: usize, width: usize) -> Vec<Line<'static>> {
        let room = room.max(1);
        let rows = self.rows(width);
        let start = self.first.min(rows.len().saturating_sub(room));
        let end = (start + room).min(rows.len());
        let mut out = rows[start..end].to_vec();
        if start > 0 || end < rows.len() {
            // The window was full, so the row the count takes is one of them:
            // what it names is the row it displaces, plus everything below.
            out.pop();
            let shown = out.len();
            out.push(Line::from(format!(
                "  {}-{} of {}{}esc close, j/k scroll",
                start + 1,
                start + shown,
                rows.len(),
                icons::KEY_NOTE_SEP,
            )));
        }
        out
    }

    /// Move the window `by` rows, `room` being what the menu has for it: the
    /// last row comes to the bottom and no further.
    pub fn scroll(&mut self, by: isize, room: usize, width: usize) {
        let last = self.rows(width).len().saturating_sub(room.max(1));
        self.first = self.first.saturating_add_signed(by).min(last);
    }
}

impl Ui {
    /// What a press does while a reply is up. It is the topmost thing on the
    /// surface, so there is nothing else a key could mean here except the two
    /// the caller has already taken: the line being submitted and `ctrl+c`.
    /// The ways out are the ways the menu's other lists leave — `esc` and
    /// `tab` — with the browsing `q` beside them for the hand already there.
    pub(super) fn reply_key(&mut self, bound: Option<Action>, key: KeyEvent) -> Asked {
        let room = self.regions.menu.height as usize;
        // The width the drawing wraps at, not the terminal's own: a row this
        // counts as one and the painter wraps into two is a row at the end of
        // the reply that nothing can scroll to.
        let width = self.screen.usable();
        // The letters are bare ones, as they are in the panel: with a modifier
        // they are the menu's keys, which is nothing on this screen.
        if let Some(c) = keys::bare_letter(&key) {
            match c {
                'j' => return self.scrolled(1, room, width),
                'k' => return self.scrolled(-1, room, width),
                'q' => {
                    self.reply = None;
                    return Asked::Own(Deed::Nothing);
                }
                _ => {}
            }
        }
        match bound {
            Some(Action::MenuDismiss | Action::MenuAccept) => self.reply = None,
            Some(Action::MenuNext) => return self.scrolled(1, room, width),
            Some(Action::MenuPrevious) => return self.scrolled(-1, room, width),
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    /// Move the window over the reply. `room` and `width` are what the menu
    /// last showed it, which is what the bound on the last row is measured
    /// against.
    pub(super) fn scrolled(&mut self, by: isize, room: usize, width: usize) -> Asked {
        if let Some(reply) = &mut self.reply {
            reply.scroll(by, room, width);
        }
        Asked::Own(Deed::Nothing)
    }

    /// Put what a slash command answered up, over everything else.
    ///
    /// Nothing on it is in the session, so a reply that is gone is gone; one
    /// with no lines is not opened at all — an overlay the user has to dismiss
    /// for nothing is worse than silence, and several commands answer with
    /// nothing when there is nothing to say. It takes down whatever was there
    /// either way: an answer that was not given is not the answer to this line,
    /// and the last one left standing would read as if it were.
    pub(super) fn open_reply(&mut self, lines: impl IntoIterator<Item = impl Into<Line<'static>>>) {
        let lines: Vec<Line<'static>> = lines.into_iter().map(Into::into).collect();
        self.reply = (!lines.is_empty()).then(|| Reply::new(lines));
    }
}

#[cfg(test)]
mod tests {
    use super::Reply;
    use ratatui::text::Line;

    fn reply(n: usize) -> Reply {
        Reply::new((1..=n).map(|i| Line::from(format!("line {i}"))).collect())
    }

    // A width wide enough that nothing in these replies wraps: what is under
    // test here is the window, and `a_wrapped_row_counts_as_one` covers the rest.
    const WIDE: usize = 60;

    fn shown(reply: &Reply, room: usize) -> Vec<String> {
        reply
            .view(room, WIDE)
            .iter()
            .map(|l| l.to_string())
            .collect()
    }

    // A reply that fits is drawn whole and says nothing about keys: the chrome
    // is what tells a short answer from a long one.
    #[test]
    fn one_that_fits_is_drawn_whole() {
        assert_eq!(shown(&reply(3), 5), ["line 1", "line 2", "line 3"]);
    }

    // The window is the room the menu has, and the row the count takes is one
    // of the window's own: a reply one line too long shows one line and says so.
    #[test]
    fn one_that_does_not_fit_names_what_is_below() {
        let rows = shown(&reply(4), 3);
        assert_eq!(rows.len(), 3, "the window is the room it was given");
        assert_eq!(rows[0], "line 1");
        assert_eq!(rows[2], "  1-2 of 4  ·  esc close, j/k scroll");
    }

    // A line wider than the surface is a row it wraps to, and the count is of
    // rows: a reply measured in lines puts more rows into the menu than the
    // menu has, and the rows it pushes out are the bar's.
    #[test]
    fn a_wrapped_row_counts_as_one() {
        let reply = Reply::new(vec![Line::from("x".repeat(30)), Line::from("last")]);
        // Ten columns: the long line is three rows of them, the short one is
        // one, and a menu with four rows shows all four.
        assert_eq!(reply.view(4, 10).len(), 4);
        // Three rows of menu: the window holds three, and the third is the
        // count of what it could not hold.
        let tight = reply.view(3, 10);
        let rows: Vec<String> = tight.iter().map(|l| l.to_string()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], "  1-2 of 4  ·  esc close, j/k scroll");
    }

    // Scrolling stops with the last line on the bottom row: one more press
    // moves nothing, so the window cannot be pushed off the end.
    #[test]
    fn the_window_stops_at_the_end() {
        let mut reply = reply(10);
        reply.scroll(100, 4, WIDE);
        assert_eq!(reply.first, 6);
        assert_eq!(shown(&reply, 4)[0], "line 7");
        reply.scroll(-100, 4, WIDE);
        assert_eq!(reply.first, 0);
    }

    // A window past the end — a resize, or a press made before the frame that
    // sized it — is pulled back to fit rather than drawn off the end. What it
    // is pulled back to is the last screenful, not the top: the window the user
    // moved is the one that stays.
    #[test]
    fn a_window_past_the_end_is_pulled_back() {
        let mut reply = reply(10);
        reply.scroll(7, 4, WIDE);
        assert_eq!(reply.first, 6);
        let roomier = shown(&reply, 9);
        assert_eq!(
            roomier[0], "line 2",
            "the last screenful, from the top it can reach"
        );
        assert_eq!(roomier.len(), 9);
    }
}
