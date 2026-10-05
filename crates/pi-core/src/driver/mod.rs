//! What drives a lane from outside the keyboard: a chat channel, a `/loop`.
//!
//! A driver hears only the end of turns its own lines began; `Drivers`
//! keeps that ledger so the surface need not know which driver cares.

mod channel;
pub mod looping;

use std::sync::Arc;

use agent::Event;
use tool::Ctx;

use crate::input::Drive;
use channel::Channels;
use looping::Loops;

/// Who sent a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Typed,
    Channel(&'static str),
    Loop,
}

/// How a turn ended, as every driver that began it is told.
pub enum Ended {
    Done,
    // Esc, or a stop asked for another way.
    Stopped,
    // The prompt was taken back before the model said a word.
    Unsent,
    Failed(String),
}

/// What a driver's command answers, and where it goes.
pub enum Said {
    Nothing,
    // The answer to the command, over the editor.
    Reply(Vec<String>),
    // Something that happened to the lane, filed with its transcript.
    Transcript(String),
}

/// A line a driver sends a free lane, and the note that goes before it.
pub struct Next {
    pub line: String,
    pub note: String,
    pub origin: Origin,
}

pub struct Drivers {
    channels: Channels,
    loops: Loops,
    // Where each line steered into a running turn came from, by lane token,
    // in the order said: a run hands back what it never heard bare.
    steered: Vec<(u64, Origin)>,
}

impl Drivers {
    pub fn new(channels: Vec<Arc<dyn ::channel::Channel>>) -> Self {
        Self {
            channels: Channels::new(channels),
            loops: Loops::default(),
            steered: Vec::new(),
        }
    }

    /// The line a driver sends `lane` next. Asked only when the lane is free
    /// and nothing typed is waiting: what the user says comes first.
    pub fn next(&mut self, lane: u64) -> Option<Next> {
        let due = self.loops.due(lane)?;
        Some(Next {
            line: due.goal,
            note: due.note,
            origin: Origin::Loop,
        })
    }

    /// What a channel says, stamped with who said it. Cancel-safe, so the
    /// surface may drop it mid-wait in a `select!`.
    pub async fn inbound(&mut self) -> Option<(Origin, ::channel::Inbound)> {
        let (name, msg) = self.channels.rx.recv().await?;
        Some((Origin::Channel(name), msg))
    }

    /// Carry out a driver's command, given on `lane`.
    pub fn command(&mut self, drive: Drive, lane: u64, ctx: &Ctx) -> Said {
        match drive {
            Drive::Channel(name, cmd) => Said::Reply(self.channels.command(&name, cmd)),
            Drive::Loop(Some(goal)) => match self.loops.start(lane, goal, ctx) {
                Ok(()) => Said::Nothing,
                Err(why) => Said::Reply(vec![why]),
            },
            Drive::Loop(None) => match self.loops.stop(lane) {
                // A loop really ended: that belongs in the transcript.
                Some(said) => Said::Transcript(said),
                None => Said::Reply(vec![
                    "no loop here — /loop <line> runs one again while it keeps changing files"
                        .into(),
                ]),
            },
        }
    }

    /// A line from `origin` was dispatched on `lane`, and `started` a run —
    /// a model turn when `prompt`. What comes back is for the lane.
    pub fn dispatched(
        &mut self,
        origin: Origin,
        lane: u64,
        started: bool,
        prompt: bool,
    ) -> Option<String> {
        match origin {
            // A channel relays model turns only: a `!` never reaches `Done`,
            // so its answer would be owed forever.
            Origin::Channel(name) if prompt && started => self.channels.ask(name, lane),
            Origin::Loop if started => self.loops.ask(lane),
            Origin::Loop => return self.loops.unstarted(lane),
            Origin::Channel(_) | Origin::Typed => {}
        }
        None
    }

    /// A line from `origin` was steered into the turn running on `lane`.
    pub fn steered(&mut self, lane: u64, origin: Origin) {
        self.steered.push((lane, origin));
        if let Origin::Channel(name) = origin {
            self.channels.ask(name, lane);
        }
    }

    /// The run on `lane` is over and `n` steered lines went unheard: who sent
    /// each, in order. The ledger lets go of the lane either way.
    pub fn unheard(&mut self, lane: u64, n: usize) -> Vec<Origin> {
        let said: Vec<Origin> = self
            .steered
            .extract_if(.., |s| s.0 == lane)
            .map(|s| s.1)
            .collect();
        // The mailbox is first in, first out: what went unheard is the tail
        // of what was said, so it lines up with the tail of the ledger.
        let heard = said.len().saturating_sub(n);
        said[heard..]
            .iter()
            .copied()
            .chain(std::iter::repeat(Origin::Typed))
            .take(n)
            .collect()
    }

    /// One event from any lane's run; each driver keeps its own turns'.
    pub fn observe(&mut self, lane: u64, event: &Event) {
        self.channels.observe(lane, event);
    }

    /// A run on `lane` ended. What comes back says why a loop ended, if one
    /// did; `cap` is the config's ceiling on its rounds.
    pub fn turn_ended(
        &mut self,
        lane: u64,
        ended: &Ended,
        ctx: &Ctx,
        cap: Option<usize>,
    ) -> Option<String> {
        self.channels.finish_turn(lane, ended);
        self.loops
            .turn_ended(lane, ended, ctx, cap)
            .and_then(|round| round.ending())
    }

    /// Whether a driver will send `lane` more: it has not finished.
    pub fn holds(&self, lane: u64) -> bool {
        self.loops.active(lane)
    }

    /// End the loops of lanes that are gone, saying which ended.
    pub fn retain(&mut self, live: impl Fn(u64) -> bool) -> Vec<String> {
        self.loops.retain(live)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn steered_lines(&self) -> &[(u64, Origin)] {
        &self.steered
    }
}
