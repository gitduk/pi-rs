//! The limits every tool shares: how much it reads whole, how long it runs.

use std::time::Duration;

/// The size past which no tool reads a file whole.
pub const MAX_BYTES: u64 = 10 << 20;

/// The longest one call may run, whatever its caller asked for.
pub const MAX_CALL: Duration = Duration::from_secs(600);

/// The one refusal every over-limit file shares, whatever tool meets it.
pub fn over_limit(name: &str, len: u64) -> String {
    format!("{name} is {len} bytes, over the {MAX_BYTES}-byte read limit; use bash to slice it")
}

/// How long a call that asked for `ms` may run: `default` when it named none,
/// or zero, which would end it before it began; never past `max`.
pub fn timeout(ms: Option<u64>, default: Duration, max: Duration) -> Duration {
    ms.filter(|ms| *ms > 0)
        .map_or(default, Duration::from_millis)
        .min(max)
}
