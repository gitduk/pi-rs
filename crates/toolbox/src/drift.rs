//! What changed on disk since the model last saw it, told before it next
//! speaks rather than when an edit trips over it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use tool::Ctx;
use tool::limit::{MAX_BYTES, over_limit};

use crate::edit::{CHANGED_RUN, rewrite_sketch};

/// The diff of each file from its last view to disk, or `None`; each change
/// is told once, the view moving to what is on disk now. A file deleted, or
/// grown past the read limit, is told and forgotten: it is to be read again.
pub fn drift(ctx: &Ctx) -> Option<String> {
    let mut told = Vec::new();
    for (path, was) in seen(ctx) {
        let rel = ctx.workspace.display(&path);
        match on_disk(&path) {
            Ok(Disk::Text(now)) if *was == *now => {}
            Ok(Disk::Text(now)) => {
                told.push(rewrite_sketch(&rel, &was, &now, CHANGED_RUN));
                settle(ctx, &path, &was, Some(now.into()));
            }
            Ok(Disk::Over(len)) => {
                told.push(over_limit(&rel, len));
                settle(ctx, &path, &was, None);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                told.push(format!("{rel} deleted"));
                settle(ctx, &path, &was, None);
            }
            Err(_) => {}
        }
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
/// the command ran is left for `drift`.
pub fn absorb(ctx: &Ctx, before: Before) {
    for (path, was, at) in before.0 {
        if modified(&path) == at {
            continue;
        }
        let now = match on_disk(&path) {
            Ok(Disk::Text(now)) => Some(now.into()),
            Ok(Disk::Over(_)) => None,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        tracing::info!(target: "pi::drift", path = %ctx.workspace.display(&path), "absorbed");
        settle(ctx, &path, &was, now);
    }
}

// A copy of the views, so the reads run without holding the lock every view
// takes; in path order, so what is told reads the same each time.
fn seen(ctx: &Ctx) -> Vec<(PathBuf, Arc<str>)> {
    let mut seen: Vec<_> = (ctx.viewed.lock().expect("viewed poisoned").iter())
        .map(|(path, was)| (path.clone(), was.clone()))
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    seen
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

enum Disk {
    Text(String),
    Over(u64),
}

// Never more than the read limit in memory, however large the file grew.
fn on_disk(path: &Path) -> std::io::Result<Disk> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len > MAX_BYTES {
        return Ok(Disk::Over(len));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Ok(Disk::Over(bytes.len() as u64));
    }
    Ok(Disk::Text(String::from_utf8_lossy(&bytes).into_owned()))
}

// Moves the view only if no tool noted a newer one while the file was read.
fn settle(ctx: &Ctx, path: &Path, was: &Arc<str>, now: Option<Arc<str>>) {
    let mut viewed = ctx.viewed.lock().expect("viewed poisoned");
    if !viewed.get(path).is_some_and(|cur| Arc::ptr_eq(cur, was)) {
        return;
    }
    match now {
        Some(now) => viewed.insert(path.to_path_buf(), now),
        None => viewed.remove(path),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_each_change_once_and_not_the_models_own() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
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
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let (ran, theirs, gone) = (
            dir.path().join("ran.txt"),
            dir.path().join("theirs.txt"),
            dir.path().join("gone.txt"),
        );
        // Backdated, so the command's write moves mtime even on a coarse clock.
        let past = SystemTime::now() - std::time::Duration::from_secs(10);
        for p in [&ran, &theirs, &gone] {
            std::fs::write(p, "old\n").unwrap();
            (std::fs::File::options().write(true).open(p).unwrap())
                .set_modified(past)
                .unwrap();
            ctx.note_view(p, "old\n");
        }
        std::fs::write(&theirs, "user\n").unwrap();

        let before = before(&ctx);
        std::fs::write(&ran, "model\n").unwrap();
        std::fs::remove_file(&gone).unwrap();
        absorb(&ctx, before);

        assert_eq!(ctx.viewed(&ran).as_deref(), Some("model\n"));
        assert!(ctx.viewed(&gone).is_none());
        let told = drift(&ctx).unwrap();
        assert!(told.starts_with("1 file changed"), "{told}");
        assert!(told.contains("theirs.txt"), "{told}");
    }

    #[test]
    fn a_file_grown_past_the_limit_is_told_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(tool::Workspace::new(dir.path()).unwrap());
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
