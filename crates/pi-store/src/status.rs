//! The parts the status line is made of.
//!
//! The names are a config file's: `status` lists these.
//! What each part reads as is the surface's, and is written where it draws.

use serde::{Deserialize, Serialize};

/// One part of a status line. These are the names the config lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Segment {
    Elapsed,
    InOut,
    Speed,
    Cache,
    Cost,
    Ctx,
    Compacted,
    Queued,
    Model,
    Worktree,
}

/// Which parts the status line shows: one list, drawn while a run works and
/// kept under its answer once it ends. A part with nothing to say drops out,
/// which is how one list serves both.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Parts(pub Vec<Segment>);

impl Default for Parts {
    fn default() -> Self {
        Self(default_parts())
    }
}

impl std::ops::Deref for Parts {
    type Target = [Segment];
    fn deref(&self) -> &[Segment] {
        &self.0
    }
}

/// The parts when the config names none.
pub fn default_parts() -> Vec<Segment> {
    vec![
        Segment::Elapsed,
        Segment::InOut,
        Segment::Speed,
        Segment::Cache,
        Segment::Ctx,
        Segment::Compacted,
        Segment::Queued,
        Segment::Cost,
    ]
}
