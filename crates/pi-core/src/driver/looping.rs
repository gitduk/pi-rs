//! `/loop`: the same line, submitted round after round until the tree stops
//! changing — judged by the tree, never by the model.
//!
//! A loop drives a lane from outside, as a channel does; the lane itself
//! knows nothing of loops.

use std::collections::BTreeMap;

use tool::Ctx;

use super::Ended;

/// Every lane's loop, by lane token.
#[derive(Default)]
pub struct Loops {
    by_lane: BTreeMap<u64, Entry>,
}

struct Entry {
    looping: Looping,
    phase: Phase,
}

// Where a loop is between one round and the next.
#[derive(PartialEq, Eq)]
enum Phase {
    // The next round is owed and not yet handed out.
    Due,
    // Handed out; waiting to hear whether it started a turn.
    Handed,
    // A turn this loop began is running: its end is the loop's to measure.
    // Anything else the lane runs meanwhile is not a round.
    Running,
}

/// A round to submit: the goal as typed, and the note that goes before it.
pub struct Due {
    pub goal: String,
    pub note: String,
}

impl Loops {
    /// Put `lane` under a loop over `goal`, marked from where the tree stands
    /// now. Its first round is due at once.
    pub fn start(&mut self, lane: u64, goal: String, ctx: &Ctx) -> Result<(), String> {
        if let Some(entry) = self.by_lane.get(&lane) {
            return Err(format!(
                "`{}` is already looping here — /loop to stop it first",
                entry.looping.goal
            ));
        }
        let looping = Looping::start(ctx, goal);
        let phase = Phase::Due;
        self.by_lane.insert(lane, Entry { looping, phase });
        Ok(())
    }

    /// Stop the loop on `lane`, saying how far it got; `None` when none was.
    pub fn stop(&mut self, lane: u64) -> Option<String> {
        let l = self.take(lane)?;
        Some(format!(
            "loop stopped after {} round(s) of `{}`",
            l.round, l.goal
        ))
    }

    /// The round `lane` owes, handed out once. Asked only when the lane is
    /// free and nothing typed is waiting: a typed line goes first.
    pub fn due(&mut self, lane: u64) -> Option<Due> {
        let entry = self
            .by_lane
            .get_mut(&lane)
            .filter(|e| e.phase == Phase::Due)?;
        entry.phase = Phase::Handed;
        Some(Due {
            goal: entry.looping.goal.clone(),
            note: entry.looping.note.clone(),
        })
    }

    /// The round just handed out started a turn: its end is this loop's.
    pub fn ask(&mut self, lane: u64) {
        if let Some(entry) = self.by_lane.get_mut(&lane) {
            entry.phase = Phase::Running;
        }
    }

    /// The round just handed out started no turn — a skill that went away at a
    /// reload — so there is nothing to measure and the loop ends.
    pub fn unstarted(&mut self, lane: u64) -> Option<String> {
        let l = self.take(lane)?;
        Some(format!(
            "loop ended — `{}` starts no turn to measure",
            l.goal
        ))
    }

    /// A turn on `lane` ended. `None` when it was not one this loop began;
    /// otherwise what the loop does next. `Again` leaves the next round due.
    pub fn turn_ended(
        &mut self,
        lane: u64,
        ended: &Ended,
        ctx: &Ctx,
        cap: Option<usize>,
    ) -> Option<Round> {
        let entry = self
            .by_lane
            .get_mut(&lane)
            .filter(|e| e.phase == Phase::Running)?;
        let cut = match ended {
            Ended::Done => None,
            Ended::Stopped => Some(Cut::Stopped),
            Ended::Failed(_) => Some(Cut::Failed),
            Ended::Unsent => Some(Cut::Unsent),
        };
        let round = entry.looping.step(ctx, cut, cap);
        if matches!(round, Round::Again) {
            entry.phase = Phase::Due;
        } else {
            self.by_lane.remove(&lane);
        }
        Some(round)
    }

    /// Whether `lane` is under a loop, between rounds or in one.
    pub fn active(&self, lane: u64) -> bool {
        self.by_lane.contains_key(&lane)
    }

    /// End the loops of lanes that are gone — a removed checkout takes its
    /// loop with it — and say which ended.
    pub fn retain(&mut self, live: impl Fn(u64) -> bool) -> Vec<String> {
        let gone: Vec<u64> = self.by_lane.keys().copied().filter(|l| !live(*l)).collect();
        gone.into_iter()
            .filter_map(|lane| self.take(lane))
            .map(|l| format!("loop ended — `{}` lost its checkout", l.goal))
            .collect()
    }

    fn take(&mut self, lane: u64) -> Option<Looping> {
        self.by_lane.remove(&lane).map(|e| e.looping)
    }
}

// One lane's loop: the goal, and what the tree has looked like so far.
struct Looping {
    // Re-submitted verbatim each round, read as whatever it was the first
    // time — a skill stays a skill, prose stays prose.
    goal: String,
    round: usize,
    // Read into every round's prompt: how far the loop has got, and the
    // standing licence to change nothing.
    note: String,
    // Fingerprints the tree has worn, oldest first (starting state
    // included); an earlier hit means a round undid its way back.
    seen: Vec<String>,
    // The written tree as the last round left it, one entry per path. The
    // next round is diffed against this, path by path.
    prev: TreeState,
    // Consecutive rounds that changed fewer than `THIN_CHANGES` lines.
    thin: usize,
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

// A content fingerprint over every written path, plus cached bytes to diff
// against; a path whose stat is unchanged since `prev` skips the re-read.
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
        // Past the LCS budget: positionally-equal lines are kept, the rest
        // counted as remove/add — an upper bound, never zero for a rewrite.
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

/// What cut a round short of its own end.
///
/// The loop goes with the run on any of the three: a stopped, failed or
/// taken-back round never left the tree a verdict to read.
pub enum Cut {
    // Esc, or a stop asked for another way.
    Stopped,
    // The round died rather than finishing. Its error has a line of its own.
    Failed,
    // The prompt was taken back, so the round is the user's again.
    Unsent,
}

/// What a loop does now that one of its rounds has ended.
pub enum Round {
    // Run the goal again: the next round is due.
    Again,
    // The round changed nothing. Where a loop that is fixing things finishes:
    // a pass that found nothing to do has nothing to do next time either.
    Quiet,
    // The tree is back at a fingerprint it wore earlier — a round undid its
    // own work. Such a loop would seesaw forever, so it stops.
    Oscillating,
    // Several rounds in a row moved fewer than `THIN_CHANGES` lines. The
    // fingerprint cannot catch a round that keeps nibbling, so this does.
    Thin,
    // `loop_max_rounds` reached, with rounds still changing the tree.
    Capped(usize),
    // Esc, an error, or a prompt taken back. The loop goes with the run.
    Cut(Cut),
}

impl Round {
    /// The line that says why the loop ended; `None` while it goes on.
    pub fn ending(&self) -> Option<String> {
        let said = match self {
            Round::Again => return None,
            Round::Cut(Cut::Stopped) => "loop stopped — the round was cut short".into(),
            Round::Cut(Cut::Failed) => "loop stopped — the round failed".into(),
            Round::Cut(Cut::Unsent) => "loop stopped — the prompt came back".into(),
            Round::Quiet => "loop done — that round changed nothing".into(),
            Round::Oscillating => "loop stopped — a round undid the work before it".into(),
            Round::Thin => "loop stopped — rounds are only nibbling now".into(),
            Round::Capped(n) => {
                format!("loop stopped at loop_max_rounds ({n}) — rounds were still changing files")
            }
        };
        Some(said)
    }
}

impl Looping {
    // A loop over `goal`, marked from where the tree stands now.
    fn start(ctx: &Ctx, goal: String) -> Self {
        let (seen, prev) = tree_mark(ctx, &TreeState::new());
        Self {
            goal,
            round: 0,
            note: String::new(),
            seen: vec![seen],
            prev,
            thin: 0,
        }
    }

    // What the loop does now that a round has ended. `cut` is what stopped the
    // round short, and `None` is a round that reached its own end.
    fn step(&mut self, ctx: &Ctx, cut: Option<Cut>, cap: Option<usize>) -> Round {
        self.round += 1;
        // A cut round ends the loop without measuring: esc may have stopped
        // the run mid-write, so the tree's state isn't a judgement to read.
        if let Some(cut) = cut {
            return Round::Cut(cut);
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
        // How much has changed is the model's to measure — the tree is right
        // there, and a count kept here is one more thing to disagree with it.
        self.note = format!(
            "This is loop round {}. If nothing is left worth changing, change \
             nothing — an unchanged round is the signal to stop.",
            self.round + 1
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
        Round::Again
    }
}
