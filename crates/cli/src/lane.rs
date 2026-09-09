//! One checkout being worked in, and everything the workspace root decides.

use agent::session::Session;
use agent::{Agent, Event, Steer, Totals};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use tools::Ctx;

use crate::tui::View;

/// Where this lane's turn stands.
///
/// One value rather than a flag beside an outcome: a lane is idle, or running,
/// or holding the end of a run nobody has seen — never two of those, and the
/// compiler is what should say so.
pub enum Turn {
    /// Nothing running, nothing waiting to be looked at.
    Idle,
    /// A run under way.
    Running {
        /// What `esc` cancels, and only for the lane in front.
        cancel: CancellationToken,
        /// Where a line typed mid-run goes, when the job in flight is a turn
        /// and can hear one: the run takes it at its next turn boundary.
        ///
        /// `None` for a `!` or a `/compact`. Neither calls a model, so neither
        /// has a boundary to take a line at, and a line typed at one waits for
        /// the lane the way every line used to. Optional rather than an empty
        /// mailbox on every job, because the two are not the same thing to say.
        steer: Option<Steer>,
        /// Esc caught the prompt on its way out: stop the run, then unsend it.
        /// Set while the run works, acted on when it ends.
        unsend: bool,
    },
    /// A run that ended while this lane was out of sight, kept until the screen
    /// is looking at it and can show how it went.
    ///
    /// Only for work that can be finished off on the view in front: closing a
    /// partial stream, landing animated tool rows, `say` without a prefix. A
    /// job with none of those settles where it ended instead, or its report
    /// waits on a screen that may never come back.
    Ended {
        out: Result<Totals, agent::AgentError>,
        /// Esc asked for the prompt back while this was still running.
        unsend: bool,
    },
}

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
    /// Fingerprints the tree has worn, oldest first, the starting state
    /// included. The last one is the round that just ended; an earlier hit
    /// means a round undid its way back.
    seen: Vec<String>,
    /// The written tree as the last round left it, one entry per path. The
    /// next round is diffed against this, path by path.
    prev: TreeState,
    /// Consecutive rounds that changed fewer than `THIN_CHANGES` lines.
    thin: usize,
    /// Lines changed since the loop began, fed into the next round's prompt.
    changed: usize,
    /// Set when this loop puts a round in the queue, taken when that round
    /// ends. A turn that did not come from here — a line typed between rounds
    /// — also ends, and counting it would move the loop on something it never
    /// ran.
    running: bool,
}

/// A round that moves fewer lines than this is below the noise floor; that
/// many in a row end the loop. Tuned for simplify/review, which converge.
const THIN_CHANGES: usize = 5;
/// Consecutive thin rounds that end the loop.
const THIN_ROUNDS: usize = 2;

/// One written path as the loop last saw it: the bytes, and the stat that
/// says whether they can be trusted next round without reading them again.
struct TreeFile {
    bytes: Vec<u8>,
    len: u64,
    mtime: Option<std::time::SystemTime>,
}

/// The written tree, one entry per path.
type TreeState = std::collections::BTreeMap<std::path::PathBuf, TreeFile>;

/// The tree as the loop measures it: a content fingerprint over every written
/// path, plus the bytes to diff the next round against. A path whose stat is
/// unchanged since `prev` keeps its cached bytes — reading every file again
/// every round is work the diff will throw away.
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
    /// Run this line again, as round `next`.
    Again { goal: String, next: usize },
    /// The round changed nothing. Where a loop that is fixing things finishes:
    /// a pass that found nothing to do has nothing to do next time either.
    Quiet,
    /// The tree is back at a fingerprint it wore earlier — a round undid its
    /// own work. Such a loop would seesaw forever, so it stops.
    Oscillating,
    /// Several rounds in a row moved fewer than `THIN_CHANGES` lines. The
    /// fingerprint cannot catch a round that keeps nibbling, so this does.
    Thin,
    /// `loop_max_turns` reached, with rounds still changing the tree.
    Capped(usize),
    /// Esc, an error, or a prompt taken back. The loop goes with the run.
    Cut,
}

/// What a job leaves behind when it hands the transcript back.
///
/// One value rather than a bool and a leftover list read from two places: a
/// caller that took the `unsend` and forgot the rest would drop what the user
/// said into the gap between the run's last look and its return.
#[derive(Default)]
pub struct Handback {
    /// Esc asked for the prompt back.
    pub unsend: bool,
    /// Said to the run after its last look at the mailbox, so never heard.
    /// It goes back to the surface to be run as an ordinary line.
    pub unheard: Vec<String>,
}

pub struct Lane {
    /// Shared so a run can take it with it: `Agent::run` needs only `&self`,
    /// and a turn outlives the borrow the surface could lend it. `/model` and
    /// `/reload` write through `Arc::make_mut`, so a run in flight keeps the
    /// agent it started on — which is what they meant all along.
    pub agent: std::sync::Arc<Agent>,
    /// The transcript, or None while a run has it — it is lent out for the
    /// length of a turn. An empty session left in its place would read like a
    /// session with nothing in it, which is a different thing to anyone asking.
    pub session: Option<Session>,
    pub id: String,
    /// When this session began. Held rather than read back: it is set once and
    /// never changes, and going to disk for it made every save parse the whole
    /// transcript to recover one integer.
    pub created: u64,
    /// What this lane's finished runs have cost, in and out and in money.
    /// Per lane, not per surface: with lanes working off-screen the surface
    /// that shows the bill has to be able to say which lane ran it up.
    pub totals: Totals,

    /// What the user calls this session, if anything.
    pub name: Option<String>,
    /// The instruction files this run stands on, named as a person would.
    /// Shown under the banner; rebuilt by `/reload` like everything else the
    /// config decides.
    pub context: Vec<String>,
    /// What this checkout tells an agent, verbatim — the tail of the system
    /// prompt that came from the tree. Held so a subagent rebuilt after
    /// `/model` gets the same one the lane was armed with.
    pub standing: std::sync::Arc<str>,
    /// Carried across turns: the file locks and edit shifts outlive any one run.
    pub ctx: Ctx,
    /// Which worktree the session is in, or None in the repository's own
    /// checkout. Held so the status line can say where the work is landing.
    pub worktree: Option<String>,
    /// Where this lane's runs post what they are doing. One channel per lane,
    /// so an event needs no label to say which screen it belongs on.
    pub events: UnboundedSender<Event>,
    /// The other end. Drained by the loop, into the view when this lane is in
    /// front and into `pending` when it is not.
    pub inbox: UnboundedReceiver<Event>,
    /// What arrived while nobody was looking, in order, waiting to be replayed
    /// into the view the moment this lane comes back to the front.
    pub pending: Vec<Event>,
    /// Where this lane's turn stands.
    pub turn: Turn,
    /// What this lane looks like on screen. Held here rather than parked
    /// beside the surface: the screen shows the lane `Repl::current` names,
    /// so there is one view to draw and no second list to keep in step.
    pub view: View,
    /// What a slash answers to here, and the key map in force. Both are what
    /// this root's config and skills resolved to, so they travel with the lane
    /// rather than with the run — a tree switched back to answers to its own.
    pub keys: std::sync::Arc<crate::keys::Keys>,
    pub commands: std::sync::Arc<Vec<crate::repl::Command>>,
    /// The `/loop` this lane is under, if any.
    pub looping: Option<Looping>,
}

impl Lane {
    /// A lane is born with its own channel: nothing else can post to it, and
    /// nothing it posts can land on another screen.
    pub fn channel() -> (UnboundedSender<Event>, UnboundedReceiver<Event>) {
        unbounded_channel()
    }

    /// Take the end of a run this lane has been holding, if it is holding one.
    ///
    /// Only when it is: a lane still working must keep its `Running`, or the
    /// token `esc` reaches and the request to unsend go with it.
    pub fn take_ended(&mut self) -> Option<(Result<Totals, agent::AgentError>, bool)> {
        match self.turn {
            Turn::Ended { .. } => match std::mem::replace(&mut self.turn, Turn::Idle) {
                Turn::Ended { out, unsend } => Some((out, unsend)),
                _ => None,
            },
            _ => None,
        }
    }

    /// A job has given this lane's transcript back. The one place `Running`
    /// ends: a lane left in it queues every later prompt and never drains.
    pub fn finish(&mut self) -> Handback {
        match std::mem::replace(&mut self.turn, Turn::Idle) {
            Turn::Running { unsend, steer, .. } => Handback {
                unsend,
                unheard: steer.map(|s| s.take()).unwrap_or_default(),
            },
            _ => Handback::default(),
        }
    }

    /// Where a line typed mid-run goes, while a run is there to hear one.
    pub fn steer(&self) -> Option<&Steer> {
        match &self.turn {
            Turn::Running { steer, .. } => steer.as_ref(),
            _ => None,
        }
    }

    /// Whether a run has this lane's transcript right now.
    pub fn is_running(&self) -> bool {
        matches!(self.turn, Turn::Running { .. })
    }

    /// The round this lane's loop queued has begun. Nothing else it runs is
    /// one, so nothing else moves it on.
    pub fn loop_running(&mut self) {
        if let Some(looping) = &mut self.looping {
            looping.running = true;
        }
    }

    /// Put this lane under a loop, marked from where the tree stands now.
    pub fn loop_start(&mut self, goal: String) {
        let (seen, prev) = tree_mark(&self.ctx, &TreeState::new());
        self.looping = Some(Looping {
            goal,
            round: 0,
            note: String::new(),
            seen: vec![seen],
            prev,
            thin: 0,
            changed: 0,
            running: false,
        });
    }

    /// What the loop in force does now that a round has ended — `None` when
    /// there was no loop. `finished` is whether the run reached its own end
    /// rather than being cut short.
    ///
    /// The loop is taken out and only put back to go round again, so every
    /// ending drops it without a second place to remember that.
    pub fn loop_step(&mut self, finished: bool, cap: Option<usize>) -> Option<Round> {
        if !self.looping.as_ref()?.running {
            return None;
        }
        let mut looping = self.looping.take()?;
        looping.running = false;
        looping.round += 1;
        // A cut round ends the loop without measuring the tree: esc may have
        // stopped the run mid-write, and where the tree stands now is not a
        // judgement anyone asked for.
        if !finished {
            return Some(Round::Cut);
        }
        let (fingerprint, now) = tree_mark(&self.ctx, &looping.prev);
        let mut change = 0usize;
        // One iterator, not two chained: a path present in both maps would
        // be visited twice and its diff counted twice.
        for path in now.keys() {
            let a = looping.prev.get(path).map(|f| f.bytes.as_slice());
            let b = now.get(path).map(|f| f.bytes.as_slice());
            if a != b {
                change += count_changes(a.unwrap_or_default(), b.unwrap_or_default());
            }
        }
        // A path that was in the tree and is gone now — deleted, or no longer
        // recorded as written — counts as its full removal.
        for path in looping.prev.keys() {
            if !now.contains_key(path) {
                change += count_changes(&looping.prev[path].bytes, b"");
            }
        }
        looping.changed += change;
        let verb = if looping.changed == 1 {
            "line has"
        } else {
            "lines have"
        };
        looping.note = format!(
            "This is loop round {}. {} {verb} changed across the tree so far. \
             If nothing is left worth changing, change nothing — an unchanged round \
             is the signal to stop.",
            looping.round + 1,
            looping.changed
        );
        let quiet = looping.seen.last() == Some(&fingerprint);
        let oscillating = looping.seen.contains(&fingerprint);
        looping.seen.push(fingerprint);
        looping.prev = now;
        if quiet {
            return Some(Round::Quiet);
        }
        if oscillating {
            return Some(Round::Oscillating);
        }
        looping.thin = if change < THIN_CHANGES {
            looping.thin + 1
        } else {
            0
        };
        if looping.thin >= THIN_ROUNDS {
            return Some(Round::Thin);
        }
        if cap.is_some_and(|cap| looping.round >= cap) {
            return Some(Round::Capped(looping.round));
        }
        let goal = looping.goal.clone();
        let next = looping.round + 1;
        self.looping = Some(looping);
        Some(Round::Again { goal, next })
    }
}
