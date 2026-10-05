//! What a slash command answered, drawn over the menu until closed — not
//! a transcript notice or a flash, but unread output the user asked for.

use crossterm::event::KeyEvent;
use ratatui::text::Line;

use super::screen::fit;
use super::{Asked, Deed, Ui};
use crate::listing;
use pi_store::icons;
use pi_store::keys::Action;
use pi_store::listing::Listing;

/// The rows a command answered with, and where the window over them starts.
pub struct Reply {
    /// Rows rather than lines: the width a row has to fit in is known at the
    /// drawing, not where the answer was built.
    content: Listing,
    // First line drawn, clamped against the menu's room at each press and
    // draw, so a resize can't leave the window past the reply's end.
    first: usize,
}

impl Reply {
    pub fn new(content: Listing) -> Self {
        Self { content, first: 0 }
    }

    /// Reply rows, wrapped to `width`; fitted here so the row count used by
    /// the menu can't miss a wrapped line and draw over the bar.
    fn rows(&self, width: usize) -> Vec<Line<'static>> {
        listing::lines(&self.content)
            .into_iter()
            .flat_map(|line| fit(&Line::from(line), width))
            .collect()
    }

    /// The window's `room` rows to draw; the last one names what's above and
    /// below when the reply doesn't fit whole.
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
                "  {}-{} of {}{}esc close, ↓/↑ scroll",
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

/// Whether a press is the reply's: menu-table keys, plus letters
/// excluded, since a letter it ate would be missing from the line below.
pub(super) fn owns(bound: Option<Action>, _key: KeyEvent) -> bool {
    const OWN: [Action; 6] = [
        Action::MenuNext,
        Action::MenuPrevious,
        Action::MenuDismiss,
        Action::MenuAccept,
        Action::LineSubmit,
        Action::AppCancel,
    ];
    bound.is_some_and(|a| OWN.contains(&a))
}

impl Ui {
    /// What a press does while a reply is up: the menu's own keys, which are
    /// the ones `owns` lets through — nothing else reaches here.
    pub(super) fn reply_key(&mut self, bound: Option<Action>, _key: KeyEvent) -> Asked {
        let room = self.regions.menu.height as usize;
        // Wraps at the drawing's width, not the terminal's: a row that wraps
        // after this count would end up unreachable to scroll to.
        let width = self.screen.usable();
        match bound {
            Some(Action::MenuDismiss | Action::MenuAccept) => self.reply = None,
            Some(Action::MenuNext) => return self.scrolled(1, room, width),
            Some(Action::MenuPrevious) => return self.scrolled(-1, room, width),
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    /// Move the window over the reply, using the `room`/`width` the menu
    /// last showed it — what the last row's bound is measured against.
    pub(super) fn scrolled(&mut self, by: isize, room: usize, width: usize) -> Asked {
        if let Some(reply) = &mut self.reply {
            reply.scroll(by, room, width);
        }
        Asked::Own(Deed::Nothing)
    }

    /// Shows a command's reply, replacing any prior one. Empty output opens
    /// nothing, since dismissing an empty overlay is worse than silence.
    pub(super) fn open_reply(&mut self, content: Listing) {
        self.reply = (!content.is_empty()).then(|| Reply::new(content));
    }
}

#[cfg(test)]
mod tests {
    use super::Reply;
    use pi_store::listing::Listing;

    fn reply(n: usize) -> Reply {
        Reply::new(Listing::say((1..=n).map(|i| format!("line {i}"))))
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
        assert_eq!(rows[2], "  1-2 of 4  ·  esc close, ↓/↑ scroll");
    }

    // A line wider than the surface wraps to more than one row; unwrapped
    // counting would push extra rows into the bar's space.
    #[test]
    fn a_wrapped_row_counts_as_one() {
        let reply = Reply::new(Listing::say(["x".repeat(30), "last".into()]));
        // Ten columns: the long line is three rows of them, the short one is
        // one, and a menu with four rows shows all four.
        assert_eq!(reply.view(4, 10).len(), 4);
        // Three rows of menu: the window holds three, and the third is the
        // count of what it could not hold.
        let tight = reply.view(3, 10);
        let rows: Vec<String> = tight.iter().map(|l| l.to_string()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], "  1-2 of 4  ·  esc close, ↓/↑ scroll");
    }

    #[test]
    fn the_window_stops_at_the_end() {
        let mut reply = reply(10);
        reply.scroll(100, 4, WIDE);
        assert_eq!(reply.first, 6);
        assert_eq!(shown(&reply, 4)[0], "line 7");
        reply.scroll(-100, 4, WIDE);
        assert_eq!(reply.first, 0);
    }

    // Can happen via resize, or a press before the frame that sized it.
    // Pulled back to the last screenful, not the top — user's position stays.
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
