//! One workspace database. Conversation and request commits share a transaction.

use super::{Header, Stored};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use transcript::{Session, archive::Record};

pub const FILE: &str = "project.db";
const VERSION: i64 = 2;

/// An explicit session address; the database is shared by the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub database: PathBuf,
    pub session: message::SessionId,
}

impl Location {
    pub fn new(database: PathBuf, session: message::SessionId) -> Self {
        Self { database, session }
    }

    pub fn artifacts(&self) -> PathBuf {
        self.database
            .parent()
            .expect("workspace database")
            .join(tool::state::file_stem(self.session.as_str()))
    }

    pub fn recorded(&self) -> Result<bool> {
        if !self.database.is_file() {
            return Ok(false);
        }
        let c = connect(&self.database)?;
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
            [self.session.as_str()],
            |r| r.get(0),
        )?)
    }

    pub fn exists(&self) -> bool {
        self.recorded().unwrap_or(false)
    }
}

pub fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let initialization = options.open(path)?;
    initialization.lock()?;
    let mut c = Connection::open(path)?;
    c.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        version <= VERSION,
        "unsupported workspace database version {version}"
    );
    c.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")?;
    if version == VERSION {
        return Ok(c);
    }
    if version == 0 {
        c.execute_batch(
        "PRAGMA journal_mode=WAL;
        CREATE TABLE IF NOT EXISTS sessions(
            id TEXT PRIMARY KEY, workspace TEXT NOT NULL, model TEXT NOT NULL,
            name TEXT, created INTEGER NOT NULL, touched INTEGER NOT NULL,
            spent TEXT NOT NULL, next INTEGER NOT NULL, interrupted TEXT,
            parent TEXT, last INTEGER NOT NULL, rounds INTEGER NOT NULL, prompt TEXT NOT NULL,
            bytes INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS contents(id INTEGER PRIMARY KEY, body TEXT NOT NULL UNIQUE);
        CREATE TABLE IF NOT EXISTS bodies(id INTEGER PRIMARY KEY, parts TEXT NOT NULL UNIQUE);
        CREATE VIEW IF NOT EXISTS body_values AS SELECT b.id,
            (SELECT json_group_array(json(c.body)) FROM json_each(b.parts) p JOIN contents c ON c.id=p.value) body FROM bodies b;
        CREATE TABLE IF NOT EXISTS entries(
            session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
            id INTEGER NOT NULL, metadata TEXT NOT NULL,
            PRIMARY KEY(session,id));
        CREATE TABLE IF NOT EXISTS context_entries(
            session TEXT NOT NULL, seq INTEGER NOT NULL, entry_id INTEGER NOT NULL,
            PRIMARY KEY(session,seq),
            FOREIGN KEY(session,entry_id) REFERENCES entries(session,id) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS messages(
            session TEXT NOT NULL, entry_id INTEGER NOT NULL, part INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('user','assistant','tool_call','tool_result')),
            metadata TEXT NOT NULL, body INTEGER NOT NULL REFERENCES bodies(id),
            PRIMARY KEY(session,entry_id,part),
            FOREIGN KEY(session,entry_id) REFERENCES entries(session,id) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS request_commits(session TEXT PRIMARY KEY, count INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS requests(
            session TEXT NOT NULL, position INTEGER NOT NULL, id TEXT NOT NULL,
            request_id TEXT, source TEXT, event TEXT, metadata TEXT NOT NULL,
            body INTEGER NOT NULL REFERENCES bodies(id),
            PRIMARY KEY(session,position), UNIQUE(session,id), UNIQUE(session,request_id));
        CREATE INDEX IF NOT EXISTS automatic_inputs ON requests(session,source,event,position);",
        )?;
    }
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS context_entries_by_entry ON context_entries(session,entry_id);
         CREATE INDEX IF NOT EXISTS sessions_by_parent ON sessions(parent);",
    )?;
    tx.pragma_update(None, "user_version", VERSION)?;
    tx.commit()?;
    Ok(c)
}

pub fn intern(c: &Connection, body: &serde_json::Value) -> Result<i64> {
    let mut insert = c.prepare_cached("INSERT OR IGNORE INTO contents(body) VALUES(?1)")?;
    let mut lookup = c.prepare_cached("SELECT id FROM contents WHERE body=?1")?;
    let mut parts = Vec::new();
    for block in body.as_array().context("message body must be an array")? {
        let block = serde_json::to_string(block)?;
        insert.execute([&block])?;
        parts.push(lookup.query_row([block], |r| r.get::<_, i64>(0))?);
    }
    let parts = serde_json::to_string(&parts)?;
    c.prepare_cached("INSERT OR IGNORE INTO bodies(parts) VALUES(?1)")?
        .execute([&parts])?;
    Ok(c.prepare_cached("SELECT id FROM bodies WHERE parts=?1")?
        .query_row([parts], |r| r.get(0))?)
}

/// Split bodies from metadata, so protocol references never duplicate content.
pub fn split(
    content: &message::conversation::Content,
) -> Result<(String, serde_json::Value, serde_json::Value)> {
    use message::{Block, ToolResultContent, conversation::Content};
    let (kind, metadata, body) = match content {
        Content::User(body) => ("user", serde_json::Value::Null, serde_json::to_value(body)?),
        Content::Assistant(body) => (
            "assistant",
            serde_json::Value::Null,
            serde_json::to_value(body)?,
        ),
        Content::ToolCall(call) => (
            "tool_call",
            serde_json::json!({"id":call.id,"name":call.name}),
            serde_json::to_value([Block::Data(call.args.clone())])?,
        ),
        Content::ToolResult(result) => (
            "tool_result",
            serde_json::json!({"call":result.call,"name":result.name,"is_error":result.is_error}),
            serde_json::to_value(
                result
                    .content
                    .iter()
                    .map(|b| match b {
                        ToolResultContent::Text(t) => Block::Text(t.text.clone()),
                        ToolResultContent::Image(i) => Block::Image(i.clone()),
                        ToolResultContent::Json { value } => Block::Data(value.clone()),
                    })
                    .collect::<Vec<_>>(),
            )?,
        ),
    };
    Ok((kind.into(), metadata, body))
}

pub fn join(kind: &str, metadata: &str, body: &str) -> Result<message::conversation::Content> {
    use message::{Block, Text, ToolCall, ToolResult, ToolResultContent, conversation::Content};
    let meta: serde_json::Value = serde_json::from_str(metadata)?;
    let body: Vec<Block> = serde_json::from_str(body)?;
    Ok(match kind {
        "user" => Content::User(body),
        "assistant" => Content::Assistant(body),
        "tool_call" => Content::ToolCall(ToolCall {
            id: meta["id"].as_str().context("missing call identity")?.into(),
            name: meta["name"].as_str().context("missing tool name")?.into(),
            args: match <[Block; 1]>::try_from(body) {
                Ok([Block::Data(value)]) => value,
                _ => anyhow::bail!("invalid stored tool arguments"),
            },
        }),
        "tool_result" => Content::ToolResult(ToolResult {
            call: meta["call"].as_str().context("missing result call")?.into(),
            name: meta["name"].as_str().context("missing result tool")?.into(),
            is_error: meta["is_error"]
                .as_bool()
                .context("missing result status")?,
            content: body
                .into_iter()
                .map(|b| match b {
                    Block::Text(text) => Ok(ToolResultContent::Text(Text { text })),
                    Block::Image(i) => Ok(ToolResultContent::Image(i)),
                    Block::Data(value) => Ok(ToolResultContent::Json { value }),
                    Block::Reasoning(_) => anyhow::bail!("reasoning in tool result"),
                })
                .collect::<Result<_>>()?,
        }),
        _ => anyhow::bail!("invalid conversation kind {kind}"),
    })
}

pub fn write(
    c: &Transaction<'_>,
    workspace: &Path,
    header: Header<'_>,
    session: &Session,
    parent: Option<&str>,
) -> Result<()> {
    let entries = session.entries();
    let touched = entries.last().map_or(header.created, |e| e.at());
    let last = entries.last().map_or(0, |e| e.id().0);
    let rounds = entries
        .iter()
        .filter(|e| matches!(e, transcript::Entry::Ask { .. }))
        .count();
    let prompt = entries
        .iter()
        .find_map(|e| match e {
            transcript::Entry::Ask { ask, .. } => Some(ask.shown_text()),
            _ => None,
        })
        .unwrap_or_default();
    c.execute("INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,0)
        ON CONFLICT(id) DO UPDATE SET workspace=excluded.workspace,model=excluded.model,name=excluded.name,
        touched=excluded.touched,spent=excluded.spent,next=excluded.next,interrupted=excluded.interrupted,
        parent=excluded.parent,last=excluded.last,rounds=excluded.rounds,prompt=excluded.prompt", params![
        header.id, workspace.to_string_lossy(), header.model, header.name, header.created, touched,
        serde_json::to_string(&header.spent)?, session.next_id(), serde_json::to_string(&session.stopped())?, parent,last,rounds,prompt])?;
    let from = common_prefix(c, header.id, entries)?;
    let mut added_bytes = 0usize;
    c.execute(
        "DELETE FROM context_entries WHERE session=?1 AND seq>=?2",
        params![header.id, from],
    )?;
    for (seq, entry) in entries.iter().enumerate().skip(from) {
        let record = Record::from_entry(entry);
        let annotation = serde_json::to_string(&record.metadata)?;
        let inserted = c.execute(
            "INSERT OR IGNORE INTO entries VALUES(?1,?2,?3)",
            params![header.id, entry.id().0, &annotation],
        )?;
        c.execute(
            "INSERT INTO context_entries VALUES(?1,?2,?3)",
            params![header.id, seq, entry.id().0],
        )?;
        if inserted == 0 {
            continue;
        }
        added_bytes += annotation.len();
        for (part, content) in record.messages.iter().enumerate() {
            let (kind, metadata, body) = split(content)?;
            let metadata = serde_json::to_string(&metadata)?;
            added_bytes += metadata.len() + serde_json::to_string(&body)?.len();
            let body = intern(c, &body)?;
            c.execute(
                "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6)",
                params![header.id, entry.id().0, part, kind, metadata, body],
            )?;
        }
    }
    c.execute(
        "UPDATE sessions SET bytes=bytes+?2 WHERE id=?1",
        params![header.id, added_bytes],
    )?;
    Ok(())
}

pub fn read(at: &Location) -> Result<Stored> {
    ensure!(
        at.database.is_file(),
        "no workspace database at {}",
        at.database.display()
    );
    let mut c = connect(&at.database)?;
    let tx = c.transaction()?;
    let header = tx
        .query_row(
            "SELECT workspace,model,name,created,spent,next,interrupted FROM sessions WHERE id=?1",
            [at.session.as_str()],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, u64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, u64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?
        .context("session is not recorded")?;
    let entries = read_entries(&tx, at.session.as_str())?;
    Ok(Stored {
        id: at.session.as_str().into(),
        workspace: header.0,
        model: header.1,
        name: header.2,
        created: header.3,
        spent: serde_json::from_str(&header.4)?,
        session: Session::from_parts(entries, header.5, serde_json::from_str(&header.6)?),
    })
}

fn common_prefix(c: &Connection, session: &str, entries: &[transcript::Entry]) -> Result<usize> {
    let last: Option<(usize, u64)> = c
        .query_row(
            "SELECT seq,entry_id FROM context_entries WHERE session=?1 ORDER BY seq DESC LIMIT 1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((seq, id)) = last else { return Ok(0) };
    if entries.get(seq).is_some_and(|e| e.id().0 == id) {
        return Ok(seq + 1);
    }
    let mut statement =
        c.prepare("SELECT entry_id FROM context_entries WHERE session=?1 ORDER BY seq")?;
    let rows = statement.query_map([session], |r| r.get::<_, u64>(0))?;
    let mut count = 0;
    for (entry, id) in entries.iter().zip(rows) {
        if entry.id().0 != id? {
            break;
        }
        count += 1;
    }
    Ok(count)
}

fn read_entries(c: &Connection, session: &str) -> Result<Vec<transcript::Entry>> {
    let mut statement = c.prepare(
        "SELECT e.id,e.metadata,m.kind,m.metadata,b.body FROM context_entries active
        JOIN entries e ON e.session=active.session AND e.id=active.entry_id
        LEFT JOIN messages m ON m.session=e.session AND m.entry_id=e.id
        LEFT JOIN body_values b ON b.id=m.body WHERE active.session=?1 ORDER BY active.seq,m.part",
    )?;
    let mut rows = statement.query([session])?;
    let mut entries = Vec::new();
    let mut current: Option<(u64, Record)> = None;
    while let Some(row) = rows.next()? {
        let id: u64 = row.get(0)?;
        if current.as_ref().is_none_or(|(previous, _)| *previous != id) {
            if let Some((_, record)) = current.take() {
                entries.push(record.into_entry()?);
            }
            let metadata: String = row.get(1)?;
            current = Some((
                id,
                Record {
                    metadata: serde_json::from_str(&metadata)?,
                    messages: Vec::new(),
                },
            ));
        }
        if let Some(kind) = row.get::<_, Option<String>>(2)? {
            let metadata: String = row.get(3)?;
            let body: String = row.get(4)?;
            current
                .as_mut()
                .expect("entry initialized")
                .1
                .messages
                .push(join(&kind, &metadata, &body)?);
        }
    }
    if let Some((_, record)) = current {
        entries.push(record.into_entry()?);
    }
    Ok(entries)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub workspace: String,
    pub created: u64,
    pub touched: u64,
    pub last: u64,
    pub name: Option<String>,
    pub rounds: usize,
    pub prompt: String,
    pub bytes: u64,
    pub parent: Option<String>,
}

const SUMMARY_QUERY: &str =
    "SELECT id,workspace,created,touched,last,name,rounds,prompt,bytes,parent FROM sessions";

impl Summary {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            workspace: row.get(1)?,
            created: row.get(2)?,
            touched: row.get(3)?,
            last: row.get(4)?,
            name: row.get(5)?,
            rounds: row.get(6)?,
            prompt: row.get(7)?,
            bytes: row.get(8)?,
            parent: row.get(9)?,
        })
    }
}

pub fn summary(path: &Path, id: &str) -> Result<Option<Summary>> {
    if !path.is_file() {
        return Ok(None);
    }
    let c = connect(path)?;
    Ok(c.query_row(
        &format!("{SUMMARY_QUERY} WHERE id=?1"),
        [id],
        Summary::from_row,
    )
    .optional()?)
}

pub fn summaries(path: &Path) -> Result<Vec<Summary>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let c = connect(path)?;
    let mut statement = c.prepare(SUMMARY_QUERY)?;
    Ok(statement
        .query_map([], Summary::from_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub fn delete(at: &Location) -> Result<()> {
    delete_many(&at.database, &[at.session.as_str()])
}

/// Remove selected conversations atomically and collect shared content once.
pub fn delete_many(path: &Path, sessions: &[&str]) -> Result<()> {
    if sessions.is_empty() {
        return Ok(());
    }
    let mut c = connect(path)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for id in sessions {
        tx.prepare_cached("DELETE FROM requests WHERE session=?1 OR session IN (SELECT id FROM sessions WHERE parent=?1)")?.execute([id])?;
        tx.prepare_cached("DELETE FROM request_commits WHERE session=?1 OR session IN (SELECT id FROM sessions WHERE parent=?1)")?.execute([id])?;
        tx.prepare_cached("DELETE FROM sessions WHERE id=?1 OR parent=?1")?
            .execute([id])?;
    }
    tx.execute("DELETE FROM bodies WHERE id NOT IN (SELECT body FROM messages UNION SELECT body FROM requests)", [])?;
    tx.execute(
        "DELETE FROM contents WHERE id NOT IN (SELECT p.value FROM bodies b,json_each(b.parts) p)",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// Copy before deleting: interruption can leave a duplicate, never lose the session.
pub fn relocate(from: &Location, to: &Location, workspace: &Path) -> Result<()> {
    if from.database == to.database {
        let c = connect(&to.database)?;
        c.execute(
            "UPDATE sessions SET workspace=?1 WHERE id=?2 OR parent=?2",
            params![workspace.to_string_lossy(), to.session.as_str()],
        )?;
        return Ok(());
    }
    let mut c = connect(&to.database)?;
    c.execute(
        "ATTACH DATABASE ?1 AS previous",
        [from.database.to_string_lossy()],
    )?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let id = from.session.as_str();
    tx.execute(
        "INSERT INTO sessions SELECT * FROM previous.sessions WHERE id=?1 OR parent=?1",
        [id],
    )?;
    tx.execute(
        "UPDATE sessions SET workspace=?1 WHERE id=?2 OR parent=?2",
        params![workspace.to_string_lossy(), id],
    )?;
    tx.execute("INSERT INTO request_commits SELECT * FROM previous.request_commits WHERE session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1)",[id])?;
    tx.execute_batch("CREATE TEMP TABLE body_map(old INTEGER PRIMARY KEY,new INTEGER NOT NULL);")?;
    {
        let mut statement=tx.prepare("SELECT id,body FROM previous.body_values WHERE id IN (SELECT body FROM previous.messages WHERE session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1) UNION SELECT body FROM previous.requests WHERE session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1))")?;
        let rows =
            statement.query_map([id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (old, body) = row?;
            let new = intern(&tx, &serde_json::from_str(&body)?)?;
            tx.execute("INSERT INTO body_map VALUES(?1,?2)", params![old, new])?;
        }
    }
    tx.execute("INSERT INTO entries SELECT * FROM previous.entries WHERE session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1)",[id])?;
    tx.execute("INSERT INTO context_entries SELECT * FROM previous.context_entries WHERE session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1)",[id])?;
    tx.execute("INSERT INTO messages SELECT m.session,m.entry_id,m.part,m.kind,m.metadata,b.new FROM previous.messages m JOIN body_map b ON b.old=m.body WHERE m.session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1)",[id])?;
    tx.execute("INSERT INTO requests SELECT r.session,r.position,r.id,r.request_id,r.source,r.event,r.metadata,b.new FROM previous.requests r JOIN body_map b ON b.old=r.body WHERE r.session IN (SELECT id FROM sessions WHERE id=?1 OR parent=?1)",[id])?;
    tx.commit()?;
    c.execute_batch("DETACH DATABASE previous")?;
    delete(from)
}
