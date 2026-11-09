//! Rebuildable disk index over committed immutable message segments.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use message::{MessageId, SessionId, WireKind};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Entry, Index, Segment, SegmentReader};

const VERSION: i64 = 2;

pub struct Lookup {
    dir: PathBuf,
    connection: Connection,
    session: SessionId,
    count: usize,
}

fn number(value: usize) -> Result<i64> {
    Ok(i64::try_from(value)?)
}

impl Lookup {
    /// Open an already indexed commit for a short read, without retaining a writer.
    pub fn committed(dir: &Path, session: &SessionId, count: usize) -> Result<Self> {
        let connection = Connection::open_with_flags(
            dir.join("lookup.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA cache_size=-1024;")?;
        let (recorded, indexed): (String, i64) = connection.query_row(
            "SELECT session, count FROM progress WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            recorded == session.as_str() && indexed >= number(count)?,
            "message lookup does not contain this commit"
        );
        Ok(Self {
            dir: dir.into(),
            connection,
            session: session.clone(),
            count,
        })
    }

    pub fn open(dir: &Path) -> Result<Option<Self>> {
        let Some(index) = super::index(dir)? else {
            return Ok(None);
        };
        let count = index.segments.last().map_or(0, |s| s.to);
        let mut connection = connect(dir, &index.session)?;
        synchronize(&mut connection, dir, &index)?;
        Ok(Some(Self {
            dir: dir.into(),
            connection,
            session: index.session,
            count,
        }))
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    pub fn count(&self) -> usize {
        self.count
    }
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn position(&self, id: &MessageId, before: usize) -> Result<Option<usize>> {
        let position: Option<i64> = self
            .connection
            .query_row(
                "SELECT position FROM messages WHERE id=?1 AND position<?2",
                params![id.as_str(), number(before)?],
                |row| row.get(0),
            )
            .optional()?;
        position
            .map(usize::try_from)
            .transpose()
            .map_err(Into::into)
    }

    pub fn automatic_request(
        &self,
        source: &str,
        event: &str,
        before: usize,
    ) -> Result<Option<MessageId>> {
        let id: Option<String> = self.connection.query_row(
            "SELECT id FROM messages WHERE source=?1 AND event=?2 AND position<?3 ORDER BY position LIMIT 1",
            params![source, event, number(before)?], |row| row.get(0),
        ).optional()?;
        id.map(MessageId::new).transpose().map_err(Into::into)
    }

    pub fn get(&self, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        self.find("id", id, before)
    }

    pub fn response(&self, request: &MessageId, before: usize) -> Result<Option<Entry>> {
        self.find("request_id", request, before)
    }

    /// Read a committed log position without scanning earlier segments.
    pub fn at(&self, position: usize, before: usize) -> Result<Option<Entry>> {
        if position >= before.min(self.count) {
            return Ok(None);
        }
        let found: Option<(String, i64, i64, i64, i64)> = self
            .connection
            .query_row(
                "SELECT id, segment_from, segment_to, byte_offset, byte_length FROM messages WHERE position=?1",
                [number(position)?],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()?;
        let Some((id, from, to, offset, bytes)) = found else {
            return Ok(None);
        };
        let entry = self.read_at(from, to, offset, bytes)?;
        ensure!(
            entry.message.id.as_str() == id,
            "message index identifier mismatch"
        );
        Ok(Some(entry))
    }

    fn find(&self, column: &str, id: &MessageId, before: usize) -> Result<Option<Entry>> {
        // `column` is supplied only by the two fixed queries above.
        let sql = format!(
            "SELECT segment_from, segment_to, byte_offset, byte_length FROM messages WHERE {column}=?1 AND position<?2"
        );
        let found: Option<(i64, i64, i64, i64)> = self
            .connection
            .query_row(&sql, params![id.as_str(), number(before)?], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        let Some((from, to, offset, bytes)) = found else {
            return Ok(None);
        };
        let entry = self.read_at(from, to, offset, bytes)?;
        match column {
            "id" => ensure!(entry.message.id == *id, "message index identifier mismatch"),
            "request_id" => ensure!(
                matches!(&entry.message.kind, WireKind::Response { request, .. } if request == id),
                "message response index mismatch"
            ),
            _ => unreachable!("fixed query"),
        }
        Ok(Some(entry))
    }

    fn read_at(&self, from: i64, to: i64, offset: i64, bytes: i64) -> Result<Entry> {
        let segment = Segment {
            from: from.try_into()?,
            to: to.try_into()?,
        };
        ensure!(bytes > 0, "invalid message index byte length");
        let path = segment.path(&self.dir);
        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        file.seek(SeekFrom::Start(offset.try_into()?))?;
        serde_json::from_reader(std::io::BufReader::new(file.take(bytes.try_into()?)))
            .with_context(|| format!("invalid indexed message in {}", path.display()))
    }
}

fn connect(dir: &Path, session: &SessionId) -> Result<Connection> {
    let path = dir.join("lookup.sqlite3");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA cache_size=-1024;
        CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY, position INTEGER NOT NULL UNIQUE,
            segment_from INTEGER NOT NULL, segment_to INTEGER NOT NULL, request_id TEXT UNIQUE, source TEXT, event TEXT,
            byte_offset INTEGER NOT NULL, byte_length INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS progress(singleton INTEGER PRIMARY KEY CHECK(singleton=1), count INTEGER NOT NULL, session TEXT NOT NULL);")?;
    connection.execute(
        "INSERT OR IGNORE INTO progress VALUES(1,0,?1)",
        [session.as_str()],
    )?;
    let recorded: String = connection.query_row(
        "SELECT session FROM progress WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        recorded == session.as_str(),
        "message lookup belongs to another session"
    );
    Ok(connection)
}

fn synchronize(connection: &mut Connection, dir: &Path, index: &Index) -> Result<()> {
    let count = index.segments.last().map_or(0, |s| s.to);
    let indexed: i64 =
        connection.query_row("SELECT count FROM progress WHERE singleton=1", [], |row| {
            row.get(0)
        })?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if indexed == number(count)? && version == VERSION {
        return Ok(());
    }
    let from = if version == VERSION {
        usize::try_from(indexed)?.min(count)
    } else {
        0
    };
    let transaction = connection.transaction()?;
    if version != VERSION {
        transaction.execute_batch("DROP TABLE messages;
            CREATE TABLE messages(id TEXT PRIMARY KEY, position INTEGER NOT NULL UNIQUE,
                segment_from INTEGER NOT NULL, segment_to INTEGER NOT NULL, request_id TEXT UNIQUE, source TEXT, event TEXT,
                byte_offset INTEGER NOT NULL, byte_length INTEGER NOT NULL);
            CREATE INDEX automatic_inputs ON messages(source,event,position);
            PRAGMA user_version=2;")?;
    }
    transaction.execute("DELETE FROM messages WHERE position>=?1", [number(from)?])?;
    for segment in index.segments.iter().filter(|s| s.to > from) {
        let mut reader = SegmentReader::open(dir, segment)?;
        while let Some(record) = reader.next()? {
            let super::Positioned {
                entry,
                position,
                offset,
                bytes,
            } = record;
            if position >= from {
                let request = match &entry.message.kind {
                    WireKind::Response { request, .. } => Some(request.as_str()),
                    WireKind::Request(_) => None,
                };
                let trigger = entry.message.automatic_trigger();
                transaction
                    .prepare_cached("INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?
                    .execute(params![
                        entry.message.id.as_str(),
                        number(position)?,
                        number(segment.from)?,
                        number(segment.to)?,
                        request,
                        trigger.map(|(source, _)| source),
                        trigger.map(|(_, event)| event),
                        i64::try_from(offset)?,
                        number(bytes)?
                    ])?;
            }
        }
    }
    transaction.execute(
        "UPDATE progress SET count=?1 WHERE singleton=1",
        [number(count)?],
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_derived_index_rebuilds_and_queries_observe_the_commit_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("s".into()).unwrap();
        let id = MessageId::new("first".into()).unwrap();
        let entry = serde_json::json!({"message": {"id": id, "session": session, "by": null, "at": 1,
            "kind": {"type": "request", "value": {"direction": "to_model", "request": "user"}},
            "body": [{"type": "text", "value": "work"}]}, "parent": null, "native_call": null});
        super::super::write(dir.path(), &session, &[entry]).unwrap();
        let lookup = Lookup::open(dir.path()).unwrap().unwrap();
        assert_eq!(lookup.position(&id, 0).unwrap(), None);
        assert_eq!(lookup.position(&id, 1).unwrap(), Some(0));
        assert_eq!(lookup.get(&id, 1).unwrap().unwrap().message.id, id);
        drop(lookup);
        std::fs::remove_file(dir.path().join("lookup.sqlite3")).unwrap();
        let lookup = Lookup::open(dir.path()).unwrap().unwrap();
        assert_eq!(lookup.position(&id, 1).unwrap(), Some(0));
        assert!(lookup.response(&id, 1).unwrap().is_none());
    }

    #[test]
    fn offsets_preserve_pretty_json_unicode_and_escaped_content_after_index_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("offsets".into()).unwrap();
        let entries: Vec<_> = (0..4).map(|n| serde_json::json!({
            "message": {"id": format!("entry-{n}"), "session": session, "by": null, "at": 1,
                "kind": {"type":"request","value":{"direction":"to_model","request":"user"}},
                "body":[{"type":"text","value":"中文, ] \\\"\n😀"}]},
            "parent": null, "native_call": null,
        })).collect();
        super::super::write(dir.path(), &session, &entries).unwrap();
        let segment = dir.path().join("0-4.json");
        std::fs::write(&segment, serde_json::to_vec_pretty(&entries).unwrap()).unwrap();
        let lookup = Lookup::open(dir.path()).unwrap().unwrap();
        for n in [3, 0, 2, 1] {
            let entry = lookup.at(n, 4).unwrap().unwrap();
            assert_eq!(entry.message.id.as_str(), format!("entry-{n}"));
            assert_eq!(
                serde_json::to_value(entry.message).unwrap(),
                entries[n]["message"]
            );
        }
        lookup
            .connection
            .execute_batch(
                "ALTER TABLE messages DROP COLUMN byte_offset;
            ALTER TABLE messages DROP COLUMN byte_length; PRAGMA user_version=1;",
            )
            .unwrap();
        drop(lookup);
        let lookup = Lookup::open(dir.path()).unwrap().unwrap();
        assert_eq!(
            lookup.at(3, 4).unwrap().unwrap().message.id.as_str(),
            "entry-3"
        );
        assert!(Lookup::committed(dir.path(), &session, 5).is_err());
        let reader = Lookup::committed(dir.path(), &session, 4).unwrap();
        assert_eq!(
            reader.at(2, 4).unwrap().unwrap().message.id.as_str(),
            "entry-2"
        );
    }

    #[test]
    fn offset_index_rejects_invalid_array_separators_and_trailing_data() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new("malformed".into()).unwrap();
        super::super::write::<serde_json::Value>(dir.path(), &session, &[]).unwrap();
        let entry = serde_json::json!({"message": {"id":"entry","session":session,"by":null,"at":1,
            "kind":{"type":"request","value":{"direction":"to_model","request":"user"}},
            "body":[{"type":"text","value":"input"}]},"parent":null,"native_call":null});
        let mut second = entry.clone();
        second["message"]["id"] = "second".into();
        super::super::write(dir.path(), &session, &[entry, second]).unwrap();
        let path = dir.path().join("0-2.json");
        let original = std::fs::read_to_string(&path).unwrap();
        for malformed in [
            original.replacen("},{", "}{", 1),
            format!("{original}x"),
            original.replacen("},{", "},,{", 1),
        ] {
            std::fs::write(&path, malformed).unwrap();
            assert!(Lookup::open(dir.path()).is_err());
        }
    }
}
