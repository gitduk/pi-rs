use std::collections::HashMap;

mod apply;
mod parse;

pub use apply::{
    Change, Landed, Plan, apply, first_changed_line, first_shifted_line, unified_patch,
};
pub use parse::{FORMS, Form, parse};

/// Content hash shown as `[path#TAG]`. Recomputing it beats storing a snapshot
/// per read: a file that changed underneath the model no longer matches.
pub fn tag(content: &str) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in content.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{:04X}", (h ^ (h >> 16)) & 0xFFFF)
}

/// `[path#TAG]` — how a view names the file it is showing, and how a patch
/// names the file it edits. Printed here because this is the crate that reads
/// it back: two `format!`s pointing opposite ways is how a view starts printing
/// a header its own parser rejects.
pub fn header(path: &str, tag: &str) -> String {
    format!("[{path}#{tag}]")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinePos {
    At(usize),
    /// From `N*:DOWN` — after the construct opening at N, wherever it closes.
    AfterBlock(usize),
}

/// What a hunk names. `Block` is resolved against the file's syntax before
/// anything is applied, so the applier only ever sees line ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Range {
        start: usize,
        end: usize,
    },
    /// `N*` — the whole construct at N, annotations above it included, so the
    /// range it resolves to may begin above N. One of the three address forms
    /// (`N`, `N-M`, `N*`); `:UP`/`:DOWN` belong to `PUT`, not to an address.
    Block {
        line: usize,
    },
}

/// Resolves `N*` to the construct it names. Injected rather than linked so this
/// crate stays a pure function of its inputs.
pub trait Blocks {
    /// The inclusive 1-based rows of the construct at `line`, if there is one.
    /// Both ends: an annotation above the row belongs to what it annotates, so
    /// the start may sit above `line`.
    fn extent_of(&self, path: &str, content: &str, line: usize) -> Option<(usize, usize)>;
}

/// For callers with no parser. Every `N*` then reports that it cannot resolve.
pub struct NoBlocks;

impl Blocks for NoBlocks {
    fn extent_of(&self, _path: &str, _content: &str, _line: usize) -> Option<(usize, usize)> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Lines(Vec<String>),
    /// `None` is the anonymous register.
    Register(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Replace {
        target: Target,
        body: Body,
    },
    InsertBefore {
        line: usize,
        body: Body,
    },
    InsertAfter {
        at: LinePos,
        body: Body,
    },
    Cut {
        target: Target,
        register: Option<String>,
    },
    Remove,
    Move {
        dest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub path: String,
    pub tag: String,
    pub ops: Vec<Op>,
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

    #[error(
        "{path} is at #{actual}, not the #{expected} you named: it changed since you read it. \
         Re-read it and rebuild the hunks against the new numbers."
    )]
    StaleTag {
        path: String,
        expected: String,
        actual: String,
    },

    #[error("{path} was not loaded; read it before editing it")]
    Missing { path: String },

    #[error(
        "{path} has {len} lines, so {range} names lines that do not exist",
        range = Target::Range { start: *start, end: *end }.to_string()
    )]
    OutOfRange {
        path: String,
        start: usize,
        end: usize,
        len: usize,
    },

    #[error(
        "in {path}, {a_start}-{a_end} and {b_start}-{b_end} both claim line {overlap}. \
         Ranges name original lines, so two hunks may never touch the same one."
    )]
    Overlap {
        path: String,
        a_start: usize,
        a_end: usize,
        b_start: usize,
        b_end: usize,
        overlap: usize,
    },

    #[error(
        "{path} line {line} opens no construct a block op can resolve — a closing \
         brace, a blank line, or a language with no parser. Name the lines with \
         `N-M` instead."
    )]
    NoBlockAt { path: String, line: usize },

    #[error("register `@{name}` was never filled; a CUT must fill it before a PUT pastes it")]
    UnknownRegister { name: String },

    #[error("the anonymous register is empty; add an unlabeled CUT before this paste")]
    EmptyAnonymous,

    #[error("{path}: RM deletes the file, so it cannot share a section with other ops")]
    RemoveWithOps { path: String },
}

pub(crate) type Files<'a> = HashMap<&'a str, &'a str>;
