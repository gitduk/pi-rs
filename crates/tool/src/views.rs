//! What a session's model has seen, kept beside its spills so a view outlives
//! the process that read the file: a resumed session is still told what moved
//! under what it read before.
//!
//! One file per path, named by a digest of the path, holding the path and the
//! text the view was built on. A view is what a `read` or an `edit` saw; a
//! command that changes the file is told against it, and a file deleted or
//! grown past the read limit has none.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{stamp, state};

/// The views one session keeps, in a directory of its own.
pub(super) struct Views {
    dir: PathBuf,
}

impl Views {
    pub(super) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Every view the session left: the text, and the path it was read under.
    /// A file that cannot be read back is one more view lost, never an error:
    /// the session reads on with what is there.
    pub(super) fn load(&self) -> HashMap<PathBuf, Arc<str>> {
        let mut views = HashMap::new();
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return views;
        };
        for file in dir.flatten() {
            let path = file.path();
            // A view being written is a `.tmp` beside the one it replaces.
            if path.extension().is_none_or(|ext| ext != "view") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some((head, content)) = text.split_once('\n') else {
                continue;
            };
            let Ok(named) = serde_json::from_str::<String>(head) else {
                continue;
            };
            views.insert(PathBuf::from(named), Arc::from(content));
        }
        views
    }

    /// What the view of `path` is now: written whole over the one it replaces,
    /// so the file never holds more than one copy of a file. Every failure is
    /// dropped, as on the way in: a view that cannot be kept is one more view
    /// the session reads on without.
    pub(super) fn note(&self, path: &Path, content: &str) {
        let Some(named) = path.to_str() else {
            return;
        };
        // The head names the path the text was read under; a JSON string never
        // holds a newline, so the first one ends it. A path is not a value
        // this can fail on, and a head `load` cannot read would be skipped
        // silently instead.
        let mut body = serde_json::to_vec(named).expect("a path as a JSON string");
        body.push(b'\n');
        body.extend_from_slice(content.as_bytes());
        if let Err(e) = state::write_private(&self.at(path), &body)
            && e.kind() == std::io::ErrorKind::NotFound
        {
            // The session has spilled nothing yet: the directory is not
            // there, and `note` is the only writer that would have made it.
            let _ = self.make();
            let _ = state::write_private(&self.at(path), &body);
        }
    }

    /// `path` has no view left to tell anything against.
    pub(super) fn forget(&self, path: &Path) {
        let _ = std::fs::remove_file(self.at(path));
    }

    // Make the directory the views sit in, private to this user: a view is
    // the text of a file the model read.
    fn make(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The session's directory above it as well: both hold the text of
            // files the model read, and neither is for another user's eyes.
            let private = std::fs::Permissions::from_mode(0o700);
            if let Some(session) = self.dir.parent() {
                std::fs::set_permissions(session, private.clone())?;
            }
            std::fs::set_permissions(&self.dir, private)?;
        }
        Ok(())
    }

    // Where one path's view sits: a digest of the path, so a long one is no
    // file name and the same path is always the same file.
    fn at(&self, path: &Path) -> PathBuf {
        let digest = stamp::fnv(path.as_os_str().as_encoded_bytes());
        self.dir.join(format!("{digest:016x}.view"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_comes_back_under_its_own_path() {
        let dir = tempfile::tempdir().unwrap();
        let views = Views::new(dir.path().to_path_buf());
        let one = Path::new("/w/one.rs");
        let odd = Path::new("/w/two \"quoted\"\nname.rs");
        views.note(one, "fn one() {}\n");
        views.note(odd, "odd\n");

        let back = Views::new(dir.path().to_path_buf()).load();
        assert_eq!(back.get(one).map(|v| v.as_ref()), Some("fn one() {}\n"));
        assert_eq!(back.get(odd).map(|v| v.as_ref()), Some("odd\n"));

        // A second view of one file leaves one copy of it, the newer.
        views.note(one, "fn one() { changed(); }\n");
        let back = Views::new(dir.path().to_path_buf()).load();
        assert_eq!(back.len(), 2);
        assert_eq!(
            back.get(one).map(|v| v.as_ref()),
            Some("fn one() { changed(); }\n")
        );

        views.forget(one);
        let back = Views::new(dir.path().to_path_buf()).load();
        assert!(!back.contains_key(one));
        assert!(back.contains_key(odd));
    }

    // What a process read, the session it read it in still knows: a resumed
    // run tells a file's change against the text its model saw, not against
    // nothing.
    #[test]
    fn a_view_survives_the_process_that_read_it() {
        let dir = tempfile::tempdir().unwrap();
        let spills = dir.path().join("spill");
        let workspace = crate::Workspace::new(dir.path()).unwrap();
        let file = dir.path().join("a.rs");

        let read = crate::Ctx::new(workspace.clone()).with_session("s-1", &spills);
        read.note_view(&file, "fn a() {}\n");
        // Another session of the same workspace saw nothing.
        let other = crate::Ctx::new(workspace.clone()).with_session("s-2", &spills);
        assert!(other.viewed(&file).is_none());

        // A new process picking the session back up reads the view again.
        let resumed = crate::Ctx::new(workspace.clone()).with_session("s-1", &spills);
        assert_eq!(resumed.viewed(&file).as_deref(), Some("fn a() {}\n"));

        // Settling it writes the view the command left, and forgetting drops it.
        let was = resumed.viewed(&file).unwrap();
        assert!(resumed.settle_view(&file, &was, Some("fn a() { changed(); }\n".into())));
        let again = crate::Ctx::new(workspace).with_session("s-1", &spills);
        assert_eq!(
            again.viewed(&file).as_deref(),
            Some("fn a() { changed(); }\n")
        );
        let was = again.viewed(&file).unwrap();
        assert!(again.settle_view(&file, &was, None));
        let gone = crate::Ctx::new(crate::Workspace::new(dir.path()).unwrap())
            .with_session("s-1", &spills);
        assert!(gone.viewed(&file).is_none());
    }
}
