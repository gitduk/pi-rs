//! What a slash command answered, drawn over the menu until closed — not
//! a transcript notice or a flash, but unread output the user asked for.

use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::ui::Focus;
use super::{Asked, Deed, Ui};
use crate::listing;
use pi_store::icons;
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
        listing::split(&self.content)
            .into_iter()
            .flat_map(|(head, rest)| {
                let indent = UnicodeWidthStr::width(head.as_str());
                // A column too wide to leave the rest half the row wraps whole.
                if indent == 0 || indent * 2 > width {
                    return words(&(head + &rest), width)
                        .into_iter()
                        .map(Line::from)
                        .collect::<Vec<_>>();
                }
                let pad = " ".repeat(indent);
                words(&rest, width - indent)
                    .into_iter()
                    .enumerate()
                    .map(|(at, piece)| {
                        let lead = if at == 0 { head.clone() } else { pad.clone() };
                        Line::from(lead + &piece)
                    })
                    .collect()
            })
            .collect()
    }

    /// The window's `room` rows to draw; the last one names what's above and
    /// below when the reply doesn't fit whole.
    pub fn view(&self, room: usize, width: usize) -> Vec<Line<'static>> {
        let rows = self.rows(width);
        let Some(shown) = window(rows.len(), room) else {
            return rows;
        };
        let start = self.first.min(rows.len() - shown);
        let mut out = rows[start..start + shown].to_vec();
        out.push(Line::from(format!(
            "  {}-{} of {}{}j/k scroll, q close",
            start + 1,
            start + shown,
            rows.len(),
            icons::KEY_NOTE_SEP,
        )));
        out
    }

    /// Move the window `by` rows, `room` being what the menu has for it: the
    /// last row comes to the bottom and no further.
    pub fn scroll(&mut self, by: isize, room: usize, width: usize) {
        let len = self.rows(width).len();
        let last = window(len, room).map_or(0, |shown| len - shown);
        self.first = self.first.saturating_add_signed(by).min(last);
    }
}

// How many of `len` rows a window of `room` shows when they do not all fit:
// one row of it goes to the count. `None` when they fit.
fn window(len: usize, room: usize) -> Option<usize> {
    let room = room.max(1);
    (len > room).then(|| (room - 1).max(1))
}

// Break `text` into rows of at most `width` columns, at spaces where it can;
// a word wider than a row is cut where the row ends. A line that fits is kept
// as it is, runs of spaces and all.
fn words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if UnicodeWidthStr::width(line) <= width {
            out.push(line.to_string());
            continue;
        }
        let mut row = String::new();
        let mut used = 0;
        for word in line.split(' ').filter(|w| !w.is_empty()) {
            let w = UnicodeWidthStr::width(word);
            if used > 0 && (used + 1 + w > width && w <= width || used + 1 >= width) {
                out.push(std::mem::take(&mut row));
                used = 0;
            }
            if used > 0 {
                row.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw > width && used > 0 {
                    out.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(c);
                used += cw;
            }
        }
        out.push(row);
    }
    out
}

impl Ui {
    /// Move the window over the reply, using the `room`/`width` the menu
    /// last showed it — what the last row's bound is measured against.
    pub(super) fn scrolled(&mut self, by: isize, room: usize, width: usize) -> Asked {
        if let Focus::Reply(reply) = &mut self.focus {
            reply.scroll(by, room, width);
        }
        Asked::Own(Deed::Nothing)
    }

    /// Shows a command's reply, replacing any prior one. Empty output opens
    /// nothing, since dismissing an empty overlay is worse than silence.
    pub(super) fn open_reply(&mut self, content: Listing) {
        if !content.is_empty() {
            self.focus = Focus::Reply(Reply::new(content));
        } else if matches!(self.focus, Focus::Reply(_)) {
            self.focus = Focus::Editor;
        }
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
        assert_eq!(rows[2], "  1-2 of 4  ·  j/k scroll, q close");
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
        assert_eq!(rows[2], "  1-2 of 4  ·  j/k scroll, q close");
    }

    // At the end the last line shows above the count, which takes a row of
    // the window: three lines of ten in a room of four.
    #[test]
    fn the_window_stops_at_the_end() {
        let mut reply = reply(10);
        reply.scroll(100, 4, WIDE);
        assert_eq!(reply.first, 7);
        assert_eq!(shown(&reply, 4)[..3], ["line 8", "line 9", "line 10"]);
        reply.scroll(-100, 4, WIDE);
        assert_eq!(reply.first, 0);
    }

    // Can happen via resize, or a press before the frame that sized it.
    // Pulled back to the last screenful, not the top — user's position stays.
    #[test]
    fn a_window_past_the_end_is_pulled_back() {
        let mut reply = reply(10);
        reply.scroll(7, 4, WIDE);
        assert_eq!(reply.first, 7);
        let roomier = shown(&reply, 9);
        assert_eq!(
            roomier[0], "line 3",
            "the last screenful, from the top it can reach"
        );
        assert_eq!(roomier.len(), 9);
    }
}
