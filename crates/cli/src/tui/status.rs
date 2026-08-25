//! The one line that says the run is still alive and what it has spent.

use std::time::Duration;

use crate::render::short;

pub const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPIN: Duration = Duration::from_millis(90);

fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// What the line should show, and which half the provider actually stated.
///
/// A half is inexact while it is pi's own count: most hosts report nothing
/// until the stream ends, and a counter frozen at zero for a minute reads as a
/// stall. Marked per half, because a host that states one and not the other is
/// the ordinary case.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub input: u64,
    pub output: u64,
    pub input_exact: bool,
    pub output_exact: bool,
}

fn counts(c: &Counts) -> Option<String> {
    let im = if c.input_exact { "" } else { "~" };
    let om = if c.output_exact { "" } else { "~" };
    let input = (c.input > 0).then(|| format!("{im}{} in", short(c.input)));
    let output = (c.output > 0).then(|| format!("{om}{} out", short(c.output)));
    match (input, output) {
        (Some(i), Some(o)) => Some(format!("{i} / {o}")),
        (some, None) | (None, some) => some,
    }
}

/// `stopping` is its own state rather than an absence: a cancelled run keeps
/// going until the request it is inside of returns, and a status line that
/// stayed unchanged would read as the key press having been missed.
pub fn line(frame: usize, since: Duration, c: &Counts, queued: usize, stopping: bool) -> String {
    let mut parts = vec![elapsed(since)];
    parts.extend(counts(c));
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
    use super::{Counts, line};
    use crate::render::short;
    use std::time::Duration;

    #[test]
    fn counts_are_rounded_once_they_stop_being_readable() {
        assert_eq!(short(999), "999");
        assert_eq!(short(1_240), "1.2k");
        assert_eq!(short(2_500_000), "2.5m");
    }

    #[test]
    fn a_fresh_run_shows_no_token_counts_it_does_not_have_yet() {
        let s = line(0, Duration::from_secs(3), &Counts::default(), 0, false);
        assert_eq!(s, "⠋ 3s · esc to stop");
    }

    #[test]
    fn what_the_provider_has_not_said_yet_is_marked_as_a_guess() {
        // The Anthropic wire states the input count up front and the output
        // count only at the end; the half that is known must not wait for the
        // half that is not.
        let c = Counts {
            input: 8_400,
            output: 512,
            input_exact: true,
            output_exact: false,
        };
        let s = line(0, Duration::from_secs(4), &c, 0, false);
        assert_eq!(s, "⠋ 4s · 8.4k in / ~512 out · esc to stop");
    }

    #[test]
    fn a_measured_count_replaces_the_guess_rather_than_joining_it() {
        let c = Counts {
            input: 8_400,
            output: 390,
            input_exact: true,
            output_exact: true,
        };
        let s = line(0, Duration::from_secs(4), &c, 0, false);
        assert_eq!(s, "⠋ 4s · 8.4k in / 390 out · esc to stop");
    }

    #[test]
    fn an_openai_host_that_says_nothing_still_shows_the_output_climbing() {
        let c = Counts {
            output: 1_500,
            ..Default::default()
        };
        let s = line(0, Duration::from_secs(9), &c, 0, false);
        assert_eq!(s, "⠋ 9s · ~1.5k out · esc to stop");
    }

    #[test]
    fn a_long_run_reads_in_minutes() {
        let c = Counts {
            input: 12_000,
            output: 340,
            input_exact: true,
            output_exact: true,
        };
        let s = line(1, Duration::from_secs(125), &c, 2, false);
        assert_eq!(s, "⠙ 2m05s · 12.0k in / 340 out · 2 queued · esc to stop");
    }

    #[test]
    fn a_stopping_run_says_so_and_stops_spinning() {
        // The request it is inside of has to return first, and a line that went
        // on spinning would read as the key press having been missed.
        let s = line(4, Duration::from_secs(1), &Counts::default(), 0, true);
        assert_eq!(s, "· 1s · stopping…");
    }
    #[test]
    fn only_the_guessed_half_wears_the_tilde() {
        // The provider stated the input; borrowing the output's doubt for it
        // claims less than is known.
        let c = Counts {
            input: 2_814,
            output: 92,
            input_exact: true,
            output_exact: false,
        };
        assert_eq!(super::counts(&c).unwrap(), "2.8k in / ~92 out");
    }
}
