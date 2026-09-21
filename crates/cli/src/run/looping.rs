//! A `/loop` in force on one lane: the same line, submitted round after round
//! until the tree stops changing.
//!
//! What decides another round is the tree, never the model. Asked of the model
//! it would be answered every time; measured, a round that changed nothing is
//! the end of the loop and not a matter of opinion.

use tools::Ctx;

/// A `/loop` in force on one lane.
///
/// What decides another round is the tree, never the model: the loop keeps a
/// content fingerprint of the tree and stops when a round stops changing it.
pub struct Looping {
    /// Re-submitted verbatim each round, read as whatever it was the first
    /// time — a skill stays a skill, prose stays prose.
    pub goal: String,
    pub round: usize,
    /// Read into every round's prompt: how far the loop has got, what it has
    /// changed so far, and the standing licence to change nothing.
    pub note: String,
    // Fingerprints the tree has worn, oldest first, the starting state
    // included. The last one is the round that just ended; an earlier hit
    // means a round undid its way back.
    seen: Vec<String>,
    // The written tree as the last round left it, one entry per path. The
    // next round is diffed against this, path by path.
    prev: TreeState,
    // Consecutive rounds that changed fewer than `THIN_CHANGES` lines.
    thin: usize,
    // Lines changed since the loop began, fed into the next round's prompt.
    changed: usize,
    // Set when this loop puts a round in the queue, taken when that round
    // ends. A turn that did not come from here — a line typed between rounds
    // — also ends, and counting it would move the loop on something it never
    // ran.
    running: bool,
}

// A round that moves fewer lines than this is below the noise floor; that
// many in a row end the loop. Tuned for simplify/review, which converge.
const THIN_CHANGES: usize = 5;
// Consecutive thin rounds that end the loop.
const THIN_ROUNDS: usize = 2;

// One written path as the loop last saw it: the bytes, and the stat that
// says whether they can be trusted next round without reading them again.
struct TreeFile {
    bytes: Vec<u8>,
    len: u64,
    mtime: Option<std::time::SystemTime>,
}

// The written tree, one entry per path.
type TreeState = std::collections::BTreeMap<std::path::PathBuf, TreeFile>;

// The tree as the loop measures it: a content fingerprint over every written
// path, plus the bytes to diff the next round against. A path whose stat is
// unchanged since `prev` keeps its cached bytes — reading every file again
// every round is work the diff will throw away.
fn tree_mark(ctx: &Ctx, prev: &TreeState) -> (String, TreeState) {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut tree = TreeState::new();
    for path in ctx.writes() {
        let rel = path.strip_prefix(ctx.workspace.root()).unwrap_or(&path);
        for b in rel.to_string_lossy().as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100_0000_01b3);
        let meta = std::fs::metadata(&path).ok();
        let len = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let fresh = prev
            .get(&path)
            .is_some_and(|f| f.len == len && f.mtime == mtime);
        // A written path that vanished reads as empty: the removal still
        // changes the tree, and the next round sees it as deleted.
        let bytes = if fresh {
            prev[&path].bytes.clone()
        } else {
            std::fs::read(&path).unwrap_or_default()
        };
        for b in &bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        tree.insert(path, TreeFile { bytes, len, mtime });
    }
    (format!("{h:016x}"), tree)
}

fn count_changes(prev: &[u8], now: &[u8]) -> usize {
    let mut a: Vec<&[u8]> = prev.split(|b| *b == b'\n').collect();
    let mut b: Vec<&[u8]> = now.split(|b| *b == b'\n').collect();
    if a.last().is_some_and(|l| l.is_empty()) {
        a.pop();
    }
    if b.last().is_some_and(|l| l.is_empty()) {
        b.pop();
    }
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return n + m;
    }
    if n.saturating_mul(m) > 250_000 {
        // Past the LCS budget: every positionally equal line is one kept, and
        // every remaining line of either side is one remove or add. The count
        // is an upper bound on the edit distance — a same-sized rewrite is
        // never reported as zero change, and a true no-op is still zero.
        let kept = a.iter().zip(&b).filter(|(x, y)| x == y).count();
        return (n + m) - 2 * kept;
    }
    // Longest common subsequence, two rolling rows: a kept line is one fewer
    // edit, every other line is either added or removed.
    let mut above = vec![0usize; m + 1];
    let mut row = vec![0usize; m + 1];
    for i in 1..=n {
        for j in 1..=m {
            row[j] = if a[i - 1] == b[j - 1] {
                above[j - 1] + 1
            } else {
                above[j].max(row[j - 1])
            };
        }
        std::mem::swap(&mut above, &mut row);
    }
    n + m - 2 * above[m]
}

/// What a loop does now that one of its rounds has ended.
pub enum Round {
    // Run this line again, as round `next`.
    Again { goal: String, next: usize },
    // The round changed nothing. Where a loop that is fixing things finishes:
    // a pass that found nothing to do has nothing to do next time either.
    Quiet,
    // The tree is back at a fingerprint it wore earlier — a round undid its
    // own work. Such a loop would seesaw forever, so it stops.
    Oscillating,
    // Several rounds in a row moved fewer than `THIN_CHANGES` lines. The
    // fingerprint cannot catch a round that keeps nibbling, so this does.
    Thin,
    // `loop_max_turns` reached, with rounds still changing the tree.
    Capped(usize),
    // Esc, an error, or a prompt taken back. The loop goes with the run.
    Cut,
}

impl Looping {
    /// The round beginning is the one this loop queued. Nothing else the lane
    /// runs is one, so nothing else may move the loop on.
    pub fn mark_running(&mut self) {
        self.running = true;
    }

    /// Whether the round now ending is the one this loop queued.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// A loop over `goal`, marked from where the tree stands now.
    pub fn start(ctx: &Ctx, goal: String) -> Self {
        let (seen, prev) = tree_mark(ctx, &TreeState::new());
        Self {
            goal,
            round: 0,
            note: String::new(),
            seen: vec![seen],
            prev,
            thin: 0,
            changed: 0,
            running: false,
        }
    }

    /// What the loop does now that a round has ended. `finished` is whether
    /// the run reached its own end rather than being cut short.
    pub fn step(&mut self, ctx: &Ctx, finished: bool, cap: Option<usize>) -> Round {
        self.running = false;
        self.round += 1;
        // A cut round ends the loop without measuring the tree: esc may have
        // stopped the run mid-write, and where the tree stands now is not a
        // judgement anyone asked for.
        if !finished {
            return Round::Cut;
        }
        let (fingerprint, now) = tree_mark(ctx, &self.prev);
        let mut change = 0usize;
        // One iterator, not two chained: a path present in both maps would
        // be visited twice and its diff counted twice.
        for path in now.keys() {
            let a = self.prev.get(path).map(|f| f.bytes.as_slice());
            let b = now.get(path).map(|f| f.bytes.as_slice());
            if a != b {
                change += count_changes(a.unwrap_or_default(), b.unwrap_or_default());
            }
        }
        // A path that was in the tree and is gone now — deleted, or no longer
        // recorded as written — counts as its full removal.
        for path in self.prev.keys() {
            if !now.contains_key(path) {
                change += count_changes(&self.prev[path].bytes, b"");
            }
        }
        self.changed += change;
        let verb = if self.changed == 1 {
            "line has"
        } else {
            "lines have"
        };
        self.note = format!(
            "This is loop round {}. {} {verb} changed across the tree so far. \
             If nothing is left worth changing, change nothing — an unchanged round \
             is the signal to stop.",
            self.round + 1,
            self.changed
        );
        let quiet = self.seen.last() == Some(&fingerprint);
        let oscillating = self.seen.contains(&fingerprint);
        self.seen.push(fingerprint);
        self.prev = now;
        if quiet {
            return Round::Quiet;
        }
        if oscillating {
            return Round::Oscillating;
        }
        self.thin = if change < THIN_CHANGES {
            self.thin + 1
        } else {
            0
        };
        if self.thin >= THIN_ROUNDS {
            return Round::Thin;
        }
        if cap.is_some_and(|cap| self.round >= cap) {
            return Round::Capped(self.round);
        }
        let goal = self.goal.clone();
        let next = self.round + 1;
        Round::Again { goal, next }
    }
}
