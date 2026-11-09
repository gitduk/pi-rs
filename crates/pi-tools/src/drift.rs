//! What changed on disk since the model last saw it, told before it next
//! speaks rather than when an edit trips over it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError};
use std::time::SystemTime;

use tool::limit::{MAX_BYTES, over_limit};
use tool::row::{Row, render};
use tool::{Ctx, ToolOutput};

use crate::edit::{CHANGED_RUN, rewrite_rows, rewrite_sketch};

/// The diff of each file from its last view to disk, or `None`; each change
/// is told once, the view moving to what is on disk now. A file deleted, or
/// grown past the read limit, is told and forgotten: it is to be read again.
pub fn drift(ctx: &Ctx) -> Option<String> {
    let mut told = Vec::new();
    for (path, was) in seen(ctx) {
        let Ok(now) = on_disk(&path) else {
            continue;
        };
        if now.matches(&was) {
            continue;
        }
        told.push(change(
            &ctx.workspace.display(&path),
            &was,
            &now,
            CHANGED_RUN,
        ));
        settle(ctx, &path, &was, now.view());
    }
    (!told.is_empty()).then(|| {
        let files = llm::figures::count(told.len(), "file");
        format!(
            "{files} changed since you last saw them:\n{}",
            told.join("\n")
        )
    })
}

/// Each viewed file's modified time before a command the model ran, so
/// what the command changed can be told apart from what changed around it.
pub struct Before(Vec<(PathBuf, Arc<str>, Option<SystemTime>)>);

pub fn before(ctx: &Ctx) -> Before {
    Before(
        (seen(ctx).into_iter())
            .map(|(path, was)| {
                let at = modified(&path);
                (path, was, at)
            })
            .collect(),
    )
}

/// Move the view of each file the command touched to what is on disk: the
/// model made the change, so it is no drift to tell. A file changed before
/// the command ran is left for `drift`. Returns each file moved.
pub fn absorb(ctx: &Ctx, before: Before) -> Vec<Moved> {
    let mut moved = Vec::new();
    for (path, was, at) in before.0 {
        if modified(&path) == at {
            continue;
        }
        let Ok(now) = on_disk(&path) else {
            continue;
        };
        // Rewritten as it was, or viewed anew by a tool that said so itself.
        if now.matches(&was) || !settle(ctx, &path, &was, now.view()) {
            continue;
        }
        let rel = ctx.workspace.display(&path);
        tracing::info!(target: "pi::drift", path = %rel, "absorbed");
        moved.push(Moved { rel, was, now });
    }
    moved
}

/// A viewed file a command changed: the view before it, and disk after.
pub struct Moved {
    rel: String,
    was: Arc<str>,
    now: Disk,
}

impl Moved {
    /// The change as a diff, its changed runs cut past `changed_run` rows.
    pub fn sketch(&self, changed_run: usize) -> String {
        change(&self.rel, &self.was, &self.now, changed_run)
    }

    /// The same change, as the rows a screen draws whole.
    fn rows(&self) -> Vec<Row> {
        match &self.now {
            Disk::Text(now) => rewrite_rows(&self.rel, &self.was, now, usize::MAX),
            Disk::Over(len) => vec![Row::Output(over_limit(&self.rel, *len))],
            Disk::Gone => vec![Row::Output(format!("{} deleted", self.rel))],
        }
    }
}

/// `out` with what the command changed told after it: to the model with long
/// runs cut, on screen whole under the row's head, below what it printed.
pub fn tell(mut out: ToolOutput, output: &[String], moved: &[Moved]) -> ToolOutput {
    if let Some(report) = report(moved) {
        out = report.append(out, output);
    }
    out
}

pub(crate) struct Report {
    pub content: String,
    /// The same change, whole: what the screen draws under the head.
    rows: Vec<Row>,
}

pub(crate) fn report(moved: &[Moved]) -> Option<Report> {
    if moved.is_empty() {
        return None;
    }
    let all = |run| {
        (moved.iter().map(|m| m.sketch(run)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let files = llm::figures::count(moved.len(), "file");
    Some(Report {
        content: format!(
            "{files} from your earlier reads changed while this command ran:\n{}",
            all(CHANGED_RUN)
        ),
        rows: moved.iter().flat_map(Moved::rows).collect(),
    })
}

// What it printed stays a glimpse: enough to see the command said something,
// few enough that the diff keeps its window.
const PRINTED_ROWS: usize = 3;

impl Report {
    pub fn append(self, mut out: ToolOutput, output: &[String]) -> ToolOutput {
        out.content
            .push(llm::message::ToolResultContent::Text(llm::message::Text {
                text: self.content,
            }));
        let head = out.preview();
        let head = head.lines().next().unwrap_or_default();
        let mut rows: Vec<Row> = output
            .iter()
            .filter(|row| !row.trim().is_empty())
            .take(PRINTED_ROWS)
            .map(|row| Row::Output(row.clone()))
            .collect();
        rows.extend(self.rows);
        out.preview = Some(if rows.is_empty() {
            head.to_string()
        } else {
            format!("{head}\n{}", render(&rows))
        });
        out
    }
}

fn change(rel: &str, was: &str, now: &Disk, changed_run: usize) -> String {
    match now {
        Disk::Text(now) => rewrite_sketch(rel, was, now, changed_run),
        Disk::Over(len) => over_limit(rel, *len),
        Disk::Gone => format!("{rel} deleted"),
    }
}

// A copy of the views, so the reads run without holding the lock every view
// takes; in path order, so what is told reads the same each time.
fn seen(ctx: &Ctx) -> Vec<(PathBuf, Arc<str>)> {
    let mut seen: Vec<_> = (ctx
        .viewed
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter())
    .map(|(path, was)| (path.clone(), was.clone()))
    .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    seen
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

enum Disk {
    Text(Arc<str>),
    Over(u64),
    Gone,
}

impl Disk {
    fn matches(&self, view: &str) -> bool {
        matches!(self, Disk::Text(now) if **now == *view)
    }

    // What the view becomes: none for a file to be read again.
    fn view(&self) -> Option<Arc<str>> {
        match self {
            Disk::Text(now) => Some(now.clone()),
            Disk::Over(_) | Disk::Gone => None,
        }
    }
}

// Never more than the read limit in memory, however large the file grew.
fn on_disk(path: &Path) -> std::io::Result<Disk> {
    let file = match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Disk::Gone),
        file => file?,
    };
    let len = file.metadata()?.len();
    if len > MAX_BYTES {
        return Ok(Disk::Over(len));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Ok(Disk::Over(bytes.len() as u64));
    }
    Ok(Disk::Text(String::from_utf8_lossy(&bytes).into()))
}

// Moves the view only if no tool noted a newer one while the file was read.
fn settle(ctx: &Ctx, path: &Path, was: &Arc<str>, now: Option<Arc<str>>) -> bool {
    let mut viewed = ctx.viewed.lock().unwrap_or_else(PoisonError::into_inner);
    if !viewed.get(path).is_some_and(|cur| Arc::ptr_eq(cur, was)) {
        return false;
    }
    match now {
        Some(now) => viewed.insert(path.to_path_buf(), now),
        None => viewed.remove(path),
    };
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{ctx, seen};

    #[test]
    fn tells_each_change_once_and_not_the_models_own() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        std::fs::write(&a, "one\ntwo\n").unwrap();
        std::fs::write(&b, "kept\n").unwrap();
        ctx.note_view(&a, "one\ntwo\n");
        ctx.note_view(&b, "kept\n");
        assert_eq!(drift(&ctx), None);

        std::fs::write(&a, "one\n2\n").unwrap();
        std::fs::remove_file(&b).unwrap();
        let told = drift(&ctx).unwrap();
        assert!(told.starts_with("2 files changed"), "{told}");
        assert!(told.contains("a.txt +1 -1"), "{told}");
        assert!(told.contains("-2 two") && told.contains("+2 2"), "{told}");
        assert!(told.contains("b.txt deleted"), "{told}");
        assert_eq!(drift(&ctx), None);
        assert!(ctx.viewed(&b).is_none());
    }

    #[test]
    fn what_a_command_changed_is_absorbed_what_came_before_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let (ran, theirs, gone, reread) = (
            seen(dir.path(), &ctx, "ran.txt", "old\n"),
            seen(dir.path(), &ctx, "theirs.txt", "old\n"),
            seen(dir.path(), &ctx, "gone.txt", "old\n"),
            seen(dir.path(), &ctx, "reread.txt", "old\n"),
        );
        std::fs::write(&theirs, "user\n").unwrap();

        let before = before(&ctx);
        std::fs::write(&ran, "model\n").unwrap();
        std::fs::remove_file(&gone).unwrap();
        // Another tool in the same turn wrote it and noted its own view.
        std::fs::write(&reread, "edited\n").unwrap();
        ctx.note_view(&reread, "edited\n");
        let moved = absorb(&ctx, before);

        let rels: Vec<_> = moved.iter().map(|m| m.rel.as_str()).collect();
        assert_eq!(rels, ["gone.txt", "ran.txt"]);

        assert_eq!(ctx.viewed(&ran).as_deref(), Some("model\n"));
        assert!(ctx.viewed(&gone).is_none());
        let told = drift(&ctx).unwrap();
        assert!(told.starts_with("1 file changed"), "{told}");
        assert!(told.contains("theirs.txt"), "{told}");
    }

    // Changes far apart in one file read as the rows they moved; the span
    // between them is context, not a whole-block replacement.
    #[test]
    fn a_change_far_from_another_is_sketched_where_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let old: String = (1..=521).map(|n| format!("row {n}\n")).collect();
        let path = seen(dir.path(), &ctx, "wide.txt", &old);

        let before = before(&ctx);
        let now = old.replacen("row 57\n", "row 57 changed\n", 1).replacen(
            "row 371\n",
            "row 371 changed\n",
            1,
        );
        std::fs::write(&path, now).unwrap();
        let moved = absorb(&ctx, before);
        let report = report(&moved).unwrap();
        let text = render(&report.rows);
        let rows: Vec<&str> = text.lines().collect();

        assert_eq!(rows[0], "wide.txt +2 -2", "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("row 57 changed")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("row 371 changed")),
            "{rows:?}"
        );
        assert!(rows.len() < 20, "{} rows for 2 changes", rows.len());
    }

    #[test]
    fn a_file_grown_past_the_limit_is_told_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let log = dir.path().join("big.log");
        std::fs::write(&log, "small\n").unwrap();
        ctx.note_view(&log, "small\n");

        std::fs::File::create(&log)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        let told = drift(&ctx).unwrap();
        assert!(
            told.contains("big.log is") && told.contains("read limit"),
            "{told}"
        );
        assert!(ctx.viewed(&log).is_none(), "to be read again");
        assert_eq!(drift(&ctx), None);
    }
}
