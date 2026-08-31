//! The one line that says the run is still alive and what it has spent.

use std::time::Duration;

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

/// What the line should show, as far as the provider has stated it.
///
/// An unstated half reads as a dash, never as a number of ours: most hosts
/// report nothing until the stream ends, and a count we made for the gap
/// would pass a guess off as a measurement.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub input: u64,
    pub output: u64,
}

/// `stopping` is its own state rather than an absence: a cancelled run keeps
/// going until the request it is inside of returns, and a status line that
/// stayed unchanged would read as the key press having been missed.
pub fn line(frame: usize, since: Duration, c: &Counts, queued: usize, stopping: bool) -> String {
    let mut parts = vec![elapsed(since), crate::render::in_out(c.input, c.output)];
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
    fn a_fresh_run_shows_dashes_for_what_the_provider_has_not_said() {
        let s = line(0, Duration::from_secs(3), &Counts::default(), 0, false);
        assert_eq!(s, "⠋ 3s · - in / - out · esc to stop");
    }

    #[test]
    fn an_unstated_half_reads_as_a_dash_until_it_arrives() {
        // The Anthropic wire states the input count up front and the output
        // count only at the end; the half that is known shows its number and
        // the one that has not said anything shows a dash.
        let c = Counts {
            input: 8_400,
            ..Default::default()
        };
        let s = line(0, Duration::from_secs(4), &c, 0, false);
        assert_eq!(s, "⠋ 4s · 8.4k in / - out · esc to stop");
    }

    #[test]
    fn both_stated_halves_read_as_a_pair() {
        let c = Counts {
            input: 8_400,
            output: 390,
        };
        let s = line(0, Duration::from_secs(4), &c, 0, false);
        assert_eq!(s, "⠋ 4s · 8.4k in / 390 out · esc to stop");
    }

    #[test]
    fn a_host_that_has_said_nothing_shows_dashes_everywhere() {
        // An OpenAI host reports no usage until the stream ends; the status
        // line must not fill the gap with a count of ours.
        let s = line(0, Duration::from_secs(9), &Counts::default(), 0, false);
        assert_eq!(s, "⠋ 9s · - in / - out · esc to stop");
    }

    #[test]
    fn a_long_run_reads_in_minutes() {
        let c = Counts {
            input: 12_000,
            output: 340,
        };
        let s = line(1, Duration::from_secs(125), &c, 2, false);
        assert_eq!(s, "⠙ 2m05s · 12.0k in / 340 out · 2 queued · esc to stop");
    }

    #[test]
    fn a_stopping_run_says_so_and_stops_spinning() {
        // The request it is inside of has to return first, and a line that went
        // on spinning would read as the key press having been missed.
        let s = line(4, Duration::from_secs(1), &Counts::default(), 0, true);
        assert_eq!(s, "· 1s · - in / - out · stopping…");
    }
}
