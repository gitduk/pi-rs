//! Char-boundary string slicing shared by the tools that clip long output
//! (bash, read) and the compaction that prunes it again.

/// Largest prefix of `s` within `max` bytes that ends on a char boundary.
pub fn head_bytes(s: &str, max: usize) -> &str {
    match s.char_indices().find(|(i, c)| i + c.len_utf8() > max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Mirror of [`head_bytes`] from the end.
pub fn tail_bytes(s: &str, max: usize) -> &str {
    let start = s.len().saturating_sub(max);
    match s.char_indices().find(|(i, _)| *i >= start) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

/// Largest prefix of `s` within `max` chars that ends on a char boundary.
pub fn head_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Mirror of [`head_chars`] from the end. Fewer than `max` chars means the
/// whole string is the tail, matching [`tail_bytes`].
pub fn tail_chars(s: &str, max: usize) -> &str {
    let Some((i, _)) = s.char_indices().nth_back(max) else {
        return s;
    };
    &s[i..]
}
