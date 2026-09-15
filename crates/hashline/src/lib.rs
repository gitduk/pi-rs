mod edits;

pub use edits::{Anchor, Applied, Edit, Landed, Refusal, apply};

/// Name for a file in a report or a refusal, as the views print it.
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

/// Resolves a `whole_block` anchor to the block it names. Injected rather than
/// linked so this crate stays a pure function of its inputs.
pub trait Blocks {
    /// The inclusive 1-based rows of the block at `line`, if there is one.
    /// Both ends: an annotation above the line belongs to what it annotates,
    /// so the start may sit above `line`.
    fn extent_of(&self, path: &str, content: &str, line: usize) -> Option<(usize, usize)>;

    /// Every row a block opens on, in order. A `whole_block` anchor names one
    /// of these by prefix, matched against the line itself, so no per-language
    /// name grammar is needed.
    fn openings(&self, path: &str, content: &str) -> Vec<usize>;

    /// Every opening row with the block it opens, for a caller testing many
    /// rows against the outline at once. The default re-resolves per row.
    fn extents(&self, path: &str, content: &str) -> Vec<(usize, usize)> {
        self.openings(path, content)
            .into_iter()
            .filter_map(|o| self.extent_of(path, content, o))
            .collect()
    }
}

/// For callers with no parser: every `whole_block` anchor is refused.
pub struct NoBlocks;

impl Blocks for NoBlocks {
    fn extent_of(&self, _path: &str, _content: &str, _line: usize) -> Option<(usize, usize)> {
        None
    }

    fn openings(&self, _path: &str, _content: &str) -> Vec<usize> {
        Vec::new()
    }
}
