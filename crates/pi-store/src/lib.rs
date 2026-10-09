//! The disk, and the names the config speaks.
//!
//! Two things live here: the files a run reads/writes (`session`, `archive`,
//! `journal`, `config`, `settings`, `memory`) and the config vocabulary (`theme`, `bar`,
//! `keys`, `status`, `icons`, `args`, `text`, `listing`) — the latter is here,
//! not in `ui`, because it's what a setting is *called*, not drawn.

use std::path::PathBuf;

pub mod args;
pub mod bar;
pub mod config;
pub mod icons;
pub mod journal;
pub mod keys;
pub mod listing;
pub mod memory;
pub mod session;
pub mod settings;
pub mod status;
pub mod text;
pub mod theme;

/// The pi root: `$PI_HOME` when set, else `~/.pi`.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("PI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
}

/// Whether `path` was last written more than `keep` ago. Unreadable reads as
/// recent: nothing is deleted on a guess.
pub fn older_than(path: &std::path::Path, keep: std::time::Duration) -> bool {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > keep)
}

/// Where images pasted with ctrl+v are saved.
pub fn images_dir() -> Option<PathBuf> {
    dir().map(|d| d.join("images"))
}

/// Deletes the pasted images not pasted again for `keep`.
pub fn forget_images(keep: std::time::Duration) {
    let Some(images) = images_dir() else {
        return;
    };
    let Ok(files) = std::fs::read_dir(images) else {
        return;
    };
    for file in files.flatten() {
        if older_than(&file.path(), keep) {
            let _ = std::fs::remove_file(file.path());
        }
    }
}

/// Where every session's spills go, a subagent's beside its caller's so a
/// locator either prints resolves from the other. Without a pi root the
/// process temp directory stands in.
pub fn spill_root() -> PathBuf {
    dir()
        .unwrap_or_else(|| std::env::temp_dir().join("pi-spill"))
        .join("spill")
}

/// A file under the pi root that only this user may read: what it holds now,
/// and a writer that replaces it whole. No pi root, nothing read or written.
pub fn private_file(name: &str) -> (Option<Vec<u8>>, impl Fn(&[u8]) + Send + Sync + 'static) {
    let path = dir().map(|d| d.join(name));
    let saved = path.as_deref().and_then(|p| std::fs::read(p).ok());
    let write = move |body: &[u8]| {
        let Some(path) = &path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = tool::state::write_private(path, body);
    };
    (saved, write)
}
