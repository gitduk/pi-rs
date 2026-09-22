//! 磁盘，以及配置所说的那些名字。
//!
//! 这里住两样东西：一次运行读写的文件（`session.rs`、`journal.rs`、
//! `config.rs`、`settings.rs`），和配置用的词汇——`theme.rs`、`keys.rs`、
//! `status.rs`、`icons.rs`、`text.rs`。词汇放在这一层而不是 `ui/`，是因为它
//! 们是设置被*称作*什么，不是被画成什么样：`ui/` 读这些名字，再决定长什么样。

pub mod config;
pub mod icons;
pub mod journal;
pub mod keys;
pub mod session;
pub mod settings;
pub mod status;
pub mod text;
pub mod theme;
