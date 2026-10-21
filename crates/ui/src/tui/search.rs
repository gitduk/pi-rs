//! A string found on a page, or on the transcript with no page up: `/`
//! types it on the bottom row, each key
//! bringing up the first match from where the view stood, and `n` / `N`
//! step through the matches once it is in. The keys only ask; the frame
//! answers, the one place that holds every row a page shows. Matches are
//! read off the rows as drawn, so what is counted is what is marked.

use crossterm::event::{KeyCode, KeyEvent};
use pi_store::keys::{Action, Layers, Surface};
use ratatui::style::Modifier;
use ratatui::text::Line;

use super::Ui;
use super::browse::Page;
use super::editor::Editor;
use super::menu::MenuEntry;
use super::mouse::Target;
use super::screen;
use super::select;
use super::view::View;

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
    pub(super) query: Editor,
    // Still being typed: the keys are the query's, not the line's.
    pub(super) typing: bool,
    // The hit the view was last brought to.
    at: Option<Hit>,
    // Where that hit was among the laid lines, for when its line is gone:
    // the next step goes on from there.
    last: (usize, usize),
    // Which hit that is, from 1, of how many: 0 for none.
    count: (usize, usize),
    // The top row at `/`: the search starts there, and esc goes back.
    from: Option<Place>,
    // The view was on the newest rows at `/`: esc goes back to them, not
    // to the row that was on top then.
    newest: bool,
    // What the next frame is asked to find.
    want: Option<Want>,
    // The top row the view showed after the last jump: a view moved off
    // it since steps from where it is now, not from the hit.
    shown: Option<Place>,
    // A jump made, its view's top for the frame after it to record.
    placed: bool,
    // The lines holding the query, in page order, as the list offers them.
    lines: Vec<Found>,
}

// A line holding the query: its first hit, and how many it holds.
struct Found {
    hit: Hit,
    // Which laid line it is, and which of every hit its first is.
    line: usize,
    first: usize,
    // The row its first hit is on, as drawn, and how often it holds one.
    show: String,
    count: usize,
}

impl Search {
    // The view brought to `hit`, the `first`-th of every hit, on laid `line`.
    fn bring(&mut self, hit: Hit, line: usize, first: usize) {
        self.at = Some(hit);
        self.last = (line, hit.at.row);
        self.count.0 = first + 1;
        self.placed = true;
    }

    // Where esc takes the view: the newest rows, or the top row at `/`.
    fn back(&self) -> Option<Anchor> {
        match self.newest {
            true => None,
            false => self.from.map(Anchor::from),
        }
    }

    // The query let go of as typed: the view stays on the hit it was brought
    // to, unless what was typed is still unanswered or found nothing.
    fn enter(&mut self) {
        self.typing = false;
        if self.want.is_some() || self.at.is_none() {
            self.want = Some(Want::Typed { entered: true });
        }
    }
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

    // A search on what is up, its prompt on the bottom row.
    pub(super) fn search_open(&mut self, view: &mut View) {
        let top = self.top_place();
        // Held on a hit at the bottom is not following the newest rows.
        let newest = matches!(self.window_mut(view), (0, None));
        // Once entered, `/` takes the query up again to be edited; esc
        // then puts the view back where this `/` found it.
        match self.search.as_mut() {
            Some(search) => {
                search.typing = true;
                search.from = top;
                search.newest = newest;
            }
            None => {
                self.search = Some(Box::new(Search {
                    query: Editor::default(),
                    typing: true,
                    at: None,
                    last: (0, 0),
                    count: (0, 0),
                    from: top,
                    newest,
                    want: None,
                    shown: None,
                    placed: false,
                    lines: Vec::new(),
                }))
            }
        }
        // The prompt may take rows off the bottom: a scrolled-up view keeps
        // its top line where it is.
        if !newest {
            *self.window_mut(view).1 = top.map(Anchor::from);
        }
    }

    // The search let go: the view back where `/` found it. False when
    // there was none.
    pub(super) fn search_close(&mut self, view: &mut View) -> bool {
        let Some(search) = self.search.take() else {
            return false;
        };
        self.search_place(view, search.back(), search.newest);
        true
    }

    // The view to `top`; to the newest rows with none, when `/` found it there.
    fn search_place(&mut self, view: &mut View, top: Option<Anchor>, newest: bool) {
        let (scroll, at) = self.window_mut(view);
        *at = top;
        if top.is_none() && newest {
            *scroll = 0;
        }
    }

    // The search's row while there is one: `/` and the query, and which
    // hit of how many the view is on.
    pub(super) fn search_prompt(&self) -> Option<(String, String)> {
        match self.search.as_deref() {
            Some(s) => {
                let count = match s.count {
                    _ if s.query.is_empty() => String::new(),
                    (at, of) => format!("  {at}/{of}"),
                };
                Some((format!("/{}", s.query.text()), count))
            }
            _ => None,
        }
    }

    // The prompt up to the caret, for the frame to stand it after.
    pub(super) fn search_caret(&self) -> Option<String> {
        let query = &self.search.as_deref()?.query;
        Some(format!("/{}", &query.text()[..query.cursor()]))
    }

    // Whether the query is still being typed: the caret is on its row.
    pub(super) fn search_typing(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.typing)
    }

    // What is searched for on what is up, for the frame to mark,
    // and the hit the view is on.
    pub(super) fn search_query(&self) -> Option<(&str, Option<Hit>)> {
        match self.search.as_deref() {
            Some(s) if !s.query.is_empty() => Some((s.query.text(), s.at)),
            _ => None,
        }
    }

    // A key while the query is typed; false leaves it to the key table, so
    // the stop still closes what is up.
    pub(super) fn search_key(&mut self, view: &mut View, key: &KeyEvent) -> bool {
        // The line's own editing keys, as the user bound them, edit the query.
        let edit = self.keys.action(
            super::keys::press_of(key),
            Layers {
                surface: Surface::Editor,
                ..Layers::default()
            },
        );
        let Some(search) = self.search.as_mut().filter(|s| s.typing) else {
            return false;
        };
        let typed = Some(Want::Typed { entered: false });
        let was = search.query.text().to_string();
        let mut dropped = false;
        match (key.code, edit) {
            (KeyCode::Esc, _) => dropped = true,
            (KeyCode::Enter, _) if search.query.is_empty() => dropped = true,
            (KeyCode::Enter, _) => search.enter(),
            // Back past the first character: no query, so no search.
            (
                _,
                Some(Action::DeleteCharBack | Action::DeleteWordBack | Action::DeleteToLineStart),
            ) if search.query.is_empty() => dropped = true,
            (_, Some(Action::LineClear)) => search.query.clear(),
            _ => {
                if !edit.is_some_and(|action| search.query.edit(action)) {
                    let Some(c) = super::keys::bare_letter(key) else {
                        return false;
                    };
                    search.query.insert(c);
                }
            }
        }
        // A caret moved finds nothing new.
        if search.query.text() != was {
            search.want = typed;
        }
        let listed = search.typing && !dropped;
        if dropped {
            self.search_close(view);
        }
        // The list went with the typing: a fresh one starts at its pick.
        if !listed {
            self.picked = None;
        }
        // Typed text, not presses: none of it starts a pair.
        self.last_press = None;
        true
    }

    // The lines holding the query, while it is typed: the list over it.
    pub(super) fn search_menu(&self) -> Vec<MenuEntry> {
        match self.search.as_deref() {
            Some(s) if s.typing => (s.lines.iter())
                .map(|f| MenuEntry::Hit {
                    show: f.show.clone(),
                    note: match f.count {
                        1 => String::new(),
                        n => pi_store::text::times(n),
                    },
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    // The list's pick taken, as Enter takes it.
    pub(super) fn search_accept(&mut self) {
        if let Some(search) = self.search.as_mut() {
            search.enter();
        }
        self.picked = None;
    }

    // The list moved: the view goes to the line it is on now.
    pub(super) fn search_follow(&mut self, view: &mut View) {
        let picked = self.picked;
        let Some(search) = self.search.as_mut().filter(|s| s.typing) else {
            return;
        };
        let Some(found) = picked.and_then(|p| search.lines.get(p)) else {
            return;
        };
        let (hit, line, first) = (found.hit, found.line, found.first);
        search.bring(hit, line, first);
        *self.window_mut(view).1 = Some(Anchor::from(hit.at));
    }

    // A paste while the query is typed: its first line goes on the query,
    // since no match spans rows.
    pub(super) fn search_paste(&mut self, text: &str) {
        if let Some(search) = self.search.as_mut()
            && search.typing
        {
            let line = text.lines().next().unwrap_or_default();
            let line: String = line.chars().filter(|c| !c.is_control()).collect();
            search.query.insert_str(&line);
            search.want = Some(Want::Typed { entered: false });
        }
    }

    // `n` or `N`: the next line holding the query, or the one before,
    // round past either end.
    pub(super) fn search_step(&mut self, forward: bool) {
        if let Some(search) = self.search.as_mut()
            && !search.query.is_empty()
        {
            search.want = Some(Want::Step { forward });
        }
    }

    // Whether the frame must lay every line out for the search.
    pub(super) fn search_due(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.want.is_some())
    }

    // What the keys asked, answered over every line the page lays out: the
    // line the window then opens on.
    pub(super) fn search_answer(&mut self, window: &mut View, laid: &[Laid]) {
        let view = self.view_places();
        let slash = matches!(self.page(), Some(Page::Slash(_)));
        let mut missed = None;
        let mut listed = None;
        let mut top = None;
        if let Some(search) = self.search.as_mut()
            && let Some(want) = search.want.take()
        {
            // A slash page's own lines only, not the transcript over it.
            let own = |spot: Spot| !slash || matches!(spot, Spot::Text(_));
            // Every match, row by row: a line's index, its row, which of the row's.
            let query = search.query.text();
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
            search.count = (0, n);
            search.at = None;
            search.placed = true;
            if let Some(h) = next {
                let (i, r, k) = hits[h];
                let spot = laid[i].spot;
                search.bring(
                    Hit {
                        at: Place { spot, row: r },
                        k,
                    },
                    i,
                    h,
                );
            }
            if let Want::Typed { .. } = want {
                search.lines = found_lines(&hits, laid);
                // The list picks the line the view was brought to.
                listed = Some(next.map(|h| {
                    (search.lines.iter())
                        .rposition(|f| f.first <= h)
                        .unwrap_or(0)
                }));
            }
            top = Some((
                search.at.map(|h| Anchor::from(h.at)).or(search.back()),
                search.newest,
            ));
            let entered = matches!(want, Want::Typed { entered: true } | Want::Step { .. });
            if search.at.is_none() && entered {
                missed = Some(search.query.text().to_string());
            }
        }
        if let Some((top, newest)) = top {
            self.search_place(window, top, newest);
        }
        if let Some(query) = missed {
            self.flash(format!("not found: {query}"));
            // The bar was laid out before this: the next frame says it.
            self.redraw = true;
        }
        if let Some(picked) = listed {
            self.picked = picked;
            self.menu_top = 0;
            // The list was laid out before this too.
            self.redraw = true;
        }
    }

    // After the frame: the search's start, when nothing was drawn at `/`,
    // and the top row a jump left the view on.
    pub(super) fn search_settle(&mut self) {
        let top = self.top_place();
        if let Some(search) = self.search.as_mut() {
            if search.from.is_none() {
                search.from = top;
            }
            if std::mem::take(&mut search.placed) {
                search.shown = top;
            }
        }
    }
}

// Every hit, grouped by the laid line it is on, in page order.
fn found_lines(hits: &[(usize, usize, usize)], laid: &[Laid]) -> Vec<Found> {
    let mut lines: Vec<Found> = Vec::new();
    for (p, &(i, r, k)) in hits.iter().enumerate() {
        match lines.last_mut() {
            Some(f) if f.line == i => f.count += 1,
            _ => lines.push(Found {
                hit: Hit {
                    at: Place {
                        spot: laid[i].spot,
                        row: r,
                    },
                    k,
                },
                line: i,
                first: p,
                // A row a line wrapped onto is marked as the rest of one.
                show: match r {
                    0 => laid[i].rows[r].trim().to_string(),
                    _ => format!("{} {}", pi_store::icons::ELLIPSIS, laid[i].rows[r].trim()),
                },
                count: 1,
            }),
        }
    }
    lines
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
