//! What changed on disk since the model last saw it, told before it next
//! speaks rather than when an edit trips over it.

use tool::Ctx;

use crate::edit::{CHANGED_RUN, rewrite_sketch};

/// The diff of each file from its last view to disk, or `None`; each change
/// is told once, the view moving to what is on disk now.
pub fn drift(ctx: &Ctx) -> Option<String> {
    let mut viewed = ctx.viewed.lock().expect("viewed poisoned");
    let mut paths: Vec<_> = viewed.keys().cloned().collect();
    paths.sort();
    let mut told = Vec::new();
    for path in paths {
        let rel = ctx.workspace.display(&path);
        let now = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                viewed.remove(&path);
                told.push(format!("{rel} deleted"));
                continue;
            }
            Err(_) => continue,
        };
        let Some(was) = viewed.get(&path) else {
            continue;
        };
        if **was == *now {
            continue;
        }
        told.push(rewrite_sketch(&rel, was, &now, CHANGED_RUN));
        viewed.insert(path, now.into());
    }
    (!told.is_empty()).then(|| format!("changed since you last saw them:\n{}", told.join("\n")))
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
}
