//! The size past which no tool reads a file whole.

pub const MAX_BYTES: u64 = 10 << 20;

/// The one refusal every over-limit file shares, whatever tool meets it.
pub fn over_limit(name: &str, len: u64) -> String {
    format!("{name} is {len} bytes, over the {MAX_BYTES}-byte read limit; use bash to slice it")
}
