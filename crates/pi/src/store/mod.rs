//! The disk, and the names the config speaks.
//!
//! Two things live here: the files a run reads and writes (`session.rs`,
//! `archive.rs`, `journal.rs`, `config.rs`, `settings.rs`), and the vocabulary a
//! config is written in — `theme.rs`, `keys/`, `status.rs`, `icons.rs`. The
//! vocabulary is here rather than in `ui/` because it is what a setting is
//! *called*, not how it is drawn: `ui/` reads these names and decides what they
//! look like.

use std::path::PathBuf;

pub mod archive;
pub mod config;
pub mod icons;
pub mod journal;
pub mod keys;
pub mod listing;
pub mod session;
pub mod settings;
pub mod status;
pub mod theme;

/// The pi root: `$PI_HOME` when set, else `~/.pi`.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("PI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
}

/// Where every session's spills go, a subagent's beside its caller's so a
/// locator either prints resolves from the other. Without a pi root the
/// process temp directory stands in.
pub fn spill_root() -> PathBuf {
    dir()
        .unwrap_or_else(|| std::env::temp_dir().join("pi-spill"))
        .join("spill")
}
