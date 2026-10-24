//! A string found on a page, or on the transcript with no page up: `/`
//! types it on the input line itself, the line being typed set aside until
//! the query is in or let go, each key bringing up the first match from
//! where the view stood, and `n` / `N` step through the matches once it is
//! in. The keys only ask; the frame answers, the one place that holds every
//! row a page shows. Matches are read off the rows as drawn, so what is
//! counted is what is marked.

use pi_store::keys::{Action, Mode};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::Ui;
use super::browse::Page;
use super::menu::MenuEntry;
use super::mouse::{Regions, Target};
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
    pub(super) fn of(target: Target) -> Option<Self> {
        match target {
            Target::Scrollback(idx, line) => Some(Spot::Row(idx, line)),
            Target::Text(n) => Some(Spot::Text(n)),
            Target::PendingTools(i) | Target::Live(i) => Some(Spot::Live(i)),
            Target::Gap => None,
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
    query: Query,
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
    // Every hit, in page order, as the list offers them.
    lines: Vec<Found>,
}

// Where the query is.
enum Query {
    // Still being typed: the line holds it. What the line held, and the
    // mode, when the query took it over.
    Typing { draft: String, mode: Option<Mode> },
    // In.
    Entered(String),
}

// A hit as the list offers it.
struct Found {
    hit: Hit,
    // Which laid line it is on, and which of every hit it is.
    line: usize,
    first: usize,
    // The row it is on, as drawn.
    show: String,
}

impl Search {
    fn typing(&self) -> bool {
        matches!(self.query, Query::Typing { .. })
    }

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
    // to, unless what was typed is still unanswered or found nothing. Hands
    // back what the line held and its mode, when it was being typed.
    fn enter(&mut self, query: String) -> Option<(String, Option<Mode>)> {
        let was = std::mem::replace(&mut self.query, Query::Entered(query));
        if self.want.is_some() || self.at.is_none() {
            self.want = Some(Want::Typed { entered: true });
        }
        match was {
            Query::Typing { draft, mode } => Some((draft, mode)),
            Query::Entered(_) => None,
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
            spot: Spot::of(*self.row_targets.get(n)?)?,
            row: *self.row_of.get(n)?,
        })
    }

    // A search on what is up, typed on the input line: what it held is set
    // aside, as a lane's is when another is in front, and typing follows.
    pub(super) fn search_open(&mut self, view: &mut View) {
        if self.search_typing() {
            return;
        }
        let top = self.top_place();
        // Held on a hit at the bottom is not following the newest rows.
        let newest = matches!(self.window_mut(view), (0, None));
        let draft = self.editor.take_composing();
        let mode = self.vim;
        self.leave_normal();
        // Once entered, `/` takes the query up again to be edited; esc
        // then puts the view back where this `/` found it.
        match self.search.as_mut() {
            Some(search) => {
                let typed = std::mem::replace(&mut search.query, Query::Typing { draft, mode });
                if let Query::Entered(query) = typed {
                    self.editor.set_line(&query);
                }
                search.from = top;
                search.newest = newest;
            }
            None => {
                self.search = Some(Box::new(Search {
                    query: Query::Typing { draft, mode },
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
        if let Query::Typing { draft, mode } = &search.query {
            self.give_back(draft, *mode);
        }
        self.search_place(view, search.back(), search.newest);
        true
    }

    // The query typed is in: the line given back, the view on its hit; an
    // empty one lets the search go.
    pub(super) fn search_enter(&mut self, view: &mut View) {
        if self.editor.is_empty() {
            self.search_close(view);
            return;
        }
        let query = self.editor.take();
        let Some(search) = self.search.as_mut() else {
            return;
        };
        if let Some((draft, mode)) = search.enter(query) {
            self.give_back(&draft, mode);
        }
        self.list.picked = None;
    }

    // The line as the query found it, in the mode it was in.
    fn give_back(&mut self, draft: &str, mode: Option<Mode>) {
        self.editor.set_line(draft);
        if let Some(mode) = mode {
            self.set_mode(mode);
        }
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
        let s = self.search.as_deref()?;
        let query = self.search_text()?;
        let count = match s.count {
            _ if query.is_empty() => String::new(),
            (at, of) => format!("  {at}/{of}"),
        };
        Some((format!("/{query}"), count))
    }

    // The prompt up to the caret, for the frame to stand it after.
    pub(super) fn search_caret(&self) -> Option<String> {
        self.search.as_deref().filter(|s| s.typing())?;
        Some(format!("/{}", &self.editor.text()[..self.editor.cursor()]))
    }

    // What is searched for: the line while it is typed, else what went in.
    fn search_text(&self) -> Option<&str> {
        let s = self.search.as_deref()?;
        Some(match &s.query {
            Query::Typing { .. } => self.editor.text(),
            Query::Entered(query) => query,
        })
    }

    // A jump made since the last frame lands its hit midway down what the
    // list leaves in sight; the frame after holds the top this one drew.
    pub(super) fn search_land(&mut self, view: &mut View, regions: &Regions) {
        let Some(hit) = (self.search.as_ref())
            .filter(|s| s.placed)
            .and_then(|s| s.at)
        else {
            return;
        };
        let covered =
            (regions.rule.height + regions.menu.height).saturating_sub(regions.gap.height);
        let shown = regions.history.height.saturating_sub(covered);
        *self.window_mut(view).1 = Some(Anchor {
            spot: hit.at.spot,
            rows: hit.at.row as isize - (shown / 2) as isize,
        });
    }

    // Whether the query is still being typed: the caret is on its row.
    pub(super) fn search_typing(&self) -> bool {
        self.search.as_ref().is_some_and(|s| s.typing())
    }

    // What is searched for on what is up, for the frame to mark,
    // and the hit the view is on.
    pub(super) fn search_query(&self) -> Option<(&str, Option<Hit>)> {
        let at = self.search.as_deref()?.at;
        let query = self.search_text().filter(|q| !q.is_empty())?;
        Some((query, at))
    }

    // What a key the table read means while the query is typed: true when
    // it was the search's. Enter puts it in, the line's way out lets it go,
    // and the rest edit the line, which is one line here.
    pub(super) fn search_key(&mut self, view: &mut View, action: Option<Action>) -> bool {
        if !self.search_typing() {
            return false;
        }
        let empty = self.editor.is_empty();
        match action {
            Some(Action::LineSubmit) => self.search_enter(view),
            Some(Action::MenuDismiss | Action::PageClose) => {
                self.search_close(view);
            }
            // Back past the first character: no query, so no search.
            Some(Action::DeleteCharBack | Action::DeleteWordBack | Action::DeleteToLineStart)
                if empty =>
            {
                self.search_close(view);
            }
            Some(Action::InsertNewline) => {}
            _ => return false,
        }
        true
    }

    // The line changed while the query is typed: what it now holds is
    // looked for. Typed text, not presses: none of it starts a pair.
    pub(super) fn search_typed(&mut self, was: &str) {
        let changed = self.editor.text() != was;
        let Some(search) = self.search.as_mut().filter(|s| s.typing()) else {
            return;
        };
        if changed {
            search.want = Some(Want::Typed { entered: false });
        }
        self.disarm();
    }

    // Every hit, while the query is typed: the list over it.
    pub(super) fn search_menu(&self) -> Vec<MenuEntry> {
        match self.search.as_deref() {
            Some(s) if s.typing() => (s.lines.iter())
                .map(|f| MenuEntry::Hit {
                    show: f.show.clone(),
                    k: f.hit.k,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    // The list moved: the view goes to its hit, as the frame lands it.
    pub(super) fn search_follow(&mut self) {
        let picked = self.list.picked;
        let Some(search) = self.search.as_mut().filter(|s| s.typing()) else {
            return;
        };
        let Some(found) = picked.and_then(|p| search.lines.get(p)) else {
            return;
        };
        let (hit, line, first) = (found.hit, found.line, found.first);
        search.bring(hit, line, first);
    }

    // A paste while the query is typed: its first line goes on the query,
    // since no match spans rows.
    pub(super) fn search_paste(&mut self, text: &str) {
        if let Some(search) = self.search.as_mut()
            && search.typing()
        {
            let line = text.lines().next().unwrap_or_default();
            // As the rows searched were drawn: escapes gone, tabs as spaces.
            self.editor.insert_str(&pi_store::text::screen_line(line));
            search.want = Some(Want::Typed { entered: false });
        }
    }

    // `n` or `N`: the next line holding the query, or the one before,
    // round past either end.
    pub(super) fn search_step(&mut self, forward: bool) {
        if let Some(search) = self.search.as_mut()
            && matches!(&search.query, Query::Entered(query) if !query.is_empty())
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
        let query = self.search_text().unwrap_or_default().to_string();
        let mut missed = None;
        let mut listed = None;
        let mut top = None;
        if let Some(search) = self.search.as_mut()
            && let Some(want) = search.want.take()
        {
            // A slash page's own lines only, not the transcript over it.
            let own = |spot: Spot| !slash || matches!(spot, Spot::Text(_));
            // Every match, row by row: a line's index, its row, which of the row's.
            let query = query.as_str();
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
                // The list picks the hit the view was brought to.
                listed = Some(next);
            }
            // A hit is landed by the frame, which knows what is in sight.
            top = search.at.is_none().then(|| (search.back(), search.newest));
            let entered = matches!(want, Want::Typed { entered: true } | Want::Step { .. });
            if search.at.is_none() && entered {
                missed = Some(query.to_string());
            }
        }
        if let Some((top, newest)) = top {
            self.search_place(window, top, newest);
        }
        if let Some(query) = missed {
            self.flash(format!("not found: {query}"));
        }
        if let Some(picked) = listed {
            self.list.picked = picked;
            self.list.top = 0;
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

// Every hit, each the list's own row, in page order.
fn found_lines(hits: &[(usize, usize, usize)], laid: &[Laid]) -> Vec<Found> {
    (hits.iter().enumerate())
        .map(|(p, &(i, r, k))| Found {
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
                _ => format!("{}{}", pi_store::icons::ELLIPSIS, laid[i].rows[r].trim()),
            },
        })
        .collect()
}

/// A drawn row with each place `query` is found in it worn as `look`, the
/// `current`-th as `here`. A muted row's dim does not reach them.
pub(super) fn marked(
    line: Line<'static>,
    query: &str,
    current: Option<usize>,
    look: Style,
    here: Style,
) -> Line<'static> {
    let lift = |s| Style::new().remove_modifier(Modifier::DIM).patch(s);
    let (look, here) = (lift(look), lift(here));
    found_in(&screen::plain(&line), query)
        .into_iter()
        .enumerate()
        .fold(line, |line, (k, (from, to))| {
            let mark = if Some(k) == current { here } else { look };
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
