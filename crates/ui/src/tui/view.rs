//! What a lane's screen shows, and what the loop needs to know about it: the
//! settled rows, the stream still writing them, the queue of what arrived while
//! nobody was looking, and the map from a lane's token to its screen.

use std::time::Instant;

use agent::session::EntryId;

use super::call::RunTool;
use super::row::Row;
use super::scrollback::{Folds, scrollback_from};
use crate::render::Paint;
use pi_core::core::Core;
use pi_core::core::lane::Lane;
use pi_core::core::meter::Snapshot;
use pi_core::core::resolve::Resolved;
use pi_core::driver::Origin;
use pi_core::input::Intent;

/// One lane's screen: conversation, stream, scroll, and parked draft.
/// Keyed by lane token, so lanes can reorder or drop without confusion.
#[derive(Default)]
pub struct View {
    // What this screen shows: a rewind replaces it whole.
    pub(super) surface: Surface,
    // This run's readings and interaction state: it lives and dies with the
    // run, and `arm_view` resets it.
    pub(super) state: State,
    // What arrived while the run worked, kept as intents (not lines): their
    // fate was settled at the door, so re-reading them wouldn't be right.
    pub(super) queued: Vec<Queued>,
    // Whether the opening banner has been drawn; drawing it twice would
    // stack two banners on one lane.
    pub(super) drawn: bool,
    // The model in force. Copied in before the run borrows the agent, which
    // is what puts it out of reach for the rest of the turn.
    pub(super) model: String,
    // The half-typed line parked when the surface last left this lane,
    // waiting in the view to come back to the editor with it.
    pub(super) draft: String,
}

// What the screen shows: the settled rows, the stream still completing them,
// and the layout between.
#[derive(Default)]
pub struct Surface {
    // Finished rows, oldest first, above the editor. Not a pure session
    // projection — also carries banners and notices only the screen knows.
    pub(super) scrollback: Vec<Row>,
    // The stream still writing the next row, and which kind it is.
    pub(super) stream: Stream,
    // How the groups of calls and reasoning are folded.
    pub(super) folds: Folds,
    // How many rows the opening block occupies. A theme change replaces
    // exactly those and leaves the conversation under them alone.
    pub(super) opened: usize,
    // Rows the view is scrolled up by. Zero shows the newest rows.
    pub(super) scroll: usize,
    // The rows the scrolled-up view measured last: growth folds into
    // `scroll`. `None` re-bases — a reflow must not move the view.
    pub(super) counted: Option<usize>,
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
    // When work began, for the timing segment. `Lane::turn` says whether a
    // run is on; this field alone must not decide that.
    pub(super) started: Option<Instant>,
    // Whether this run has produced anything yet — a word, a thought, a call.
    // Once it has, Esc means stop rather than unsend.
    pub(super) committed: bool,
    // The run has been asked to stop and is still winding down.
    pub(super) stopping: bool,
    // The retry the run is waiting out, worded for the status line; any
    // other event means the wait is over.
    pub(super) retry: Option<String>,
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
    pub(super) text: String,
}

#[derive(Default, PartialEq, Eq)]
pub enum StreamKind {
    // The reasoning half, drawn muted and folded to its count line.
    Reasoning,
    // The answer half, drawn through markdown.
    #[default]
    Answer,
}

// One line waiting for the lane in front to come free: what the door made of
// it, and who sent it.
pub(super) struct Queued {
    pub(super) intent: Intent,
    pub(super) origin: Origin,
}

pub(super) type Views = std::collections::BTreeMap<u64, View>;

// The screen for a lane's token, built lazily: a newly opened lane has
// none yet. A screen whose lane is gone stays unasked-for until prune.
pub(super) fn view_at(views: &mut Views, token: u64) -> &mut View {
    views.entry(token).or_default()
}

// What a lane's status line is drawn from: what events can't say —
// when the turn started, what's queued behind it.
pub(super) fn snapshot(lane: &Lane, view: &View) -> Snapshot {
    lane.snapshot(
        &view.model,
        view.state.started.map(|s| s.elapsed()),
        view.queued.len(),
    )
}

// Fields rather than `&mut self`: the caller still reads/writes the
// rest of the lane this screen belongs to.
pub(super) fn front_view<'a>(views: &'a mut Views, lane: &Lane) -> &'a mut View {
    view_at(views, lane.token())
}

// Drops screens for lanes that are gone. `/worktree` can remove a lane
// without telling the surface, so this reads the lane list instead.
pub(super) fn prune_views(core: &Core, views: &mut Views) {
    views.retain(|token, _| core.position_of(*token).is_some());
}

impl Surface {
    // The screen a session rebuilds to: the transcript as rows, the fold
    // switch where the user left it, and nothing streaming yet.
    pub(super) fn from(session: &agent::session::Session, paint: &Paint, folded: bool) -> Self {
        let mut folds = Folds {
            folded,
            last: folded,
            ..Default::default()
        };
        Self {
            scrollback: scrollback_from(session, paint, &mut folds),
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
        let scrollback = Row::banner(resolved, paint);
        let opened = scrollback.len();
        Self {
            drawn: true,
            surface: Surface {
                opened,
                scrollback,
                ..Default::default()
            },
            ..Self::default()
        }
    }
}
