use std::collections::HashMap;

mod apply;
mod parse;

pub use apply::{Change, Landed, Plan, apply, unified_patch};
pub use parse::parse;

pub fn header(path: &str) -> String {
    format!("[{path}]")
}

/// Content hash for the staleness note: a file that changed underneath the
/// model since its last view gets a note beside the report. Not a gate any
/// more — the anchors are the gate.
pub fn view_hash(content: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in content.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:04X}", (h ^ (h >> 16)) & 0xFFFF)
}

/// The three row states. One operation is a run of marked rows whose delete
/// and keep rows match the file contiguously and exactly; add rows are new
/// content, placed where they sit relative to the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Del,
    Keep,
    Add,
}

/// One marked row, a `*` point anchor, or an `@` construct scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Mark(Mark, String),
    Star(String),
    At(String),
}

/// Resolves `*` rows to the constructs they name. Injected rather than linked
/// so this crate stays a pure function of its inputs.
pub trait Blocks {
    /// The inclusive 1-based rows of the construct at `line`, if there is one.
    /// Both ends: an annotation above the row belongs to what it annotates, so
    /// the start may sit above `line`.
    fn extent_of(&self, path: &str, content: &str, line: usize) -> Option<(usize, usize)>;

    /// Every construct-opening row in the file, in order. A `*` row names one
    /// of these by prefix; the resolver matches the text against what is here,
    /// so no per-language name grammar is needed.
    fn openings(&self, path: &str, content: &str) -> Vec<usize>;
}

/// For callers with no parser. Every `*` then reports that it cannot resolve.
pub struct NoBlocks;

impl Blocks for NoBlocks {
    fn extent_of(&self, _path: &str, _content: &str, _line: usize) -> Option<(usize, usize)> {
        None
    }

    fn openings(&self, _path: &str, _content: &str) -> Vec<usize> {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub path: String,
    /// Blank-line-separated operations, in order. Each group carries its own
    /// `@` scopes; scopes do not survive a group boundary.
    pub groups: Vec<Vec<Row>>,
    /// Where the header sat, for error messages.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub sections: Vec<Section>,
}

impl Patch {
    /// Files the caller must load before [`apply`] can run.
    pub fn paths(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = Vec::new();
        for s in &self.sections {
            if !seen.contains(&s.path.as_str()) {
                seen.push(&s.path);
            }
        }
        seen
    }
}

/// Every message here is read by the model, so each one says what to do next.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("patch line {line}: {what}")]
    Syntax { line: usize, what: String },

    #[error("{path} was not loaded; read it before editing it")]
    Missing { path: String },

    #[error(
        "no match in {path} for the marked rows — the closest real line is \
         {line}: `{text}`. Widen the `=` context or fix the `-` rows; nothing was written."
    )]
    NoMatch {
        path: String,
        line: usize,
        text: String,
    },

    #[error(
        "the marked rows match {n} places in {path} (lines {spans}). Add `=` context \
         rows or an `@` scope to narrow it to one."
    )]
    Ambiguous {
        path: String,
        n: usize,
        spans: String,
    },

    #[error("in {path}, {what}")]
    NoConstruct { path: String, what: String },

    #[error(
        "in {path}, {a_start}-{a_end} and {b_start}-{b_end} overlap. Two operations \
         may never claim the same lines."
    )]
    Overlap {
        path: String,
        a_start: usize,
        a_end: usize,
        b_start: usize,
        b_end: usize,
        overlap: usize,
    },

    #[error("the patch is empty")]
    Empty,
}

pub(crate) type Files<'a> = HashMap<&'a str, &'a str>;
