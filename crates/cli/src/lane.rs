//! One checkout being worked in, and everything the workspace root decides.

use agent::session::Session;
use agent::{Agent, Event, Totals};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use tools::Ctx;

use crate::tui::View;

/// Where this lane's turn stands.
///
/// One value rather than a flag beside an outcome: a lane is idle, or running,
/// or holding the end of a run nobody has seen — never two of those, and the
/// compiler is what should say so.
pub enum Turn {
    /// Nothing running, nothing waiting to be looked at.
    Idle,
    /// A run under way.
    Running {
        /// What `esc` cancels, and only for the lane in front.
        cancel: CancellationToken,
        /// Esc caught the prompt on its way out: stop the run, then unsend it.
        /// Set while the run works, acted on when it ends.
        unsend: bool,
    },
    /// A run that ended while this lane was out of sight, kept until the screen
    /// is looking at it and can show how it went.
    ///
    /// Only for work that can be finished off on the view in front: closing a
    /// partial stream, landing animated tool rows, `say` without a prefix. A
    /// job with none of those settles where it ended instead, or its report
    /// waits on a screen that may never come back.
    Ended {
        out: Result<Totals, agent::AgentError>,
        /// Esc asked for the prompt back while this was still running.
        unsend: bool,
    },
}

/// A `/loop` in force on one lane.
///
/// The loop is invisible to the model: nothing about it reaches the prompt, so
/// a skill under `/loop` runs exactly as it does on its own, every round. What
/// decides another round is the tree, not anything the model says about it.
pub struct Looping {
    /// Re-submitted verbatim each round, read as whatever it was the first
    /// time — a skill stays a skill, prose stays prose.
    pub goal: String,
    pub round: usize,
    /// `ctx.writes_made()` as this round began. The count only grows, so the
    /// difference is how much the round changed.
    marked: u64,
    /// Set when this loop puts a round in the queue, taken when that round
    /// ends. A turn that did not come from here — a line typed between rounds
    /// — also ends, and counting it would move the loop on something it never
    /// ran.
    running: bool,
}

/// What a loop does now that one of its rounds has ended.
pub enum Round {
    /// Run this line again, as round `next`.
    Again { goal: String, next: usize },
    /// The round changed nothing. Where a loop that is fixing things finishes:
    /// a pass that found nothing to do has nothing to do next time either.
    Quiet,
    /// `loop_max_turns` reached, with rounds still changing the tree.
    Capped(usize),
    /// Esc, an error, or a prompt taken back. The loop goes with the run.
    Cut,
}

pub struct Lane {
    /// Shared so a run can take it with it: `Agent::run` needs only `&self`,
    /// and a turn outlives the borrow the surface could lend it. `/model` and
    /// `/reload` write through `Arc::make_mut`, so a run in flight keeps the
    /// agent it started on — which is what they meant all along.
    pub agent: std::sync::Arc<Agent>,
    /// The transcript, or None while a run has it — it is lent out for the
    /// length of a turn. An empty session left in its place would read like a
    /// session with nothing in it, which is a different thing to anyone asking.
    pub session: Option<Session>,
    pub id: String,
    /// When this session began. Held rather than read back: it is set once and
    /// never changes, and going to disk for it made every save parse the whole
    /// transcript to recover one integer.
    pub created: u64,
    /// What this lane's finished runs have cost, in and out and in money.
    /// Per lane, not per surface: with lanes working off-screen the surface
    /// that shows the bill has to be able to say which lane ran it up.
    pub totals: Totals,

    /// What the user calls this session, if anything.
    pub name: Option<String>,
    /// The instruction files this run stands on, named as a person would.
    /// Shown under the banner; rebuilt by `/reload` like everything else the
    /// config decides.
    pub context: Vec<String>,
    /// What this checkout tells an agent, verbatim — the tail of the system
    /// prompt that came from the tree. Held so a subagent rebuilt after
    /// `/model` gets the same one the lane was armed with.
    pub standing: std::sync::Arc<str>,
    /// Carried across turns: the file locks and edit shifts outlive any one run.
    pub ctx: Ctx,
    /// Which worktree the session is in, or None in the repository's own
    /// checkout. Held so the status line can say where the work is landing.
    pub worktree: Option<String>,
    /// Where this lane's runs post what they are doing. One channel per lane,
    /// so an event needs no label to say which screen it belongs on.
    pub events: UnboundedSender<Event>,
    /// The other end. Drained by the loop, into the view when this lane is in
    /// front and into `pending` when it is not.
    pub inbox: UnboundedReceiver<Event>,
    /// What arrived while nobody was looking, in order, waiting to be replayed
    /// into the view the moment this lane comes back to the front.
    pub pending: Vec<Event>,
    /// Where this lane's turn stands.
    pub turn: Turn,
    /// What this lane looks like on screen. Held here rather than parked
    /// beside the surface: the screen shows the lane `Repl::current` names,
    /// so there is one view to draw and no second list to keep in step.
    pub view: View,
    /// What a slash answers to here, and the key map in force. Both are what
    /// this root's config and skills resolved to, so they travel with the lane
    /// rather than with the run — a tree switched back to answers to its own.
    pub keys: std::sync::Arc<crate::keys::Keys>,
    pub commands: std::sync::Arc<Vec<crate::repl::Command>>,
    /// The `/loop` this lane is under, if any.
    pub looping: Option<Looping>,
}

impl Lane {
    /// A lane is born with its own channel: nothing else can post to it, and
    /// nothing it posts can land on another screen.
    pub fn channel() -> (UnboundedSender<Event>, UnboundedReceiver<Event>) {
        unbounded_channel()
    }

    /// Take the end of a run this lane has been holding, if it is holding one.
    ///
    /// Only when it is: a lane still working must keep its `Running`, or the
    /// token `esc` reaches and the request to unsend go with it.
    pub fn take_ended(&mut self) -> Option<(Result<Totals, agent::AgentError>, bool)> {
        match self.turn {
            Turn::Ended { .. } => match std::mem::replace(&mut self.turn, Turn::Idle) {
                Turn::Ended { out, unsend } => Some((out, unsend)),
                _ => None,
            },
            _ => None,
        }
    }

    /// A job has given this lane's transcript back, saying whether it wants
    /// the prompt back too. The one place `Running` ends: a lane left in it
    /// queues every later prompt and never drains.
    pub fn finish(&mut self) -> bool {
        match std::mem::replace(&mut self.turn, Turn::Idle) {
            Turn::Running { unsend, .. } => unsend,
            _ => false,
        }
    }

    /// Whether a run has this lane's transcript right now.
    pub fn is_running(&self) -> bool {
        matches!(self.turn, Turn::Running { .. })
    }

    /// The round this lane's loop queued has begun. Nothing else it runs is
    /// one, so nothing else moves it on.
    pub fn loop_running(&mut self) {
        if let Some(looping) = &mut self.looping {
            looping.running = true;
        }
    }

    /// Put this lane under a loop, marked from where the tree stands now.
    pub fn loop_start(&mut self, goal: String) {
        self.looping = Some(Looping {
            goal,
            round: 0,
            marked: self.ctx.writes_made(),
            running: false,
        });
    }

    /// What the loop in force does now that a round has ended — `None` when
    /// there was no loop. `finished` is whether the run reached its own end
    /// rather than being cut short.
    ///
    /// The loop is taken out and only put back to go round again, so every
    /// ending drops it without a second place to remember that.
    pub fn loop_step(&mut self, finished: bool, cap: Option<usize>) -> Option<Round> {
        if !self.looping.as_ref()?.running {
            return None;
        }
        let mut looping = self.looping.take()?;
        looping.running = false;
        let wrote = self.ctx.writes_made();
        looping.round += 1;
        let changed = wrote > looping.marked;
        looping.marked = wrote;
        Some(if !finished {
            Round::Cut
        } else if !changed {
            Round::Quiet
        } else if cap.is_some_and(|cap| looping.round >= cap) {
            Round::Capped(looping.round)
        } else {
            let goal = looping.goal.clone();
            let next = looping.round + 1;
            self.looping = Some(looping);
            Round::Again { goal, next }
        })
    }
}
