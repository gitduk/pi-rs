//! The parts a status line is made of, and which of them each line shows.
//!
//! The names are a config file's: `status.live` and `status.done` list these.
//! What each part reads as is the surface's, and is written where it draws.

use serde::{Deserialize, Serialize};

/// One part of a status line. These are the names the config lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Elapsed,
    InOut,
    Cache,
    Cost,
    Turns,
    Ctx,
    Compacted,
    Queued,
    Model,
    Worktree,
}

/// Which parts each line shows, as the config states it. An absent list is the
/// default one, so a file that names neither reads as the shipped layout.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lines {
    #[serde(default = "default_live")]
    pub live: Vec<Segment>,
    #[serde(default = "default_done")]
    pub done: Vec<Segment>,
}

impl Default for Lines {
    fn default() -> Self {
        Self {
            live: default_live(),
            done: default_done(),
        }
    }
}

/// The live line when the config names nothing: elapsed, the counts, the cache
/// read so far, context, queued work, and the worktree.
pub fn default_live() -> Vec<Segment> {
    vec![
        Segment::Elapsed,
        Segment::InOut,
        Segment::Cache,
        Segment::Ctx,
        Segment::Queued,
        Segment::Worktree,
    ]
}

/// The same for the done line — `turns · in/out · cached · $cost` as it stood,
/// with context and compaction added.
pub fn default_done() -> Vec<Segment> {
    vec![
        Segment::Turns,
        Segment::InOut,
        Segment::Cache,
        Segment::Ctx,
        Segment::Compacted,
        Segment::Cost,
        Segment::Worktree,
    ]
}
