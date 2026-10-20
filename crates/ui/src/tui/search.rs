//! A string found on a page: `/` types it on the bottom row, each key
//! bringing up the first match from where the view stood, and `n` / `N`
//! step through the matches once it is in. The keys only ask; the frame
//! answers, the one place that holds every row a page shows. Matches are
//! read off the rows as drawn, so what is counted is what is marked.

use crossterm::event::{KeyCode, KeyEvent};
use pi_store::keys::{Action, Layers, Surface};
use ratatui::style::Modifier;
use ratatui::text::Line;

use super::Ui;
use super::browse::{Page, Read};
use super::mouse::Target;
use super::screen;
use super::select;
use super::ui::Focus;

/// A line of what a page shows, by where it comes from, so it is the same
/// line from one frame to the next. Ordered as a page lays them out.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Spot {
    Row(usize, usize),
    Live(usize),
    Text(usize),
}

impl Spot {
    pub(super) fn of(target: Target) -> Self {
        match target {
            Target::Scrollback(idx, line) => Spot::Row(idx, line),
            Target::Text(n) => Spot::Text(n),
            Target::PendingTools(i) | Target::Live(i) => Spot::Live(i),
        }
    }
}

/// A screen row of a page: its line, and which row of the line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Place {
    pub(super) spot: Spot,
    pub(super) row: usize,
}

/// Where a scrolled-up window opens: `rows` down from the first row of the
/// line at `spot`, above it when negative and past its end when beyond it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Anchor {
    pub(super) spot: Spot,
    pub(super) rows: isize,
}

impl From<Place> for Anchor {
    fn from(place: Place) -> Self {
        Anchor {
            spot: place.spot,
            rows: place.row as isize,
        }
    }
}

/// One match: the row it is on, and which of that row's matches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Hit {
    pub(super) at: Place,
    pub(super) k: usize,
}

/// A line the frame lays out, as a search reads it: its rows as drawn.
pub(super) struct Laid {
    pub(super) spot: Spot,
    pub(super) rows: Vec<String>,
}

impl Laid {
    pub(super) fn of(spot: Spot, rows: Vec<Line<'static>>) -> Self {
        Laid {
            spot,
            rows: rows.iter().map(screen::plain).collect(),
        }
    }
}

pub(super) struct Search {
    pub(super) query: String,
    // Still being typed: the keys are the query's, not the pager's.
    pub(super) typing: bool,
    // The hit the view was last brought to.
    at: Option<Hit>,
    // Where that hit was among the laid lines, for when its line is gone:
    // the next step goes on from there.
    last: (usize, usize),
    // Which hit that is, from 1, of how many: 0 for none.
    count: (usize, usize),
    // The top row at `/`: the search starts there, and esc goes back. The
    // first frame fills it when the page opened with the search.
    from: Option<Place>,
    // What the next frame is asked to find.
    want: Option<Want>,
    // The page came up with the search, so it goes with a search let go.
    brought: bool,
    // The top row the view showed after the last jump: a view moved off
    // it since steps from where it is now, not from the hit.
    shown: Option<Place>,
    // A jump made, its view's top for the frame after it to record.
    placed: bool,
}

#[derive(Clone, Copy)]
enum Want {
    // The query changed; entered too, so a miss is said.
    Typed { entered: bool },
    // `n`, or `N`.
    Step { forward: bool },
}

impl Ui {
    // The view's first and last rows, by the targets the last frame drew.
    fn view_places(&self) -> Option<(Place, Place)> {
        let last = self.place_at(self.row_targets.len().checked_sub(1)?)?;
        Some((self.top_place()?, last))
    }

    // The top row the last frame drew, part of its line cut off above or not.
    // A slash page's top is its own text's, the transcript under it aside.
    pub(super) fn top_place(&self) -> Option<Place> {
        let n = match self.page() {
            Some(Page::Slash(_)) => {
                (self.row_targets.iter()).position(|t| matches!(t, Target::Text(_)))?
            }
            // Under the pinned head: the window scrolls the rest, if any.
            Some(Page::Preview(_)) => (self.row_targets.iter())
                .position(|t| !matches!(t, Target::Text(0)))
                .unwrap_or(0),
            _ => 0,
        };
        self.place_at(n)
    }

    fn place_at(&self, n: usize) -> Option<Place> {
        Some(Place {
            spot: Spot::of(*self.row_targets.get(n)?),
            row: *self.row_of.get(n)?,
        })
    }

    // A search on the page that is up, its prompt on the bottom row.
    pub(super) fn search_open(&mut self) {
        let top = self.top_place();
        if let Focus::Read(read) = &mut self.focus {
            // Once entered, `/` takes the query up again to be edited; esc
            // then puts the view back where this `/` found it.
            if let Some(search) = read.search.as_mut() {
                search.typing = true;
                search.from = top;
                read.top = top.map(Anchor::from);
                return;
            }
            read.search = Some(Box::new(Search {
                query: String::new(),
                typing: true,
                at: None,
                last: (0, 0),
                count: (0, 0),
                from: top,
                want: None,
                brought: false,
                shown: None,
                placed: false,
            }));
            // The prompt takes a row off the bottom: the top line stays put.
            read.top = top.map(Anchor::from);
        }
    }

    // The conversation alone, searched: `/` from the line in Normal.
    pub(super) fn search_conversation(&mut self) {
        self.browse();
        self.search_open();
        // The last frame was the line's, its rows not the page's.
        if let Focus::Read(Read {
            search: Some(search),
            top,
            ..
        }) = &mut self.focus
        {
            search.from = None;
            search.brought = true;
            *top = None;
        }
    }

    // The search's row while there is one: `/` and the query, and which
    // hit of how many the view is on.
    pub(super) fn search_prompt(&self) -> Option<(String, String)> {
        match &self.focus {
            Focus::Read(Read {
                search: Some(s), ..
            }) => {
                let count = match s.count {
                    _ if s.query.is_empty() => String::new(),
                    (at, of) => format!("  {at}/{of}"),
                };
                Some((format!("/{}", s.query), count))
            }
            _ => None,
        }
    }

    // Whether the query is still being typed: the caret is on its row.
    pub(super) fn search_typing(&self) -> bool {
        matches!(&self.focus, Focus::Read(Read { search: Some(s), .. }) if s.typing)
    }

    // What is searched for on the page that is up, for the frame to mark,
    // and the hit the view is on.
    pub(super) fn search_query(&self) -> Option<(&str, Option<Hit>)> {
        match &self.focus {
            Focus::Read(Read {
                search: Some(s), ..
            }) if !s.query.is_empty() => Some((&s.query, s.at)),
            _ => None,
        }
    }

    // A key while the query is typed; false leaves it to the key table, so
    // the stop still closes the page.
    pub(super) fn search_key(&mut self, key: &KeyEvent) -> bool {
        // The line's own editing keys, as the user bound them, edit the query.
        let edit = self.keys.action(
            super::keys::press_of(key),
            Layers {
                surface: Surface::Editor,
                ..Layers::default()
            },
        );
        let Focus::Read(read) = &mut self.focus else {
            return false;
        };
        let Some(search) = read.search.as_mut().filter(|s| s.typing) else {
            return false;
        };
        let typed = Some(Want::Typed { entered: false });
        let mut dropped = false;
        match (key.code, edit) {
            (KeyCode::Esc, _) => dropped = true,
            (KeyCode::Enter, _) if search.query.is_empty() => dropped = true,
            (KeyCode::Enter, _) => {
                search.typing = false;
                search.want = Some(Want::Typed { entered: true });
            }
            // Back past the first character: no query, so no search.
            (_, Some(Action::DeleteCharBack)) if search.query.is_empty() => dropped = true,
            (_, Some(Action::DeleteCharBack)) => {
                search.query.pop();
                search.want = typed;
            }
            (_, Some(Action::DeleteWordBack)) => {
                let end = search.query.trim_end().len();
                let start = search.query[..end]
                    .char_indices()
                    .rfind(|(_, c)| c.is_whitespace())
                    .map_or(0, |(i, c)| i + c.len_utf8());
                search.query.truncate(start);
                search.want = typed;
            }
            (_, Some(Action::DeleteToLineStart | Action::LineClear)) => {
                search.query.clear();
                search.want = typed;
            }
            _ => match super::keys::bare_letter(key) {
                Some(c) => {
                    search.query.push(c);
                    search.want = typed;
                }
                None => return false,
            },
        }
        if dropped {
            if search.brought {
                self.focus = Focus::Editor;
            } else {
                read.top = search.from.map(Anchor::from);
                read.search = None;
            }
        }
        // Typed text, not presses: none of it starts a pair.
        self.last_press = None;
        true
    }

    // A paste while the query is typed: its first line goes on the query,
    // since no match spans rows.
    pub(super) fn search_paste(&mut self, text: &str) {
        if let Focus::Read(Read {
            search: Some(search),
            ..
        }) = &mut self.focus
            && search.typing
        {
            let line = text.lines().next().unwrap_or_default();
            search
                .query
                .extend(line.chars().filter(|c| !c.is_control()));
            search.want = Some(Want::Typed { entered: false });
        }
    }

    // `n` or `N`: the next line holding the query, or the one before,
    // round past either end.
    pub(super) fn search_step(&mut self, forward: bool) {
        if let Focus::Read(Read {
            search: Some(search),
            ..
        }) = &mut self.focus
            && !search.query.is_empty()
        {
            search.want = Some(Want::Step { forward });
        }
    }

    // Whether the frame must lay every line out for the search.
    pub(super) fn search_due(&self) -> bool {
        match &self.focus {
            Focus::Read(read) => read.search.as_ref().is_some_and(|s| s.want.is_some()),
            _ => false,
        }
    }

    // What the keys asked, answered over every line the page lays out: the
    // line the window then opens on.
    pub(super) fn search_answer(&mut self, laid: &[Laid]) {
        let view = self.view_places();
        let Focus::Read(read) = &mut self.focus else {
            return;
        };
        let mut missed = None;
        if let Some(search) = &mut read.search
            && let Some(want) = search.want.take()
        {
            // A slash page's own lines only, not the transcript over it.
            let own =
                |spot: Spot| !matches!(read.page, Page::Slash(_)) || matches!(spot, Spot::Text(_));
            // Every match, row by row: a line's index, its row, which of the row's.
            let query = &search.query;
            let hits: Vec<(usize, usize, usize)> = laid
                .iter()
                .enumerate()
                .filter(|(_, l)| own(l.spot))
                .flat_map(|(i, l)| {
                    l.rows.iter().enumerate().flat_map(move |(r, text)| {
                        (0..found_in(text, query).len()).map(move |k| (i, r, k))
                    })
                })
                .collect();
            let index = |p: Place| {
                laid.iter()
                    .position(|l| l.spot == p.spot)
                    .map(|i| (i, p.row))
            };
            let from = search.from.and_then(index).unwrap_or((0, 0));
            let cur = search.at.and_then(|h| {
                let (i, r) = index(h.at)?;
                hits.iter().position(|&x| x == (i, r, h.k))
            });
            let n = hits.len();
            let first_from = |at: (usize, usize)| {
                hits.iter()
                    .position(|&(i, r, _)| (i, r) >= at)
                    .or((n > 0).then_some(0))
            };
            // Moved off the last jump (`gg`, a scroll): from the view's top
            // down, or its bottom up; the hit left on that very row is passed.
            let moved = view
                .filter(|(top, _)| search.shown.is_some_and(|s| s != *top))
                .and_then(|(top, bottom)| Some((index(top)?, index(bottom)?)));
            let past =
                |p: usize, at: (usize, usize), edge: (usize, usize)| at != edge || Some(p) != cur;
            let next = match (want, moved) {
                (Want::Step { forward: true }, Some((top, _))) => hits
                    .iter()
                    .enumerate()
                    .position(|(p, &(i, r, _))| (i, r) >= top && past(p, (i, r), top))
                    .or((n > 0).then_some(0)),
                (Want::Step { forward: false }, Some((_, bottom))) => hits
                    .iter()
                    .enumerate()
                    .rposition(|(p, &(i, r, _))| (i, r) <= bottom && past(p, (i, r), bottom))
                    .or(n.checked_sub(1)),
                _ => None,
            };
            let next = next.or_else(|| match (want, cur) {
                (Want::Step { forward: true }, Some(c)) => Some((c + 1) % n),
                (Want::Step { forward: false }, Some(c)) => Some((c + n - 1) % n),
                // The hit's line is gone: on from where it was.
                (Want::Step { forward: true }, None) if search.at.is_some() => hits
                    .iter()
                    .position(|&(i, r, _)| (i, r) > search.last)
                    .or((n > 0).then_some(0)),
                (Want::Step { forward: false }, None) if search.at.is_some() => hits
                    .iter()
                    .rposition(|&(i, r, _)| (i, r) < search.last)
                    .or(n.checked_sub(1)),
                _ => first_from(from),
            });
            search.at = next.map(|h| {
                let (i, r, k) = hits[h];
                Hit {
                    at: Place {
                        spot: laid[i].spot,
                        row: r,
                    },
                    k,
                }
            });
            if let Some(h) = next {
                search.last = (hits[h].0, hits[h].1);
            }
            search.count = (next.map_or(0, |h| h + 1), n);
            search.placed = true;
            read.top = search.at.map(|h| h.at).or(search.from).map(Anchor::from);
            let entered = matches!(want, Want::Typed { entered: true } | Want::Step { .. });
            if search.at.is_none() && entered {
                missed = Some(search.query.clone());
            }
        }
        if let Some(query) = missed {
            self.flash(format!("not found: {query}"));
            // The bar was laid out before this: the next frame says it.
            self.redraw = true;
        }
    }

    // After the frame: the search's start, when the page opened with it,
    // and the top row a jump left the view on.
    pub(super) fn search_settle(&mut self) {
        let top = self.top_place();
        if let Focus::Read(Read {
            search: Some(search),
            ..
        }) = &mut self.focus
        {
            if search.from.is_none() {
                search.from = top;
            }
            if std::mem::take(&mut search.placed) {
                search.shown = top;
            }
        }
    }
}

/// A drawn row with each place `query` is found in it underlined, the
/// `current`-th reversed instead.
pub(super) fn marked(line: Line<'static>, query: &str, current: Option<usize>) -> Line<'static> {
    found_in(&screen::plain(&line), query)
        .into_iter()
        .enumerate()
        .fold(line, |line, (k, (from, to))| {
            let mark = if Some(k) == current {
                Modifier::REVERSED
            } else {
                Modifier::UNDERLINED
            };
            select::modified(line, from, to, mark)
        })
}

// Where `query` is in `text`, as cell ranges.
fn found_in(text: &str, query: &str) -> Vec<(usize, usize)> {
    let cells = |b: usize| pi_store::text::width(&text[..b]);
    (pi_store::text::found(text, query).into_iter())
        .map(|r| (cells(r.start), cells(r.end)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn found_counts_cells() {
        assert_eq!(found_in("中文 Foo foo", "foo"), vec![(5, 8), (9, 12)]);
    }
}
