//! Where pi keeps what it accumulates between runs, and the id sanitization
//! that names files inside it. Shared by the transcript store in `cli` and the
//! spill layer in `tools`, which both need the same answer without a cycle.

use std::path::PathBuf;

/// `$XDG_STATE_HOME/pi`, or `~/.local/state/pi` when the variable is unset.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|b| b.join("pi"))
}

/// An id as a file or directory name, with everything that could leave the
/// parent gone. Ids are minted as `{ts}-{pid}`, so this changes nothing for a
/// real one; it is the guard every consumer applies before an id opens a path.
pub fn file_stem(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::file_stem;

    #[test]
    fn a_real_id_is_its_own_stem() {
        assert_eq!(file_stem("1787426708-4135307"), "1787426708-4135307");
    }

    #[test]
    fn an_id_cannot_name_a_path_outside_its_directory() {
        assert_eq!(file_stem("../../etc/cron.d/x"), "______etc_cron_d_x");
        assert_eq!(file_stem(".."), "__");
        assert_eq!(file_stem(""), "unnamed");
    }
}
