//! The one line that says the run is still alive and what it has spent.

use std::time::Duration;

use brain::stream::Usage;

pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPIN: Duration = Duration::from_millis(90);

/// Thousands, one decimal. Exact counts are noise at this size and the line has
/// to hold still while they climb.
fn short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}m", n as f64 / 1_000_000.0),
    }
}

fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// `stopping` is its own state rather than an absence: a cancelled run keeps
/// going until the request it is inside of returns, and a status line that
/// stayed unchanged would read as the key press having been missed.
pub fn line(frame: usize, since: Duration, usage: &Usage, queued: usize, stopping: bool) -> String {
    let mut parts = vec![elapsed(since)];
    if usage.input + usage.output > 0 {
        parts.push(format!(
            "{} in / {} out",
            short(usage.input),
            short(usage.output)
        ));
    }
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    parts.push(if stopping {
        "stopping…".to_string()
    } else {
        "esc to stop".to_string()
    });
    let spin = if stopping {
        "·"
    } else {
        FRAMES[frame % FRAMES.len()]
    };
    format!("{spin} {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::{line, short};
    use brain::stream::Usage;
    use std::time::Duration;

    #[test]
    fn counts_are_rounded_once_they_stop_being_readable() {
        assert_eq!(short(999), "999");
        assert_eq!(short(1_240), "1.2k");
        assert_eq!(short(2_500_000), "2.5m");
    }

    #[test]
    fn a_fresh_run_shows_no_token_counts_it_does_not_have_yet() {
        let s = line(0, Duration::from_secs(3), &Usage::default(), 0, false);
        assert_eq!(s, "⠋ 3s · esc to stop");
    }

    #[test]
    fn a_long_run_reads_in_minutes() {
        let u = Usage {
            input: 12_000,
            output: 340,
            ..Default::default()
        };
        let s = line(1, Duration::from_secs(125), &u, 2, false);
        assert_eq!(s, "⠙ 2m05s · 12.0k in / 340 out · 2 queued · esc to stop");
    }

    #[test]
    fn a_stopping_run_says_so_and_stops_spinning() {
        // The request it is inside of has to return first, and a line that went
        // on spinning would read as the key press having been missed.
        let s = line(4, Duration::from_secs(1), &Usage::default(), 0, true);
        assert_eq!(s, "· 1s · stopping…");
    }
}
