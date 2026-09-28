//! What a lane's screen shows, and what the loop needs to know about it: the
//! settled rows, the stream still writing them, the queue of what arrived while
//! nobody was looking, and the map from a lane's token to its screen.

use std::time::Instant;

use agent::session::EntryId;

use super::row::Row;
use super::scrollback::{Folds, scrollback_from};
use super::tool::RunTool;
use crate::core::Core;
use crate::core::lane::Lane;
use crate::core::meter::Snapshot;
use crate::input::Intent;
use crate::ui::render::Paint;

// Everything the terminal shows, and nothing the session knows.
/// What one lane looks like on screen: its conversation, the stream filling
/// it, where it is scrolled, and the half-typed line parked when the surface
/// left it. The surface holds one per lane, keyed by the lane's token, so the
/// lane list can reorder and drop lanes without a screen following the wrong
/// one; everything on `Ui` around it is the one terminal, the one keyboard and
/// whatever menu is open over them.
#[derive(Default)]
pub struct View {
    // What this screen shows: a rewind replaces it whole.
    pub(super) surface: Surface,
    // This run's readings and interaction state: it lives and dies with the
    // run, and `arm_view` resets it.
    pub(super) state: State,
    // What arrived while the run was working, kept as intents rather than
    // lines: their fate was settled at the door, and re-reading them on the
    // way out would ask a question that has already been answered.
    // A line waiting for the lane, or a round of the loop this lane is under.
    // The round is not an `Intent`: nothing the door can read produces one, and
    // nothing in `Core` answers one, so it lives with the queue it waits in.
    pub(super) queued: Vec<Queued>,
    // Whether this lane's opening block has been built. A rebuild builds the
    // whole surface, banner not among it, and marks it drawn for that reason —
    // neither an empty scrollback nor a zero `opened` can stand in for "never
    // drawn" — and drawing it a second time would stack two banners on one lane.
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
    // The conversation as the screen holds it: finished rows, oldest first,
    // everything above the editor. Not a projection of the session — it also
    // carries what only the screen ever knew, the banner and every notice a
    // command or a warning left behind, interleaved where they happened.
    pub(super) scrollback: Vec<Row>,
    // The stream still writing the next row, and which kind it is.
    pub(super) stream: Stream,
    // The reasoning folds ledger.
    pub(super) folds: Folds,
    // How many rows the opening block occupies. A theme change replaces
    // exactly those and leaves the conversation under them alone.
    pub(super) opened: usize,
    // Rows the view is scrolled up by. Zero shows the newest rows.
    pub(super) scroll: usize,
    // The rows the scrolled-up view measured last: growth folds into
    // `scroll`. `None` re-bases — a reflow must not move the view.
    pub(super) counted: Option<usize>,
    // The id of the last entry this surface adopted into the scrollback.
    // Entries beyond it are folded in through the A table as they commit, so
    // live never waits on a rebuild to show what happened.
    pub(super) tail: Option<EntryId>,
}

// What this run is doing, as far as the screen knows. It ends with the run:
// no tools running, no clock. What the run has cost is the lane's tally, which
// outlives the screen it was drawn on.
#[derive(Default)]
pub struct State {
    // Calls in flight, plus ended calls whose entries are not adopted yet:
    // their row parks until the commit checks it against what the entry
    // itself derives to, and `abandon_tools` files the rest.
    pub(super) tools: Vec<RunTool>,
    // When the work in flight began, for the segment that times it. A clock
    // and nothing else: whether a run is on is `Lane::turn`'s to say, and one
    // field answering both left every ending path to put the clock back or
    // leave a spinner running over a finished lane.
    pub(super) started: Option<Instant>,
    // Whether this run has produced anything yet — a word, a thought, a call.
    // Once it has, Esc means stop rather than unsend.
    pub(super) committed: bool,
    // The run has been asked to stop and is still winding down.
    pub(super) stopping: bool,
}

// The stream still writing itself into the screen, and the kind of line it
// is completing. The model emits one interleaved sequence — a reasoning
// delta and an answer delta never arrive together — so one buffer with one
// kind is the whole state space: two half lines, one of each kind, cannot
// be written down.
#[derive(Default)]
pub struct Stream {
    // Which pipeline a completed line lands through: reasoning appends into
    // the fold blocks, the answer goes through markdown into the scrollback.
    pub(super) kind: StreamKind,
    // Model output with no newline after it yet. Kept live because it is
    // still being written; a completed line goes straight to scrollback.
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

// A turn that has ended, on its way back to the loop that started it.
//
// The transcript comes home this way rather than through a `JoinHandle`, so
// one channel serves every lane and nothing has to poll a growing list of
// them. `session` is None only when the run panicked and took its copy down.
// What woke the loop this time round. One value out of the select rather than
// a pair of optional locals, so what happened is read in one place.
// One thing waiting for the lane in front to come free.
pub(super) enum Queued {
    // What the door made of a submitted line, and the channel it came from
    // when it was not typed here.
    Line(Intent, Option<&'static str>),
    // A round of the loop this lane is under, to be read like a typed line when
    // its turn comes — which is why the goal is kept as text.
    Round { goal: String, note: String },
}

pub(super) type Views = std::collections::BTreeMap<u64, View>;

// The screen one lane's token names, built on first sight: a lane the core
// opened while the surface was busy with another has none yet. A screen whose
// lane is gone goes unasked for until the next prune.
pub(super) fn view_at(views: &mut Views, token: u64) -> &mut View {
    views.entry(token).or_default()
}

// The screen of the lane in front. Fields rather than `&mut self`, and the lane
// rather than the core behind it: a caller holding this screen still reads and
// writes the rest of the lane it belongs to.
// What a lane's status line is drawn from: the numbers the events carried, and
// the two they cannot say — when this turn started, and what is queued behind
// it.
pub(super) fn snapshot(lane: &Lane, view: &View) -> Snapshot {
    lane.snapshot(
        &view.model,
        view.state.started.map(|s| s.elapsed()),
        view.queued
            .iter()
            .filter(|q| matches!(q, Queued::Line(..)))
            .count(),
    )
}

pub(super) fn front_view<'a>(views: &'a mut Views, lane: &Lane) -> &'a mut View {
    view_at(views, lane.token())
}

// Drop the screens of lanes that are gone. A lane can be removed without the
// surface being told — `/worktree` removes one to leave it — so this reads the
// lane list rather than tracking it.
pub(super) fn prune_views(core: &Core, views: &mut Views) {
    views.retain(|token, _| core.lanes.iter().any(|lane| lane.token() == *token));
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

// The screen of a lane nobody has drawn yet: the banner naming what it stands
// on. Both callers that can be the first to a lane need it — the one that moves
// it in front, and the one that says something into a lane nobody is looking at
// — because a rebuild replaces a screen it finds undrawn, and the row said into
// it would go with the old drawing.
pub(super) fn opened<'a>(views: &'a mut Views, lane: &Lane, paint: &Paint) -> &'a mut View {
    let view = view_at(views, lane.token());
    if !view.drawn {
        *view = View::opening(&lane.resolved.context, paint);
    }
    view
}

impl View {
    /// A lane nobody has said anything in yet: the banner naming what it
    /// stands on, and everything else empty.
    pub fn opening(context: &[String], paint: &Paint) -> Self {
        let scrollback = Row::banner(context, paint);
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
