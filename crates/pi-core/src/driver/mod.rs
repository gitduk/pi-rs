//! What drives a lane from outside the keyboard: a chat channel, a `/loop`.
//!
//! A driver hears only the end of turns its own lines began; `Drivers`
//! keeps that ledger so the surface need not know which driver cares.

mod channel;
pub mod later;
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
    Later,
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
    /// Spent before the line was sent, by a subagent in the background.
    pub spent: llm::stream::Usage,
    /// Set when the line is the model's own come back, not the user's: what
    /// the screen names it by. The note then rides with the ask.
    pub label: Option<String>,
}

pub struct Drivers {
    channels: Channels,
    loops: Loops,
    later: Arc<later::Table>,
}

impl Drivers {
    pub fn new(channels: Vec<Arc<dyn ::channel::Channel>>) -> Self {
        Self {
            channels: Channels::new(channels),
            loops: Loops::default(),
            later: Arc::default(),
        }
    }

    /// What `later` writes into: the lanes' tool and this driver share it.
    pub fn later(&self) -> Arc<later::Table> {
        self.later.clone()
    }

    /// The line a driver sends `lane`, the checkout at `root`, next. Asked
    /// only when the lane is free and nothing typed is waiting: what the user
    /// says comes first.
    ///
    /// `later` first: it was due at a time, and a loop's next round can wait
    /// one turn where a loop that never settles would hold it off for good.
    pub fn next(&mut self, lane: u64, root: &std::path::Path) -> Option<Next> {
        Some(match self.later.take_due(root) {
            Some(due) => Next {
                line: due.line,
                note: due.note,
                origin: Origin::Later,
                spent: due.spent,
                label: Some(due.label),
            },
            None => {
                let due = self.loops.due(lane)?;
                Next {
                    line: due.goal,
                    note: due.note,
                    origin: Origin::Loop,
                    spent: Default::default(),
                    label: None,
                }
            }
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
            Drive::Later(arg) => Said::Reply(self.later_command(&arg, ctx)),
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
            Origin::Channel(_) | Origin::Typed | Origin::Later => {}
        }
        None
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
}

impl Drivers {
    // `/later`: what is pending here, or `rm <id>` to cancel one.
    fn later_command(&self, arg: &str, ctx: &Ctx) -> Vec<String> {
        let root = ctx.workspace.root();
        match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => {
                let pending = self.later.listing(root);
                if pending.is_empty() {
                    vec!["nothing left for later here".into()]
                } else {
                    pending
                }
            }
            ["rm", id] => match id.trim_start_matches('#').parse() {
                Ok(id) => vec![self.later.cancel(root, id).unwrap_or_else(|e| e)],
                Err(_) => vec![format!("`{id}` is not an id; /later lists them")],
            },
            _ => vec!["/later lists what is pending; /later rm <id> cancels one".into()],
        }
    }
}
