//! One session, one append-only JSONL archive. A complete line publishes a save.

mod index;
mod plan;
mod record;

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use transcript::{Entry, Session};

use super::{Header, Stored};
pub(in crate::session) use index::{Index, RequestAddress};
use record::{Address, Record};

pub const FILE: &str = "session.jsonl";
const FORMAT: u8 = 1;
const REFERENCE_DEPTH: usize = 128;
pub(super) const WRITE_LOCK: &str = ".write.lock";

/// A session address owns its incremental index; it never caches message bodies.
#[derive(Debug, Clone)]
pub struct Location {
    pub path: PathBuf,
    pub session: message::SessionId,
    pub(super) index: Arc<Mutex<Index>>,
}

impl PartialEq for Location {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.session == other.session
    }
}
impl Eq for Location {}
impl std::hash::Hash for Location {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.session.hash(state);
    }
}

impl Location {
    pub fn new(path: PathBuf, session: message::SessionId) -> Self {
        Self {
            path,
            index: Arc::new(Mutex::new(Index::new(session.clone()))),
            session,
        }
    }
    pub fn artifacts(&self) -> PathBuf {
        self.path.parent().expect("session directory").to_path_buf()
    }
    pub fn recorded(&self) -> Result<bool> {
        Ok(tail(self)?.is_some_and(|(state, _)| state.metadata.is_some()))
    }
    pub fn exists(&self) -> bool {
        self.recorded().unwrap_or(false)
    }

    pub(super) fn cached_index(&self) -> std::sync::MutexGuard<'_, Index> {
        self.index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn refresh(&self) -> Result<std::sync::MutexGuard<'_, Index>> {
        let mut index = self.cached_index();
        if let Err(error) = refresh(self, &mut index) {
            *index = Index::new(self.session.clone());
            return Err(error);
        }
        Ok(index)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct Metadata {
    pub workspace: String,
    pub model: String,
    pub name: Option<String>,
    pub created: u64,
    pub spent: llm::Totals,
    pub parent: Option<String>,
    pub touched: u64,
    pub rounds: usize,
    pub prompt: String,
}

/// Derived in memory, never appended as a snapshot.
#[derive(Debug, Clone)]
pub(super) struct State {
    pub session: message::SessionId,
    pub metadata: Option<Metadata>,
    pub entries: usize,
    pub last: Option<transcript::EntryId>,
    pub next: u64,
    pub interrupted: Option<transcript::StopCause>,
    pub requests: Option<usize>,
}
impl State {
    fn new(session: message::SessionId) -> Self {
        Self {
            session,
            metadata: None,
            entries: 0,
            last: None,
            next: 0,
            interrupted: None,
            requests: None,
        }
    }
}

pub(super) fn tail(at: &Location) -> Result<Option<(State, u64)>> {
    let index = at.refresh()?;
    Ok((index.end > 0).then(|| (index.state.clone(), index.end)))
}

pub(super) fn location(path: PathBuf) -> Result<Option<Location>> {
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line)?;
    if !line.ends_with('\n') {
        return Ok(None);
    }
    let records = parse_line(line.as_bytes())?;
    let Some(Record::Session(header)) = records.first() else {
        anyhow::bail!("session file has no header");
    };
    Ok(Some(Location::new(path, header.id.clone())))
}

fn parse_line(line: &[u8]) -> Result<Vec<Record>> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Line {
        Batch(Vec<Record>),
        One(Box<Record>),
    }
    match serde_json::from_slice(line).context("invalid session record")? {
        Line::Batch(records) => {
            ensure!(!records.is_empty(), "empty session batch");
            Ok(records)
        }
        Line::One(record) => Ok(vec![*record]),
    }
}

fn refresh(at: &Location, index: &mut Index) -> Result<()> {
    let file = match File::open(&at.path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && index.end == 0 => {
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", at.path.display()));
        }
    };
    refresh_from(at, index, file)
}

fn refresh_from(at: &Location, index: &mut Index, mut file: File) -> Result<()> {
    ensure!(
        file.metadata()?.len() >= index.end,
        "session file was truncated"
    );
    file.seek(SeekFrom::Start(index.end))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    loop {
        line.clear();
        let len = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        let records = parse_line(&line)?;
        index.apply(at, index.end, &records)?;
        index.end += len as u64;
    }
    Ok(())
}

/// Parse each batch once while walking a session. This reader is short-lived;
/// only offsets survive in the session index.
pub(super) struct Reader<'a> {
    at: &'a Location,
    file: BufReader<File>,
    offset: Option<u64>,
    records: Vec<Record>,
}

impl<'a> Reader<'a> {
    pub(super) fn new(at: &'a Location) -> Result<Self> {
        Ok(Self {
            at,
            file: BufReader::new(File::open(&at.path)?),
            offset: None,
            records: Vec::new(),
        })
    }
    pub(super) fn record(&mut self, address: Address) -> Result<&Record> {
        if self.offset != Some(address.0) {
            self.file.seek(SeekFrom::Start(address.0))?;
            let mut line = Vec::new();
            self.file.read_until(b'\n', &mut line)?;
            ensure!(line.last() == Some(&b'\n'), "session record is incomplete");
            self.records = parse_line(&line)?;
            self.offset = Some(address.0);
        }
        self.records
            .get(address.1)
            .context("session record reference is missing")
    }
    pub(super) fn entry(&mut self, address: Address) -> Result<Entry> {
        let at = self.at;
        let record = self.record(address)?;
        let canonical = record
            .item()
            .context("missing context item")?
            .resolved_body(at, 0)?;
        record.entry(&canonical)
    }

    pub(super) fn request(&mut self, address: RequestAddress) -> Result<super::messages::Entry> {
        let at = self.at;
        let record = self.record(address.record)?;
        let item = record.item().context("record has no interaction")?;
        let canonical = item.resolved_body(at, 0)?;
        let message = match address.call {
            Some(call) => item.calls.get(call).context("missing tool interaction")?,
            None => item,
        };
        message.message(at, address.record, &canonical)
    }
}

pub(super) fn read_body(
    at: &Location,
    address: Address,
    depth: usize,
) -> Result<serde_json::Value> {
    ensure!(
        depth < REFERENCE_DEPTH,
        "session body reference is too deep"
    );
    let mut reader = Reader::new(at)?;
    reader
        .record(address)?
        .item()
        .context("record has no body")?
        .resolved_body(at, depth + 1)
}

pub(super) fn request(at: &Location, address: RequestAddress) -> Result<super::messages::Entry> {
    Reader::new(at)?.request(address)
}

/// Stage context and protocol facts before touching the archive.
pub struct Transaction {
    pub(super) state: State,
    records: Vec<Record>,
    pending: Vec<(usize, super::messages::Entry)>,
    pub(super) identities: std::collections::HashSet<message::MessageId>,
    pub(super) requests: std::collections::HashSet<message::MessageId>,
    pub(super) responses: std::collections::HashSet<message::MessageId>,
}
impl Transaction {
    pub(super) fn request(&mut self, position: usize, entry: super::messages::Entry) {
        self.pending.push((position, entry));
    }
}

pub(super) fn update<T>(
    at: &Location,
    change: impl FnOnce(&mut Transaction) -> Result<T>,
) -> Result<T> {
    let dir = at.artifacts();
    std::fs::create_dir_all(&dir)?;
    let options = write_options();
    let lock = options.open(dir.join(WRITE_LOCK))?;
    lock.lock()?;
    let (result, records, end) = stage(at, change)?;
    // A complete line can survive a failed sync. Even a no-op retry must sync it.
    let bytes = encode_batch(&records, end)?;
    publish(at, &options, &bytes, end)?;
    // Update the same index readers already hold, only after durable publication.
    drop(at.refresh()?);
    Ok(result)
}

fn stage<T>(
    at: &Location,
    change: impl FnOnce(&mut Transaction) -> Result<T>,
) -> Result<(T, Vec<Record>, u64)> {
    let (before, end) = {
        let index = at.refresh()?;
        (index.state.clone(), index.end)
    };
    let mut tx = Transaction {
        state: before.clone(),
        records: Vec::new(),
        pending: Vec::new(),
        identities: Default::default(),
        requests: Default::default(),
        responses: Default::default(),
    };
    let result = change(&mut tx)?;
    let records = plan::finish(at, &before, &mut tx, end)?;
    Ok((result, records, end))
}

fn write_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn publish(at: &Location, options: &OpenOptions, bytes: &[u8], end: u64) -> Result<()> {
    let mut file = options.open(&at.path)?;
    file.set_len(end)?;
    file.seek(SeekFrom::Start(end))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    sync_dir(&at.artifacts())?;
    Ok(())
}

fn encode_batch(records: &[Record], end: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    // The header may stand alone. Every following save has one newline boundary,
    // including all context, protocol and accounting facts needed together.
    let start = if end == 0 && matches!(records.first(), Some(Record::Session(_))) {
        serde_json::to_writer(&mut bytes, &records[0])?;
        bytes.push(b'\n');
        1
    } else {
        0
    };
    match &records[start..] {
        [] => {}
        [record] => append_line(&mut bytes, record)?,
        records => append_line(&mut bytes, records)?,
    }
    Ok(bytes)
}

fn append_line(bytes: &mut Vec<u8>, value: &(impl Serialize + ?Sized)) -> Result<()> {
    serde_json::to_writer(&mut *bytes, value)?;
    bytes.push(b'\n');
    Ok(())
}

pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

pub(super) fn write(
    tx: &mut Transaction,
    at: &Location,
    workspace: &Path,
    header: Header<'_>,
    session: &Session,
    parent: Option<&str>,
) -> Result<()> {
    let entries = session.entries();
    let previous = tx.state.entries;
    let from = if previous == 0 || entries.get(previous - 1).map(Entry::id) == tx.state.last {
        previous
    } else {
        let index = at.refresh()?;
        index
            .context
            .iter()
            .zip(entries)
            .take_while(|((id, _), entry)| *id == entry.id())
            .count()
    };
    ensure!(from <= entries.len(), "invalid conversation prefix");
    if from < previous {
        tx.records.push(Record::Rewind { from });
    }
    for entry in &entries[from..] {
        tx.records.push(Record::from_entry(entry)?);
    }
    tx.state.entries = entries.len();
    tx.state.last = entries.last().map(Entry::id);
    tx.state.next = session.next_id();
    tx.state.interrupted = session.stopped().cloned();
    let created = tx
        .state
        .metadata
        .as_ref()
        .map_or(header.created, |metadata| metadata.created);
    tx.state.metadata = Some(Metadata {
        workspace: workspace.to_string_lossy().into_owned(),
        model: header.model.into(),
        name: header.name.map(str::to_owned),
        created,
        spent: header.spent,
        parent: parent.map(str::to_owned),
        touched: entries.last().map_or(created, Entry::at),
        rounds: entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Ask { .. }))
            .count(),
        prompt: entries
            .iter()
            .find_map(|entry| match entry {
                Entry::Ask { ask, .. } => {
                    Some(crate::text::clip(ask.shown_text(), super::RESUME_WIDTH))
                }
                _ => None,
            })
            .unwrap_or_default(),
    });
    Ok(())
}

pub fn read(at: &Location) -> Result<Stored> {
    let index = at.refresh()?;
    let metadata = index
        .state
        .metadata
        .clone()
        .context("session has no conversation")?;
    let mut reader = Reader::new(at)?;
    let entries = index
        .context
        .iter()
        .map(|(_, address)| reader.entry(*address))
        .collect::<Result<Vec<_>>>()?;
    let session = Session::from_parts(entries, index.state.next, index.state.interrupted.clone());
    Ok(Stored {
        id: at.session.as_str().into(),
        workspace: metadata.workspace,
        model: metadata.model,
        created: metadata.created,
        name: metadata.name,
        spent: metadata.spent,
        session,
    })
}

pub(super) fn relocate(at: &Location, workspace: &Path) -> Result<()> {
    update(at, |tx| {
        tx.state
            .metadata
            .as_mut()
            .context("session has no conversation")?
            .workspace = workspace.to_string_lossy().into_owned();
        Ok(())
    })
}
