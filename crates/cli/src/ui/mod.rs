//! Everything that faces the terminal: the mode that reads a line, the
//! drawing helpers, and the full-screen surface.
//!
//! This is the top of the tree: it reads `app` and `store`, and nothing here is
//! read back.

pub mod line;
pub mod render;
pub mod status;
pub mod tui;
