//! How one transcript is laid out on disk.
//!
//! A session is written after every turn, and a compiler-written snapshot made
//! every save cost what the whole session weighs — a long one took a hundred
//! milliseconds to save, and the file was replaced whole each time, so a run's
//! total write cost grew with the square of what it had said.
//!
//! So the file is an ordered log instead: a ledger line naming the session, then
//! one record per save holding only the entries added since the record before,
//! each followed by a small index line saying how far the log has got. Appending
//! costs what the new entries weigh — the index line is what a save reads to
//! know where it stands, so it never reads the log it is appending to.
//!
//! Reading replays the records in order, which is the work a snapshot parse was.
//!
//! Compacting — writing every entry again as one record — happens when the log
//! holds many records, or when the transcript shrank (a rewind leaves records
//! for entries that no longer exist). A file written before this layout is one
//! JSON object; it is read as it was, and rewritten in this shape the next time
//! the session is saved.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use transcript::Session;

use super::{Header, sync_dir};

/// What the first line says the file is. Anything else is not a log of saves.
const LOG: u8 = 1;

/// Records before the file is rewritten whole: enough that the rewrite is
/// rare, few enough that reading one stays a single file's worth of work.
const COMPACT_AFTER: u64 = 64;

/// The first line: whose transcript this is. What does not change as a
/// session grows, so a listing reads one line to name it.
#[derive(Serialize)]
struct Ledger<'a> {
    log: u8,
    id: &'a str,
    workspace: String,
    model: &'a str,
    created: u64,
}

#[derive(serde::Deserialize)]
pub(crate) struct LedgerRead {
    log: u8,
    pub(crate) id: String,
    pub(crate) workspace: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) created: u64,
}

/// One save: the entries added since the record before, and what the session
/// was as a whole at that point.
#[derive(Serialize)]
struct Record<'a, T> {
    /// Which record this is, from 0.
    n: u64,
    from: usize,
    next: u64,
    interrupted: Option<&'a transcript::StopCause>,
    name: Option<&'a str>,
    /// The model that answered, which a resume can change: the file names the
    /// one that ran, not the one this session began on.
    model: &'a str,
    spent: llm::Totals,
    entries: T,
}

#[derive(serde::Deserialize)]
#[serde(bound(deserialize = "T: DeserializeOwned"))]
struct RecordRead<T> {
    #[serde(default)]
    n: u64,
    #[serde(default)]
    from: usize,
    #[serde(default)]
    next: u64,
    #[serde(default)]
    interrupted: Option<transcript::StopCause>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    spent: llm::Totals,
    #[serde(default)]
    entries: Vec<T>,
}

/// What follows each record: how far the log has got, in its own short line so
/// a save can read the end of a log without reading a record that spans
/// megabytes.
#[derive(Serialize, serde::Deserialize)]
struct IndexLine {
    index: Index,
}

#[derive(Serialize, serde::Deserialize)]
struct Index {
    n: u64,
    to: usize,
}

/// Where the log had got to: the entries it holds, and how many records.
struct Tail {
    entries: usize,
    records: u64,
}

/// What a replayed log holds, apart from the entries themselves: the last
/// record's state, since the session is one thing and its records are how it
/// got there.
pub(crate) struct Replayed<T> {
    pub entries: Vec<T>,
    pub next: u64,
    pub interrupted: Option<transcript::StopCause>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub spent: llm::Totals,
}

/// Save `session` under `path`, appending when that is safe.
pub(crate) fn write(
    path: &Path,
    workspace: &Path,
    header: Header<'_>,
    session: &Session,
) -> Result<PathBuf> {
    let dir = path.parent().expect("a session directory");
    std::fs::create_dir_all(dir)?;
    let entries = session.entries();
    match tail(path) {
        // Only what is new goes down: the rest is already in the file, in order.
        Some(tail)
            if entries.len() >= tail.entries
                && tail.records < COMPACT_AFTER
                && append(path, &tail, header, session, &entries[tail.entries..]).is_ok() =>
        {
            Ok(path.to_path_buf())
        }
        // A transcript that shrank (a rewind), a log gone unreadable, or one
        // holding many records: written whole, so what the file says is the
        // session itself and not a history of how it changed.
        _ => compact(path, workspace, header, session),
    }
}

// Add one record, and its index, to the end of the log.
fn append(
    path: &Path,
    tail: &Tail,
    header: Header<'_>,
    session: &Session,
    added: &[transcript::Entry],
) -> Result<()> {
    let record = Record {
        n: tail.records,
        from: tail.entries,
        next: session.next_id(),
        interrupted: session.stopped(),
        name: header.name,
        model: header.model,
        spent: header.spent,
        entries: added,
    };
    let index = IndexLine {
        index: Index {
            n: tail.records,
            to: tail.entries + added.len(),
        },
    };
    let mut line = serde_json::to_vec(&record)?;
    line.push(b'\n');
    line.extend_from_slice(&serde_json::to_vec(&index)?);
    line.push(b'\n');
    // One append per save: a crash mid-write costs the record, never the
    // records before it.
    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

// The whole transcript as one record, under a fresh ledger. The sync then
// rename means a crash mid-write cannot leave a truncated one.
fn compact(
    path: &Path,
    workspace: &Path,
    header: Header<'_>,
    session: &Session,
) -> Result<PathBuf> {
    let dir = path.parent().expect("a session directory");
    let tmp = path.with_extension("json.tmp");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The transcript holds prompts and file contents. Chmodded before
        // the write, not after: no moment when the data sits world-readable.
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    let mut out = std::io::BufWriter::new(file);
    let ledger = Ledger {
        log: LOG,
        id: header.id,
        workspace: workspace.display().to_string(),
        model: header.model,
        created: header.created,
    };
    serde_json::to_writer(&mut out, &ledger)?;
    out.write_all(b"\n")?;
    let record = Record {
        n: 0,
        from: 0,
        next: session.next_id(),
        interrupted: session.stopped(),
        name: header.name,
        model: header.model,
        spent: header.spent,
        entries: session.entries(),
    };
    serde_json::to_writer(&mut out, &record)?;
    out.write_all(b"\n")?;
    let index = IndexLine {
        index: Index {
            n: 0,
            to: session.entries().len(),
        },
    };
    serde_json::to_writer(&mut out, &index)?;
    out.write_all(b"\n")?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_dir(dir);
    Ok(path.to_path_buf())
}

// Where the log had got to, read from its last line — its index — rather than
// from its records: a save's own cost stays off the transcript's size.
fn tail(path: &Path) -> Option<Tail> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    // A file from before the log is one object, with nothing to append to.
    let mut first = String::new();
    (&mut file).take(4096).read_to_string(&mut first).ok()?;
    if !is_log(&first) {
        return None;
    }
    // An index line is short by construction; the window only has to hold one.
    const WINDOW: u64 = 4096;
    let start = len.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut body = Vec::new();
    file.read_to_end(&mut body).ok()?;
    // The last complete line: a write a crash cut in half is not one to
    // count on, and the index line before it is.
    let end = body.iter().rposition(|b| *b == b'\n')?;
    let before = &body[..end];
    let line = before.rsplit(|b| *b == b'\n').next()?;
    let index: IndexLine = serde_json::from_slice(line).ok()?;
    Some(Tail {
        entries: index.index.to,
        records: index.index.n + 1,
    })
}

// Whether this file is a log of saves rather than the single object a build
// before it wrote.
pub(crate) fn is_log(text: &str) -> bool {
    text.lines()
        .next()
        .and_then(|first| serde_json::from_str::<LedgerRead>(first).ok())
        .is_some_and(|ledger| ledger.log == LOG)
}

/// Replay a log's records into the entries they add up to.
///
/// A record whose `from` is not what came before is one of two things: the tail
/// of a write a crash cut short, or a record for a session that has since been
/// rewritten. Either way it is not this transcript, and the log stops there.
pub(crate) fn replay<T: DeserializeOwned>(text: &str) -> (Replayed<T>, LedgerRead) {
    let mut lines = text.lines();
    let ledger: LedgerRead = lines
        .next()
        .and_then(|first| serde_json::from_str(first).ok())
        .unwrap_or(LedgerRead {
            log: LOG,
            id: String::new(),
            workspace: String::new(),
            model: String::new(),
            created: 0,
        });
    let mut records = 0u64;
    let mut out: Replayed<T> = Replayed {
        entries: Vec::new(),
        next: 0,
        interrupted: None,
        name: None,
        model: None,
        spent: llm::Totals::default(),
    };
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        // Each record's own index line: nothing to replay, and nothing that
        // would parse as a record.
        if serde_json::from_str::<IndexLine>(line).is_ok() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<RecordRead<T>>(line) else {
            break;
        };
        // Both have to be where the record says they are: a record written
        // for another order of saves is not part of this transcript.
        if record.n != records || record.from != out.entries.len() {
            break;
        }
        records += 1;
        out.entries.extend(record.entries);
        out.next = record.next;
        out.interrupted = record.interrupted;
        out.name = record.name;
        out.model = record.model;
        out.spent = record.spent;
    }
    (out, ledger)
}

/// A transcript as the resume list reads it, and the fields a listing needs
/// without building one: what the ledger says plus what the records add.
pub(crate) fn shallow(path: &Path) -> Option<(LedgerRead, Replayed<super::PeekEntry>, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let bytes = text.len() as u64;
    // A file from before the log: one object holding the whole transcript.
    if !is_log(&text) {
        let old: super::Peek = serde_json::from_str(&text).ok()?;
        let ledger = LedgerRead {
            log: 0,
            id: old.id,
            workspace: old.workspace,
            model: old.model.clone(),
            created: old.created,
        };
        return Some((
            ledger,
            Replayed {
                entries: old.entries,
                next: 0,
                interrupted: None,
                name: old.name,
                model: Some(old.model),
                spent: llm::Totals::default(),
            },
            bytes,
        ));
    }
    let (replayed, ledger) = replay(&text);
    Some((ledger, replayed, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TRANSCRIPT;

    // A save that only appends leaves the earlier records where they were: the
    // file grows by what was added, not by what it holds.
    #[test]
    fn a_second_save_appends_rather_than_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join(TRANSCRIPT);
        let mut session = Session::new();
        session.push_note("one");
        write(&path, dir.path(), header("s"), &session).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();

        session.push_note("two");
        write(&path, dir.path(), header("s"), &session).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(
            second.starts_with(&first),
            "the first save's bytes were rewritten"
        );
        assert_eq!(
            second.lines().count(),
            first.lines().count() + 2,
            "a record and its index for one save"
        );
    }

    // A rewind leaves entries the session no longer holds: the file is written
    // whole rather than replayed into a transcript that never was.
    #[test]
    fn a_transcript_that_shrank_is_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join(TRANSCRIPT);
        let mut session = Session::new();
        session.push_note("one");
        write(&path, dir.path(), header("s"), &session).unwrap();
        session.push_note("two");
        write(&path, dir.path(), header("s"), &session).unwrap();

        session.push_note("three");
        let kept = session.entries()[0].id();
        session.rollback_to(kept);
        write(&path, dir.path(), header("s"), &session).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "a ledger, a record, an index");
        let (replayed, _) = replay::<transcript::Entry>(&text);
        assert_eq!(replayed.entries.len(), 1);
    }

    // A write a crash cut in half costs that write and no more.
    #[test]
    fn a_half_written_record_is_not_read_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join(TRANSCRIPT);
        let mut session = Session::new();
        session.push_note("one");
        write(&path, dir.path(), header("s"), &session).unwrap();
        session.push_note("two");
        write(&path, dir.path(), header("s"), &session).unwrap();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"n\":2,\"from\":2,\"entries\":[{\"id\"");

        let (replayed, _) = replay::<transcript::Entry>(&text);
        assert_eq!(replayed.entries.len(), 2, "the records before it stand");
    }

    // The index line is what a save reads to find where to append: a log holding
    // a record that spans megabytes still appends in constant time.
    #[test]
    fn a_long_record_does_not_stop_the_next_save_appending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join(TRANSCRIPT);
        let mut session = Session::new();
        // One entry filling the first record well past the read window.
        session.push_note("x".repeat(200_000));
        write(&path, dir.path(), header("s"), &session).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();

        session.push_note("after");
        write(&path, dir.path(), header("s"), &session).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(
            second.starts_with(&first),
            "a long record forced the whole file to be rewritten"
        );
        let (replayed, _) = replay::<transcript::Entry>(&second);
        assert_eq!(replayed.entries.len(), 2);
    }

    // A file written before the log is read as it was, and the next save turns
    // it into one: nothing of a transcript is lost by the change of layout.
    #[test]
    fn a_file_from_before_the_log_is_read_and_then_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join(TRANSCRIPT);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut session = Session::new();
        session.push_note("before the log");
        let old = serde_json::json!({
            "id": "s",
            "workspace": dir.path().display().to_string(),
            "model": "m",
            "created": 1,
            "name": "n",
            "entries": serde_json::to_value(session.entries()).unwrap(),
            "next": 1,
        });
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let stored = crate::session::Store::read(&path).unwrap();
        assert_eq!(stored.session.entries().len(), 1, "read as it was");

        write(&path, dir.path(), header("s"), &stored.session).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(is_log(&text), "the next save uses the log");
        let (replayed, _) = replay::<transcript::Entry>(&text);
        assert_eq!(replayed.entries.len(), 1);
    }

    // A run saves after every turn, so a long one crosses the point where the
    // log is rewritten whole: what it holds then is still the session.
    #[test]
    fn a_log_rewritten_after_many_saves_still_replays_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s").join(TRANSCRIPT);
        let mut session = Session::new();
        for i in 0..(COMPACT_AFTER + 5) {
            session.push_note(format!("turn {i}"));
            write(&path, dir.path(), header("s"), &session).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines().count() < COMPACT_AFTER as usize * 2,
            "the log grew past one rewrite"
        );
        let stored = crate::session::Store::read(&path).unwrap();
        assert_eq!(stored.session.entries(), session.entries());
        assert_eq!(stored.session.next_id(), session.next_id());
    }

    fn header<'a>(id: &'a str) -> Header<'a> {
        Header {
            id,
            model: "m",
            name: Some("n"),
            created: 1,
            spent: llm::Totals::default(),
        }
    }
}
