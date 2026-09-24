//! The disk, and the names the config speaks.
//!
//! Two things live here: the files a run reads and writes (`session.rs`,
//! `journal.rs`, `config.rs`, `settings.rs`), and the vocabulary a config is
//! written in — `theme.rs`, `keys.rs`, `status.rs`, `icons.rs`, `text.rs`. The
//! vocabulary is here rather than in `ui/` because it is what a setting is
//! *called*, not how it is drawn: `ui/` reads these names and decides what they
//! look like.

pub mod config;
pub mod icons;
pub mod journal;
pub mod keys;
pub mod listing;
pub mod session;
pub mod settings;
pub mod status;
pub mod text;
pub mod theme;
