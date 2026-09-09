//! The shelf compaction cannot reach.
//!
//! Compaction drops a *round* — a question and everything that answered it —
//! and most of what it takes deserves to go. What does not is written here
//! first: a constraint stated once, a route proven closed, a decision that
//! would otherwise be re-litigated.
//!
//! One file per repository, in the bucket its main checkout owns: a note is
//! about the work, and the work is the repository.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session::now;

/// How many notes the shelf holds. Yours are never crowded out, so what this
/// really caps is how many the model may keep.
pub const CAP: usize = 30;

/// What a note loses per day, against a weight of 1 to 3. A weight-3 note is
/// worth keeping for a month, a weight-1 note for ten days — recency and
/// importance in one number, which is what deciding between them needs.
const PER_DAY: f64 = 0.1;

const DAY: u64 = 60 * 60 * 24;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    /// What names this note, so a panel can act on the note it is showing
    /// rather than the row it sat on. Compaction rewrites this file mid-run,
    /// and a row number means a different note afterwards.
    #[serde(default)]
    pub id: u64,
    pub text: String,
    /// Unix seconds. Rendered with the note, because a shelf of bare
    /// statements is read in the present tense however old it is.
    pub at: u64,
    /// What the model judged this worth, 1 to 3. A note you typed has none: it
    /// does not compete for room and does not age out. You put it there; only
    /// you take it away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u8>,
}

impl Note {
    /// A note you typed.
    pub fn yours(text: impl Into<String>) -> Self {
        Self {
            id: 0,
            text: text.into(),
            at: now(),
            weight: None,
        }
    }

    /// A note the model wrote, at the weight it gave.
    pub fn theirs(text: impl Into<String>, weight: u8) -> Self {
        Self {
            id: 0,
            text: text.into(),
            at: now(),
            weight: Some(weight.clamp(1, 3)),
        }
    }

    pub fn is_yours(&self) -> bool {
        self.weight.is_none()
    }

    /// What it is still worth, `now` being the moment asked about. Yours have
    /// no score: they are not in the running.
    fn score(&self, now: u64) -> Option<f64> {
        let days = now.saturating_sub(self.at) as f64 / DAY as f64;
        self.weight.map(|w| w as f64 - days * PER_DAY)
    }
}

/// One note as a panel shows it: what to call it, when it was written, and
/// what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u64,
    pub day: String,
    pub text: String,
}

/// One repository's shelf.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub notes: Vec<Note>,
}

impl Memory {
    /// Read the shelf, or an empty one. A file that will not parse is treated
    /// as empty rather than as a failure: memory is an aid, and a run that
    /// refused to start over a corrupted aid would be the worse trade.
    pub fn load(path: &Path) -> Self {
        let mut shelf: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|body| serde_json::from_str(&body).ok())
            .unwrap_or_default();
        shelf.number();
        shelf
    }

    /// Give an id to whatever lacks one. Zero is not an id — it is what a
    /// note written before ids reads as, and what `Note::yours` mints before
    /// it knows what else is on the shelf.
    fn number(&mut self) {
        let taken = self.notes.iter().map(|n| n.id).max().unwrap_or(0);
        for (nth, note) in self.notes.iter_mut().filter(|n| n.id == 0).enumerate() {
            note.id = taken + nth as u64 + 1;
        }
    }

    /// Change the shelf on disk as one read-change-write, holding the shelf
    /// lock across the whole of it. Every writer goes through here, so runs
    /// that share a repository's shelf — lanes of one session, or processes
    /// on unix — cannot save over each other's read.
    pub fn update<T>(
        path: &Path,
        change: impl FnOnce(&mut Memory) -> T,
    ) -> anyhow::Result<T> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let _lock = lock_shelf(path)?;
        let mut shelf = Memory::load(path);
        let out = change(&mut shelf);
        shelf.save(path)?;
        Ok(out)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Notes quote the user and the tree; a transcript's permissions.
        tools::state::write_private(path, &serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Add notes and make room for them.
    pub fn add(&mut self, notes: impl IntoIterator<Item = Note>) {
        self.notes.extend(notes);
        self.number();
        self.evict(now());
    }

    /// Drop the model's lowest-scoring notes until the shelf fits.
    ///
    /// Only the model's: yours are the reason the cap is on its notes rather
    /// than on the shelf, so a shelf you filled yourself is full and holds no
    /// model notes at all, rather than dropping what you put there.
    fn evict(&mut self, now: u64) {
        let yours = self.notes.iter().filter(|n| n.is_yours()).count();
        let room = CAP.saturating_sub(yours);
        let theirs = self.notes.len() - yours;
        if theirs <= room {
            return;
        }
        // Worst first, and the older of an equal pair before the newer.
        let mut ranked: Vec<usize> = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, n)| !n.is_yours())
            .map(|(i, _)| i)
            .collect();
        ranked.sort_by(|a, b| {
            let (x, y) = (&self.notes[*a], &self.notes[*b]);
            let (sx, sy) = (x.score(now).unwrap_or(0.0), y.score(now).unwrap_or(0.0));
            sx.total_cmp(&sy).then(x.at.cmp(&y.at))
        });
        let mut doomed = vec![false; self.notes.len()];
        for i in ranked.into_iter().take(theirs - room) {
            doomed[i] = true;
        }
        let mut doomed = doomed.into_iter();
        self.notes.retain(|_| !doomed.next().unwrap_or(false));
    }

    /// One row per note, for a panel to show: the day it was written and the
    /// note itself. The day is separate here and inline in `render` — a person
    /// reads it in a column, the model reads it in the sentence.
    pub fn rows(&self) -> Vec<Row> {
        self.notes
            .iter()
            .map(|n| Row {
                id: n.id,
                day: day(n.at),
                text: n.text.clone(),
            })
            .collect()
    }

    /// Rewrite one note, leaving its weight and its day alone: it is the same
    /// note said better, not a new one, and re-dating it would let a shelf be
    /// kept alive forever by tidying it.
    pub fn rewrite(&mut self, id: u64, text: &str) {
        if let Some(note) = self.notes.iter_mut().find(|n| n.id == id) {
            note.text = text.to_string();
        }
    }

    /// Take one away. A note that is not there is not an error: it went while
    /// the panel was looking at it, which is what was wanted anyway.
    pub fn forget(&mut self, id: u64) {
        self.notes.retain(|n| n.id != id);
    }

    /// The shelf as the model reads it, or nothing when it is empty.
    ///
    /// Each note carries the day it was written. A standing fact and a
    /// three-week-old guess look identical as bare statements, and the model
    /// has no way to tell them apart but this.
    pub fn render(&self) -> Option<String> {
        if self.notes.is_empty() {
            return None;
        }
        let mut out = String::from("<memory>\n");
        for note in &self.notes {
            out.push_str(&format!("{} {}\n", day(note.at), note.text.trim()));
        }
        out.push_str("</memory>");
        Some(out)
    }
}

/// The shelf's write lock, held for one whole read-change-write. A sibling
/// file, never the shelf: `save` replaces the shelf by temp-and-rename, so a
/// lock on the shelf's inode would guard a file nothing else opens any more.
#[cfg(unix)]
struct ShelfLock {
    _file: std::fs::File,
}

#[cfg(unix)]
fn lock_shelf(path: &Path) -> anyhow::Result<ShelfLock> {
    use std::os::unix::io::AsRawFd;
    let lock = PathBuf::from(format!("{}.lock", path.display()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)?;
    // Blocks until the current holder finishes; the lock goes with the fd.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(ShelfLock { _file: file })
}

#[cfg(not(unix))]
struct ShelfLock(std::sync::MutexGuard<'static, ()>);

#[cfg(not(unix))]
fn lock_shelf(_path: &Path) -> anyhow::Result<ShelfLock> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let lock = LOCK.get_or_init(|| std::sync::Mutex::new(()));
    Ok(ShelfLock(lock.lock().unwrap_or_else(|e| e.into_inner())))
}

/// The date part of an instant, which is all a note needs: what matters is
/// whether this was said today or last month.
fn day(at: u64) -> String {
    crate::journal::rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(at))[..10]
        .to_string()
}

/// This repository's shelf, as the agent reaches it.
pub fn shelf(path: PathBuf) -> std::sync::Arc<dyn agent::Shelf> {
    std::sync::Arc::new(File::at(path))
}

/// The shelf as the agent reaches it: a path, read and written on demand.
///
/// Read fresh each time rather than held: `/mem` writes to the same file
/// between turns, and a copy taken at startup would quietly overwrite it.
pub struct File {
    path: PathBuf,
}

impl File {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }
}

impl agent::Shelf for File {
    fn read(&self) -> Option<String> {
        Memory::load(&self.path).render()
    }

    // Inline rather than on a blocking thread, unlike `subagent::Filed`: that
    // one files megabyte transcripts from several lanes at once, this one
    // thirty short lines at a compaction.
    fn keep(&self, notes: Vec<agent::Kept>) {
        // The update takes the shelf lock, so a compaction running in another
        // lane cannot lose notes to this lane's read-change-write.
        if let Err(e) = Memory::update(&self.path, |shelf| {
            shelf.add(notes.into_iter().map(|n| Note::theirs(n.text, n.weight)));
        }) {
            tracing::warn!(target: "pi::memory", error = %e, "could not write the shelf");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CAP, DAY, Memory, Note};

    fn model(text: &str, weight: u8, days_ago: u64) -> Note {
        Note {
            id: 0,
            text: text.into(),
            at: 1_000 * DAY - days_ago * DAY,
            weight: Some(weight),
        }
    }

    fn yours(text: &str) -> Note {
        Note {
            id: 0,
            text: text.into(),
            at: 1_000 * DAY,
            weight: None,
        }
    }

    fn shelf(notes: Vec<Note>) -> Memory {
        Memory { notes }
    }

    fn texts(m: &Memory) -> Vec<&str> {
        m.notes.iter().map(|n| n.text.as_str()).collect()
    }

    /// Importance and age in one number, which is what choosing between them
    /// needs: a weight-3 note outlives a weight-1 note by twenty days.
    #[test]
    fn the_weakest_and_the_stalest_go_first() {
        let mut m = shelf(vec![
            model("fresh but slight", 1, 0),
            model("old and slight", 1, 15),
            model("old but weighty", 3, 15),
        ]);
        m.notes.push(model("one too many", 1, 0));
        m.evict(1_000 * DAY);
        // Nothing went: four notes is not thirty.
        assert_eq!(m.notes.len(), 4);

        let mut m = shelf(
            (0..CAP)
                .map(|i| model(&format!("filler {i}"), 2, 0))
                .collect(),
        );
        m.notes.push(model("old and slight", 1, 25));
        m.evict(1_000 * DAY);
        assert_eq!(m.notes.len(), CAP);
        assert!(
            !texts(&m).contains(&"old and slight"),
            "the weakest survived"
        );
    }

    /// You put it there; only you take it away. A shelf you filled yourself is
    /// full, and the model keeps none — rather than yours being pushed off.
    #[test]
    fn your_own_notes_are_never_crowded_out() {
        let mut m = shelf((0..CAP).map(|i| yours(&format!("mine {i}"))).collect());
        m.add([model("theirs", 3, 0)]);
        assert_eq!(m.notes.len(), CAP);
        assert!(!texts(&m).contains(&"theirs"));
        assert!(m.notes.iter().all(|n| n.is_yours()));
    }

    /// Half the shelf yours leaves half of it for the model, not all of it.
    #[test]
    fn yours_take_room_from_theirs_rather_than_from_the_cap() {
        let mut m = shelf((0..10).map(|i| yours(&format!("mine {i}"))).collect());
        m.add((0..40).map(|i| model(&format!("theirs {i}"), 2, i)));
        assert_eq!(m.notes.len(), CAP);
        assert_eq!(m.notes.iter().filter(|n| n.is_yours()).count(), 10);
    }

    /// The panel draws rows, then a compaction rewrites the file underneath
    /// it. A row number would name a different note by the time the user
    /// pressed `x`; a name names the note.
    #[test]
    fn a_note_is_named_not_numbered() {
        let mut m = shelf(vec![yours("first"), yours("second"), yours("third")]);
        m.number();
        let second = m.notes[1].id;
        assert!(
            m.notes.iter().all(|n| n.id != 0),
            "every note answers to something"
        );

        // What a compaction does behind the panel's back.
        m.notes.remove(0);
        m.add([model("what the model kept", 3, 0)]);

        m.rewrite(second, "second, said better");
        assert_eq!(
            m.notes.iter().find(|n| n.id == second).unwrap().text,
            "second, said better"
        );

        m.forget(second);
        assert!(m.notes.iter().all(|n| n.id != second));
        assert_eq!(
            texts(&m),
            vec!["third", "what the model kept"],
            "nothing else moved"
        );
    }

    /// Ids are minted against what is already there, so a shelf that grew and
    /// shrank never hands two notes the same name.
    #[test]
    fn a_name_is_never_handed_out_twice() {
        let mut m = shelf(vec![yours("a"), yours("b")]);
        m.number();
        let a = m.notes[0].id;
        m.forget(a);
        m.add([yours("c"), yours("d")]);
        let ids: Vec<u64> = m.notes.iter().map(|n| n.id).collect();
        let unique: std::collections::HashSet<u64> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "{ids:?}");
        assert!(!ids.contains(&a), "a name that went stays gone: {ids:?}");
    }

    /// A bare statement is read in the present tense however old it is, so
    /// every line says when it was written.
    #[test]
    fn every_note_is_rendered_with_its_day() {
        assert!(Memory::default().render().is_none(), "nothing to say");

        let m = shelf(vec![yours("  prefers xh over curl  ")]);
        let got = m.render().unwrap();
        assert!(got.starts_with("<memory>\n"), "{got}");
        assert!(got.ends_with("</memory>"), "{got}");
        assert!(got.contains(" prefers xh over curl\n"), "trimmed: {got}");
        let line = got.lines().nth(1).unwrap();
        assert_eq!(line.split(' ').next().unwrap().len(), 10, "a day: {line}");
    }

    /// Memory is an aid. A run that refused to start because the aid would not
    /// parse would be the worse trade.
    #[test]
    fn an_unreadable_shelf_reads_as_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");
        assert_eq!(Memory::load(&path), Memory::default(), "not there yet");

        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Memory::load(&path), Memory::default());
    }

    #[test]
    fn a_shelf_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deeper").join("memory.json");
        let mut m = Memory::default();
        m.add([yours("prefers xh"), model("--tools was removed", 2, 0)]);
        m.save(&path).unwrap();
        assert_eq!(Memory::load(&path), m);
    }

    /// Two writers reaching the same shelf at once — the case the lock
    /// exists for — each keep their note.
    #[test]
    fn concurrent_updates_keep_both_notes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");
        let gate = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut threads = Vec::new();
        for text in ["note a", "note b"] {
            let path = path.clone();
            let gate = gate.clone();
            threads.push(std::thread::spawn(move || {
                gate.wait();
                Memory::update(&path, |s| {
                    s.add([yours(text)]);
                })
                .unwrap();
            }));
        }
        for t in threads {
            t.join().unwrap();
        }
        let notes: Vec<String> =
            Memory::load(&path).notes.iter().map(|n| n.text.clone()).collect();
        assert_eq!(notes.len(), 2, "both notes landed: {notes:?}");
    }
}
