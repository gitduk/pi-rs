//! Everything that faces the terminal: the drawing helpers, what the terminal
//! says about itself, and the full-screen surface.
//!
//! This is the top of the tree: it reads `core` and `store`, and nothing here is
//! read back.

pub mod listing;
pub mod render;
pub mod sgr;
pub mod status;
pub mod tty;
pub mod tui;
