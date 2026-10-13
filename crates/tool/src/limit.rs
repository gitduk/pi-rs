//! The limits every tool shares: how much it reads whole, how long it runs.

/// The size past which no tool reads a file whole.
pub const MAX_BYTES: u64 = 10 << 20;

/// The longest one call may run, whatever its caller asked for.
pub const MAX_CALL: std::time::Duration = std::time::Duration::from_secs(600);

/// The one refusal every over-limit file shares, whatever tool meets it.
pub fn over_limit(name: &str, len: u64) -> String {
    format!("{name} is {len} bytes, over the {MAX_BYTES}-byte read limit; use bash to slice it")
}
