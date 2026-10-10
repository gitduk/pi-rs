//! What drives a lane from outside the keyboard: a job, a `/loop`.
//!
//! A driver hears only the end of turns its own lines began; `Drivers`
//! keeps that ledger so the surface need not know which driver cares.

pub mod jobs;
pub mod looping;

use std::sync::Arc;

use agent::Event;
use tool::Ctx;

use crate::input::Drive;
use looping::Loops;

/// Who sent a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Typed,
    Loop,
    Job,
    /// A person speaking through job `0`: the turn's answer goes back to it.
    Input(u64),
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

#[derive(Default)]
pub struct Drivers {
    loops: Loops,
    jobs: Arc<jobs::Table>,
    // Turns a job's input opened, each with its answer so far.
    owed: Vec<Owed>,
}

struct Owed {
    job: u64,
    lane: u64,
    said: String,
}

impl Drivers {
    /// What `jobs` and every call that outlives its turn write into: the
    /// lanes' tools and this driver share it.
    pub fn jobs(&self) -> Arc<jobs::Table> {
        self.jobs.clone()
    }

    /// The line a driver sends `lane`, the checkout at `root`, next. Asked
    /// only when the lane is free and nothing typed is waiting: what the user
    /// says comes first.
    ///
    /// A job's result first: it was due already, and a loop's next round can
    /// wait one turn where a loop that never settles would hold it off for good.
    pub fn next(&mut self, lane: u64, root: &std::path::Path) -> Option<Next> {
        Some(match self.jobs.take_back(root) {
            Some((_, jobs::Back::Result(due))) => Next {
                line: due.line,
                note: due.note,
                origin: Origin::Job,
                spent: due.spent,
                label: Some(due.label),
            },
            Some((job, jobs::Back::Input { text, note })) => Next {
                line: text,
                note,
                origin: Origin::Input(job),
                spent: Default::default(),
                label: None,
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

    /// Carry out a driver's command, given on `lane`.
    pub fn command(&mut self, drive: Drive, lane: u64, ctx: &Ctx) -> Said {
        match drive {
            Drive::Loop(Some(goal)) => match self.loops.start(lane, goal, ctx) {
                Ok(()) => Said::Nothing,
                Err(why) => Said::Reply(vec![why]),
            },
            Drive::Jobs(arg) => Said::Reply(self.jobs_command(&arg, ctx)),
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
            // Only a model turn ends with an answer to send back: a `!` never
            // reaches `Done`, so its answer would be owed forever.
            Origin::Input(job) if prompt && started => {
                self.owed.push(Owed {
                    job,
                    lane,
                    said: String::new(),
                });
                self.jobs.tell(job, tool::Told::Started);
            }
            Origin::Loop if started => self.loops.ask(lane),
            Origin::Loop => return self.loops.unstarted(lane),
            Origin::Typed | Origin::Job | Origin::Input(_) => {}
        }
        None
    }

    /// One event from any lane's run; each driver keeps its own turns'.
    pub fn observe(&mut self, lane: u64, event: &Event) {
        if let Event::TextDelta(text) = event {
            for owed in self.owed.iter_mut().filter(|o| o.lane == lane) {
                owed.said.push_str(text);
            }
        }
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
        for owed in self.owed.extract_if(.., |o| o.lane == lane) {
            self.jobs
                .tell(owed.job, tool::Told::Reply(reply(owed.said, ended)));
        }
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
        // A job left waiting on a reply would wait for good.
        for owed in self.owed.extract_if(.., |o| !live(o.lane)) {
            self.jobs.tell(
                owed.job,
                tool::Told::Reply(reply(owed.said, &Ended::Stopped)),
            );
        }
        self.loops.retain(live)
    }
}

// A turn's answer as its job is told it: what it said, then how it ended
// when that was not cleanly, so a stop never reads as silence.
fn reply(mut said: String, ended: &Ended) -> String {
    let how = match ended {
        Ended::Done => return said,
        Ended::Stopped | Ended::Unsent => "(stopped)".to_string(),
        Ended::Failed(why) => format!("(failed: {why})"),
    };
    if !said.trim().is_empty() {
        said.push_str("\n\n");
    }
    said.push_str(&how);
    said
}

impl Drivers {
    // `/jobs`: what runs in the background here, or `stop <id>` to end one.
    fn jobs_command(&self, arg: &str, ctx: &Ctx) -> Vec<String> {
        let root = ctx.workspace.root();
        match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => {
                let jobs = self.jobs.listing(root);
                if jobs.is_empty() {
                    vec!["nothing in the background here".into()]
                } else {
                    jobs
                }
            }
            ["stop", id] => match id.trim_start_matches('#').parse() {
                Ok(id) => vec![self.jobs.stop(root, id).unwrap_or_else(|e| e)],
                Err(_) => vec![format!("`{id}` is not an id; /jobs lists them")],
            },
            _ => vec!["/jobs lists what runs in the background; /jobs stop <id> ends one".into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tool::{JobSink as _, Told};

    // The line a person sent through a job opens a turn as if typed; the
    // job hears it begin, then gets the whole answer, cut short or not.
    #[tokio::test]
    async fn a_turn_an_input_opened_is_told_back_to_its_job() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ctx = Ctx::new(tool::Workspace::new(&root).unwrap());
        let mut drivers = Drivers::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let job = jobs::Jobs::new(drivers.jobs()).start(
            root.clone(),
            "wechat".into(),
            Default::default(),
            Some(tx),
        );
        job.input("why did it fail".into());

        let next = drivers.next(1, &root).expect("the input is due");
        assert_eq!(next.line, "why did it fail");
        assert_eq!(next.origin, Origin::Input(job.id()));
        assert!(next.label.is_none(), "read as typed, not relayed");

        drivers.dispatched(next.origin, 1, true, true);
        assert_eq!(rx.try_recv(), Ok(Told::Started));
        drivers.observe(1, &Event::TextDelta("parse.rs".into()));
        drivers.observe(2, &Event::TextDelta("another lane".into()));
        drivers.turn_ended(1, &Ended::Stopped, &ctx, None);
        assert_eq!(
            rx.try_recv(),
            Ok(Told::Reply("parse.rs\n\n(stopped)".into()))
        );
        drivers.turn_ended(1, &Ended::Done, &ctx, None);
        assert!(rx.try_recv().is_err(), "told once");
    }
}
