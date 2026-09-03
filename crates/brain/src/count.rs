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

/// The in/out counts in the one wording every line that shows them uses: a
/// figure the provider reported is shown as-is, one it left out reads as a
/// dash, never as a count of ours standing in for it.
pub fn in_out(input: u64, output: u64) -> String {
    let input = if input > 0 {
        short(input)
    } else {
        "-".to_string()
    };
    let output = if output > 0 {
        short(output)
    } else {
        "-".to_string()
    };
    format!("{input} in / {output} out")
}

#[cfg(test)]
mod tests {
    use super::{in_out, short};

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
}
