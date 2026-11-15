//! What a lane's screen shows, and what the loop needs to know about it: the
//! settled rows, the stream still writing them, the queue of what arrived while
//! nobody was looking, and the map from a lane's token to its screen.

use std::time::Instant;

use agent::session::EntryId;

use super::call::RunTool;
use super::row::Row;
use super::scrollback::{Folds, Scrollback, scrollback_from};
use crate::render::Paint;
use pi_core::core::Core;
use pi_core::core::lane::Lane;
use pi_core::core::meter::Snapshot;
use pi_core::core::resolve::Resolved;
use pi_core::execution::Request;

/// One lane's screen: conversation, stream, scroll, and parked draft.
/// Keyed by lane token, so lanes can reorder or drop without confusion.
#[derive(Default)]
pub struct View {
    // What this screen shows: a rewind replaces it whole.
    pub(super) surface: Surface,
    // This run's tools and interaction state: it lives and dies with the
    // run, and `arm_view` resets it.
    pub(super) state: State,
    // What arrived while the run worked, kept as intents (not lines): their
    // fate was settled at the door, so re-reading them wouldn't be right.
    pub(super) queued: Queue,
    // Whether the opening banner has been drawn; drawing it twice would
    // stack two banners on one lane.
    pub(super) drawn: bool,
    // The half-typed line parked when the surface last left this lane,
    // waiting in the view to come back to the editor with it.
    pub(super) draft: String,
    // Which theme these rows were painted in. A row holds the colours it
    // was made with, so a new theme reaches it only by being made again.
    pub(super) painted: u64,
}

// What the screen shows: the settled rows, the stream still completing them,
// and the layout between.
#[derive(Default)]
pub struct Surface {
    // Finished rows, oldest first, above the editor. Not a pure session
    // projection — also carries banners and notices only the screen knows.
    pub(super) scrollback: Scrollback,
    // The stream still writing the next row, and which kind it is.
    pub(super) stream: Stream,
    // How the groups of calls and reasoning are folded.
    pub(super) folds: Folds,
    // Whether the first row is the opening banner, which a theme change
    // repaints in its place.
    pub(super) opened: bool,
    // Where it is scrolled to. A page up has its own, so closing it lands
    // back where the transcript was left.
    pub(super) scroll: Scroll,
    // Id of the last entry adopted into scrollback. Entries beyond it fold
    // in as they commit, so live doesn't wait on a rebuild.
    pub(super) tail: Option<EntryId>,
}

// What this run is doing, as far as the screen knows; ends with the run.
// Cost tally lives on the lane, outliving this screen.
#[derive(Default)]
pub struct State {
    // Calls in flight, plus ended calls not yet adopted: parked until the
    // commit checks them, `abandon_tools` files the rest.
    pub(super) tools: Vec<RunTool>,
    // The run this screen is drawing, from `arm_view` until it ends.
    // `Lane::turn` says whether a run is on; this field alone must not
    // decide that.
    pub(super) work: Option<Work>,
}

// One run, as the screen draws it.
pub(super) struct Work {
    // When it began, for the `elapsed` part.
    pub(super) since: Instant,
    pub(super) doing: Doing,
    // It has been asked to stop and is still winding down.
    pub(super) stopping: bool,
    // The retry it is waiting out; any other event ends the wait.
    pub(super) retry: Option<Retry>,
}

impl Work {
    pub(super) fn new(doing: Doing) -> Self {
        Self {
            since: Instant::now(),
            doing,
            stopping: false,
            retry: None,
        }
    }
}

// What kind of run it is, with what only that kind carries.
pub(super) struct Retry {
    pub(super) attempt: usize,
    pub(super) wait: std::time::Duration,
}

pub(super) enum Doing {
    // A prompt's turn. `committed` once it has produced anything — a word,
    // a thought, a call; from then Esc means stop rather than unsend.
    Turn { committed: bool },
    // A `!` command: no prompt to take back.
    Bash,
    // A `/compact`: no figures of its own to show.
    Compact,
}

// The stream still writing into the screen, tagged with which kind of
// line it is completing — reasoning and answer deltas never interleave.
#[derive(Default)]
pub struct Stream {
    // Which pipeline a completed line uses: reasoning folds into blocks,
    // answer goes through markdown into scrollback.
    pub(super) kind: StreamKind,
    // Model output with no trailing newline yet, kept live while still
    // being written; a completed line goes straight to scrollback.
    text: String,
    revision: u64,
    // The answer as last drawn: a frame with nothing new to draw, a spinner
    // tick, draws it again from here rather than from the markdown.
    drawn: std::cell::RefCell<Option<Drawn>>,
}

// The lines an answer drew, and what they were drawn from.
struct Drawn {
    revision: u64,
    width: usize,
    color: bool,
    theme: std::sync::Arc<pi_store::theme::Theme>,
    lines: std::sync::Arc<[ratatui::text::Line<'static>]>,
}

impl Stream {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn push(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.revision = self.revision.wrapping_add(1);
        self.text.push_str(delta);
    }

    pub(super) fn take(&mut self) -> String {
        self.revision = self.revision.wrapping_add(1);
        std::mem::take(&mut self.text)
    }

    pub(super) fn take_line(&mut self) -> Option<String> {
        let at = self.text.find('\n')?;
        self.revision = self.revision.wrapping_add(1);
        Some(self.text.drain(..=at).collect())
    }

    #[cfg(test)]
    pub(super) fn replace(&mut self, text: String) {
        self.revision = self.revision.wrapping_add(1);
        self.text = text;
    }

    // The answer's lines `draw` makes of the text at this width and paint,
    // drawn again only once one of them has changed.
    pub(super) fn answer_lines(
        &self,
        width: usize,
        paint: &Paint,
        draw: impl FnOnce(&str) -> Vec<ratatui::text::Line<'static>>,
    ) -> std::sync::Arc<[ratatui::text::Line<'static>]> {
        let mut drawn = self.drawn.borrow_mut();
        let fresh = drawn.as_ref().is_some_and(|d| {
            d.width == width
                && d.color == paint.color
                && std::sync::Arc::ptr_eq(&d.theme, &paint.theme)
                && d.revision == self.revision
        });
        if !fresh {
            *drawn = Some(Drawn {
                revision: self.revision,
                width,
                color: paint.color,
                theme: paint.theme.clone(),
                lines: draw(&self.text).into(),
            });
        }
        drawn.as_ref().expect("drawn above").lines.clone()
    }
}

#[derive(Default, PartialEq, Eq)]
pub enum StreamKind {
    // The reasoning half, drawn muted and folded to its count line.
    Reasoning,
    // The answer half, drawn through markdown.
    #[default]
    Answer,
}

pub(super) use pi_core::execution::queue::{Pending, Queue, Queued};

pub(super) type Views = std::collections::BTreeMap<u64, View>;

// The screen for a lane's token, built lazily: a newly opened lane has
// none yet. A screen whose lane is gone stays unasked-for until prune.
pub(super) fn view_at(views: &mut Views, token: u64) -> &mut View {
    views.entry(token).or_default()
}

// What a lane's status line is drawn from: what events can't say —
// when the turn started, what's queued behind it. Typed lines are drawn
// under the status line already, so only the unseen rest is counted.
pub(super) fn snapshot(lane: &Lane, view: &View) -> Snapshot {
    lane.snapshot(
        lane.model_name(),
        view.state.work.as_ref().map(|w| w.since.elapsed()),
        view.queued
            .lock()
            .iter()
            .filter(|q| q.line.is_none())
            .count(),
    )
}

// Fields rather than `&mut self`: the caller still reads/writes the
// rest of the lane this screen belongs to.
pub(super) fn front_view<'a>(views: &'a mut Views, lane: &Lane) -> &'a mut View {
    let view = view_at(views, lane.token());
    view.queued = lane.queue().clone();
    view
}

// Drops screens for lanes that are gone. `/worktree` can remove a lane
// without telling the surface, so this reads the lane list instead.
pub(super) fn prune_views(core: &Core, views: &mut Views) -> Vec<Request> {
    let mut cancelled = Vec::new();
    views.retain(|token, view| {
        if core.position_of(*token).is_some() {
            true
        } else {
            cancelled.extend(view.queued.lock().drain().map(|queued| queued.request));
            false
        }
    });
    cancelled
}

/// Where a surface is scrolled to: rows up from the newest, and the row the
/// top of the window opens on so what lands or folds under it leaves it put.
/// Zero and `None` show the newest rows.
///
/// One value rather than two fields: a window is written from one place
/// (`window_mut`) and read in one frame, and the two halves only move together.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct Scroll {
    pub(super) by: usize,
    pub(super) top: Option<super::search::Anchor>,
}

impl Scroll {
    /// On the newest rows, following them as they land.
    pub(super) fn newest() -> Self {
        Self::default()
    }

    /// Whether it shows the newest rows, with nothing held above them.
    pub(super) fn newest_rows(&self) -> bool {
        *self == Self::newest()
    }
}

impl Surface {
    // Back on the newest rows, following them as they land.
    pub(super) fn follow(&mut self) {
        self.scroll = Scroll::newest();
    }

    // The screen a session rebuilds to: the transcript as rows, the fold
    // switch where the user left it, and nothing streaming yet.
    pub(super) fn from(
        session: &agent::session::Session,
        paint: &Paint,
        folded: bool,
        root: &std::path::Path,
    ) -> Self {
        let mut folds = Folds {
            folded,
            last: folded,
            ..Default::default()
        };
        Self {
            scrollback: scrollback_from(session, paint, &mut folds, root),
            folds,
            tail: tail_of(session),
            ..Default::default()
        }
    }
}

// Where a transcript ends: the entry a surface that has drawn all of it has
// adopted. A rebuild and a `!` both park the cursor here.
pub(super) fn tail_of(session: &agent::session::Session) -> Option<EntryId> {
    session.entries().last().map(|e| e.id())
}

// The screen for an undrawn lane, with the opening banner: a rebuild
// replaces an undrawn screen, so any earlier row into it would be lost.
pub(super) fn opened<'a>(views: &'a mut Views, lane: &Lane, paint: &Paint) -> &'a mut View {
    let view = view_at(views, lane.token());
    if !view.drawn {
        *view = View::opening(lane.resolved(), paint);
    }
    view
}

impl View {
    /// A lane nobody has said anything in yet: the banner naming what it
    /// stands on, and everything else empty.
    pub fn opening(resolved: &Resolved, paint: &Paint) -> Self {
        Self {
            drawn: true,
            surface: Surface {
                opened: true,
                scrollback: vec![Row::banner(resolved, paint)].into(),
                ..Default::default()
            },
            ..Self::default()
        }
    }
}
