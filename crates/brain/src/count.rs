//! Token counts, said the one way every surface says them.
//!
//! Here rather than in the CLI because the agent crate has a figure to show
//! too — a subagent's — and a count spelled `8400` beside one spelled `8.4k`
//! reads as a different unit.

/// Thousands, one decimal. Exact counts are noise at this size and the line has
/// to hold still while they climb.
pub fn short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}m", n as f64 / 1_000_000.0),
    }
}

/// A figure the provider reported, or the dash standing for one it left out —
/// never a count of ours standing in for it.
fn reported(n: u64) -> String {
    if n > 0 { short(n) } else { "-".to_string() }
}

/// The in/out counts in the one wording every line that shows them uses.
pub fn in_out(input: u64, output: u64) -> String {
    format!("{} in / {} out", reported(input), reported(output))
}

/// The same two figures where the words will not fit: a scrollback row that
/// has already said what it is, and needs the columns for what it did. Same
/// unit and same dash as `in_out` — a narrower spelling, not another count.
pub fn slash(input: u64, output: u64) -> String {
    format!("{}/{}", reported(input), reported(output))
}

#[cfg(test)]
mod tests {
    use super::{in_out, short, slash};

    #[test]
    fn counts_shorten_once_they_stop_being_worth_reading_exactly() {
        assert_eq!(short(999), "999");
        assert_eq!(short(8_400), "8.4k");
        assert_eq!(short(2_500_000), "2.5m");
    }

    #[test]
    fn a_figure_the_provider_left_out_reads_as_a_dash() {
        assert_eq!(in_out(8_400, 390), "8.4k in / 390 out");
        assert_eq!(in_out(8_400, 0), "8.4k in / - out");
        assert_eq!(in_out(0, 0), "- in / - out");
    }

    /// The narrow spelling drops the words and nothing else: the same
    /// shortening, and the same dash for what was never reported.
    #[test]
    fn the_narrow_spelling_keeps_the_unit_and_the_dash() {
        assert_eq!(slash(8_400, 390), "8.4k/390");
        assert_eq!(slash(8_400, 0), "8.4k/-");
        assert_eq!(slash(0, 0), "-/-");
    }
}
