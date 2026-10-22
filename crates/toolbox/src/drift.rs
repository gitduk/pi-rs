//! What changed on disk since the model last saw it, told before it next
//! speaks rather than when an edit trips over it.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use tool::Ctx;
use tool::limit::{MAX_BYTES, over_limit};

use crate::edit::{CHANGED_RUN, rewrite_sketch};

/// The diff of each file from its last view to disk, or `None`; each change
/// is told once, the view moving to what is on disk now. A file deleted, or
/// grown past the read limit, is told and forgotten: it is to be read again.
pub fn drift(ctx: &Ctx) -> Option<String> {
    // A copy, so the reads run without holding the lock every view takes.
    let mut seen: Vec<(_, Arc<str>)> = (ctx.viewed.lock().expect("viewed poisoned").iter())
        .map(|(path, was)| (path.clone(), was.clone()))
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    let mut told = Vec::new();
    for (path, was) in seen {
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
    (!told.is_empty()).then(|| format!("changed since you last saw them:\n{}", told.join("\n")))
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
        assert!(told.contains("a.txt +1 -1"), "{told}");
        assert!(told.contains("-2 two") && told.contains("+2 2"), "{told}");
        assert!(told.contains("b.txt deleted"), "{told}");
        assert_eq!(drift(&ctx), None);
        assert!(ctx.viewed(&b).is_none());
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
