//! Immutable message segments, committed by one atomic session index.

mod lookup;
pub use lookup::Lookup;

use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use message::{MessageId, SessionId, WireMessage};
use serde::{Deserialize, Serialize};

pub const FORMAT: u8 = 1;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub format: u8,
    pub session: SessionId,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub message: WireMessage,
    pub parent: Option<MessageId>,
    pub native_call: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format: u8,
    session: SessionId,
    segments: Vec<Segment>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Segment {
    from: usize,
    to: usize,
}

impl Segment {
    fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}-{}.json", self.from, self.to))
    }
}

/// Streams an existing JSON array while retaining each entry's exact byte range.
struct SegmentReader {
    file: BufReader<std::fs::File>,
    next: usize,
    to: usize,
}

struct Positioned {
    entry: Entry,
    position: usize,
    offset: u64,
    bytes: usize,
}

impl SegmentReader {
    fn open(dir: &Path, segment: &Segment) -> Result<Self> {
        let path = segment.path(dir);
        let file = std::fs::File::open(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut reader = Self {
            file: BufReader::new(file),
            next: segment.from,
            to: segment.to,
        };
        ensure!(reader.nonspace()? == Some(b'['), "invalid message segment");
        Ok(reader)
    }

    fn nonspace(&mut self) -> Result<Option<u8>> {
        let mut byte = [0];
        while self.file.read(&mut byte)? != 0 {
            if !matches!(byte[0], b' ' | b'\t' | b'\n' | b'\r') {
                return Ok(Some(byte[0]));
            }
        }
        Ok(None)
    }

    fn next(&mut self) -> Result<Option<Positioned>> {
        if self.next == self.to {
            return Ok(None);
        }
        let offset = self.file.stream_position()?;
        let mut values = serde_json::Deserializer::from_reader(&mut self.file).into_iter::<Entry>();
        let entry = values.next().context("message segment is too short")??;
        let bytes = values.byte_offset();
        drop(values);
        // serde's reader can retain a lookahead byte. Put that byte back without
        // discarding the buffered remainder of this immutable segment.
        let end = offset
            .checked_add(bytes.try_into()?)
            .context("message offset overflow")?;
        let adjustment = i64::try_from(end)? - i64::try_from(self.file.stream_position()?)?;
        self.file.seek_relative(adjustment)?;
        let position = self.next;
        self.next += 1;
        let delimiter = if self.next == self.to { b']' } else { b',' };
        ensure!(
            self.nonspace()? == Some(delimiter),
            "invalid message segment length or separator"
        );
        if self.next == self.to {
            ensure!(
                self.nonspace()?.is_none(),
                "message segment has trailing data"
            );
        }
        Ok(Some(Positioned {
            entry,
            position,
            offset,
            bytes,
        }))
    }
}

pub fn path(transcript: &Path) -> PathBuf {
    transcript.with_file_name("messages")
}

fn index(dir: &Path) -> Result<Option<Index>> {
    let path = dir.join("index.json");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let index: Index = serde_json::from_reader(std::io::BufReader::new(file))
        .with_context(|| format!("invalid message index {}", path.display()))?;
    ensure!(
        index.format == FORMAT,
        "unsupported message archive format {}",
        index.format
    );
    let mut next = 0;
    for segment in &index.segments {
        ensure!(
            segment.from == next && segment.to > segment.from,
            "invalid message segment range"
        );
        next = segment.to;
    }
    Ok(Some(index))
}

fn visit_segment(
    dir: &Path,
    segment: &Segment,
    mut visit: impl FnMut(usize, Entry) -> Result<()>,
) -> Result<()> {
    struct Entries<'a, F> {
        from: usize,
        to: usize,
        visit: &'a mut F,
    }
    impl<'de, F: FnMut(usize, Entry) -> Result<()>> serde::de::Visitor<'de> for Entries<'_, F> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a message segment")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<(), A::Error> {
            let mut at = self.from;
            while let Some(entry) = sequence.next_element()? {
                if at >= self.to {
                    return Err(serde::de::Error::custom("message segment is too long"));
                }
                (self.visit)(at, entry).map_err(serde::de::Error::custom)?;
                at += 1;
            }
            if at != self.to {
                return Err(serde::de::Error::custom("message segment is too short"));
            }
            Ok(())
        }
    }
    let path = segment.path(dir);
    let file =
        std::fs::File::open(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut decoder = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
    serde::de::Deserializer::deserialize_seq(
        &mut decoder,
        Entries {
            from: segment.from,
            to: segment.to,
            visit: &mut visit,
        },
    )?;
    decoder.end()?;
    Ok(())
}

pub fn visit(dir: &Path, mut visit: impl FnMut(usize, Entry) -> Result<()>) -> Result<()> {
    let index = index(dir)?.context("message archive has no index")?;
    for segment in index.segments {
        visit_segment(dir, &segment, &mut visit)?;
    }
    Ok(())
}

pub fn read(dir: &Path) -> Result<Option<Archive>> {
    let Some(index) = index(dir)? else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    for segment in index.segments {
        visit_segment(dir, &segment, |_, entry| {
            entries.push(entry);
            Ok(())
        })?;
    }
    Ok(Some(Archive {
        format: index.format,
        session: index.session,
        entries,
    }))
}

/// Existing entries are immutable. Only the new suffix is serialized and synced.
/// Orphan segments from an interrupted save are invisible until the index commits them.
pub fn write<T: Serialize>(dir: &Path, session: &SessionId, entries: &[T]) -> Result<()> {
    if index(dir)?.is_some_and(|index| index.segments.last().is_some_and(|s| s.to > entries.len()))
    {
        bail!("message archive cannot discard recorded interactions");
    }
    write_from(dir, session, 0, entries)
}

pub fn write_from<T: Serialize>(
    dir: &Path,
    session: &SessionId,
    base: usize,
    entries: &[T],
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let previous = index(dir)?;
    let fresh = previous.is_none();
    let mut index = previous.unwrap_or_else(|| Index {
        format: FORMAT,
        session: session.clone(),
        segments: Vec::new(),
    });
    ensure!(
        &index.session == session,
        "message archive belongs to another session"
    );
    let from = index.segments.last().map_or(0, |segment| segment.to);
    let end = base
        .checked_add(entries.len())
        .context("message archive size overflow")?;
    if base > from {
        bail!("message archive is missing an earlier save");
    }
    if end < from {
        return Ok(());
    }
    if end > from {
        let segment = Segment { from, to: end };
        atomic_write(&segment.path(dir), &entries[from - base..])?;
        index.segments.push(segment);
    } else if !fresh {
        return Ok(());
    }
    atomic_write(&dir.join("index.json"), &index)?;
    if let Some(parent) = dir.parent() {
        super::sync_dir(parent);
    }
    Ok(())
}

fn atomic_write(path: &Path, value: &(impl Serialize + ?Sized)) -> Result<()> {
    let dir = path
        .parent()
        .context("message archive has no parent directory")?;
    let temp = path.with_extension(format!("{}.tmp", super::new_id()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&temp)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        std::fs::rename(&temp, path)?;
        super::sync_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("cannot save message archive {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &SessionId, id: &str) -> serde_json::Value {
        serde_json::json!({
            "message": { "id": id, "session": session, "by": null, "at": 1,
                "kind": { "type": "request", "value": { "direction": "to_model", "request": "user" } },
                "body": [{ "type": "text", "value": "hello" }] },
            "parent": null, "native_call": null,
        })
    }

    #[test]
    fn append_preserves_the_existing_segment_and_orphans_do_not_commit() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("s".into()).unwrap();
        let mut entries = vec![entry(&session, "first")];
        write(dir.path(), &session, &entries).unwrap();
        let before = std::fs::read(dir.path().join("0-1.json")).unwrap();
        std::fs::write(dir.path().join("1-2.json"), b"partial write").unwrap();
        assert_eq!(read(dir.path()).unwrap().unwrap().entries.len(), 1);
        entries.push(entry(&session, "second"));
        write(dir.path(), &session, &entries).unwrap();
        assert_eq!(std::fs::read(dir.path().join("0-1.json")).unwrap(), before);
        assert_eq!(read(dir.path()).unwrap().unwrap().entries.len(), 2);
        assert!(write(dir.path(), &session, &entries[..1]).is_err());
    }

    #[test]
    fn unknown_versions_and_missing_committed_segments_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("s".into()).unwrap();
        write(dir.path(), &session, &[entry(&session, "first")]).unwrap();
        std::fs::remove_file(dir.path().join("0-1.json")).unwrap();
        assert!(read(dir.path()).is_err());
        std::fs::write(
            dir.path().join("index.json"),
            r#"{"format":255,"session":"s","segments":[]}"#,
        )
        .unwrap();
        assert!(read(dir.path()).is_err());
    }
}
