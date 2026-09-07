//! The shelf compaction cannot reach.
//!
//! Compaction drops a *round* — a question and everything that answered it —
//! and most of what it takes deserves to go. What does not is written here
//! first: a constraint stated once, a route proven closed, a decision that
//! would otherwise be re-litigated.
//!
//! One file per workspace, beside its transcripts: a note is about the work,
//! and the work is a checkout.

use std::path::Path;

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
        Self { text: text.into(), at: now(), weight: None }
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

/// One workspace's shelf.
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
        std::fs::read_to_string(path)
            .ok()
            .and_then(|body| serde_json::from_str(&body).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Rename, so a crash mid-write cannot leave half a shelf.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Notes quote the user and the tree; a transcript's permissions.
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Add notes and make room for them.
    pub fn add(&mut self, notes: impl IntoIterator<Item = Note>) {
        self.notes.extend(notes);
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

/// The date part of an instant, which is all a note needs: what matters is
/// whether this was said today or last month.
fn day(at: u64) -> String {
    crate::journal::rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(at))[..10]
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{CAP, DAY, Memory, Note};

    fn model(text: &str, weight: u8, days_ago: u64) -> Note {
        Note { text: text.into(), at: 1_000 * DAY - days_ago * DAY, weight: Some(weight) }
    }

    fn yours(text: &str) -> Note {
        Note { text: text.into(), at: 1_000 * DAY, weight: None }
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

        let mut m = shelf((0..CAP).map(|i| model(&format!("filler {i}"), 2, 0)).collect());
        m.notes.push(model("old and slight", 1, 25));
        m.evict(1_000 * DAY);
        assert_eq!(m.notes.len(), CAP);
        assert!(!texts(&m).contains(&"old and slight"), "the weakest survived");
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
}
