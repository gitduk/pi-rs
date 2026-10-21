//! Figures a person is shown, said the one way everywhere.
//!
//! Down here so every crate that shows a count spells it alike: `8400` beside
//! `8.4k` reads as a different unit.

use std::time::Duration;

/// `thing` as many of it as `n` says: `1 file`, `2 files`, `3 matches`.
pub fn count(n: usize, thing: &str) -> String {
    format!("{n} {}", plural(n, thing))
}

/// `thing` in the number `n` asks for, for a count told apart from its noun.
pub fn plural(n: usize, thing: &str) -> String {
    let es = thing.ends_with(['s', 'x']) || thing.ends_with("ch") || thing.ends_with("sh");
    let ies = thing
        .strip_suffix('y')
        .filter(|stem| !stem.ends_with(['a', 'e', 'i', 'o', 'u']));
    match (n, ies) {
        (1, _) => thing.to_string(),
        (_, Some(stem)) => format!("{stem}ies"),
        _ if es => format!("{thing}es"),
        _ => format!("{thing}s"),
    }
}

/// How many of `n` items a list shows under `limit`: all of them when only
/// one is over, since the "N more" line would take that item's row.
pub fn shown(n: usize, limit: usize) -> usize {
    if n > limit + 1 { limit } else { n }
}

/// Thousands, one decimal. Exact counts are noise at this size and the line has
/// to hold still while they climb.
pub fn short(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_949 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}m", n as f64 / 1_000_000.0),
    }
}

/// A size that never climbs, such as a context window: `200k`, not `200.0k`.
/// The decimals `short` keeps are for counts that must hold still while rising.
pub fn size(n: u64) -> String {
    let s = short(n);
    let Some((whole, rest)) = s.split_once('.') else {
        return s;
    };
    let unit = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let decimals = &rest[..rest.len() - unit.len()];
    if decimals.chars().all(|c| c == '0') {
        format!("{whole}{unit}")
    } else {
        s
    }
}

/// Money spent: four places under a dollar, where a run's cost lives, two
/// once past it, where the cents beyond are noise.
pub fn dollars(d: f64) -> String {
    if d < 1.0 {
        format!("${d:.4}")
    } else {
        format!("${d:.2}")
    }
}

// A figure the provider reported, or the dash standing for one it left out —
// never a count of ours standing in for it.
fn reported(n: u64) -> String {
    if n > 0 { short(n) } else { "-".to_string() }
}

/// The in/out counts in the one wording every line that shows them uses.
pub fn in_out(input: u64, output: u64) -> String {
    format!("{} in / {} out", reported(input), reported(output))
}

/// The in/out counts while they are still arriving: a half not yet stated is
/// left off rather than dashed, so a repainting line does not read as broken.
pub fn in_out_so_far(input: u64, output: u64) -> String {
    match (input, output) {
        (_, 0) => format!("{} in", short(input)),
        (0, _) => format!("{} out", short(output)),
        _ => in_out(input, output),
    }
}

/// The same two figures where the words will not fit: a row that
/// has already said what it is, and needs the columns for what it did. Same
/// unit and same dash as `in_out` — a narrower spelling, not another count.
pub fn slash(input: u64, output: u64) -> String {
    format!("{}/{}", reported(input), reported(output))
}

/// How long something took, in the one wording every line that shows one uses:
/// seconds, and the minutes once there are any.
pub fn elapsed(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m{:02}s", s / 60, s % 60),
        3_600..86_400 => format!("{}h{:02}m", s / 3_600, s % 3_600 / 60),
        _ => format!("{}d{:02}h", s / 86_400, s % 86_400 / 3_600),
    }
}
