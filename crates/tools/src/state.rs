//! Where pi keeps what it accumulates between runs, and the id sanitization
//! that names files inside it. Shared by the transcript store in `cli` and the
//! spill layer in `tools`, which both need the same answer without a cycle.

use std::path::{Path, PathBuf};

/// The pi root: `$PI_HOME` when set, else `~/.pi`.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("PI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
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

/// A path as a single directory name, for grouping a machine's state by
/// workspace. The same shape Claude Code uses for its project buckets:
/// `/` and every other character a directory name may not take become `-`,
/// so `/home/u/pi-rs` is `-home-u-pi-rs`. Distinct from `file_stem` (which
/// mints `_` for the same characters) because the two name different things:
/// a file a session owns, and the bucket that groups them.
pub fn key_of(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{dir, file_stem, key_of};
    use std::path::Path;

    #[test]
    fn a_workspace_key_is_its_slash_path_with_separators_dashed() {
        assert_eq!(
            key_of(Path::new("/home/wukaige/pi-rs")),
            "-home-wukaige-pi-rs"
        );
        assert_eq!(key_of(Path::new("/")), "-");
        assert_eq!(key_of(Path::new(".")), "-");
    }

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

    #[test]
    fn pi_home_replaces_the_default_root() {
        let prior = std::env::var_os("PI_HOME");
        unsafe {
            std::env::set_var("PI_HOME", "/srv/pi");
        }
        assert_eq!(
            dir().map(|d| d.display().to_string()),
            Some("/srv/pi".into())
        );
        match prior {
            Some(v) => unsafe { std::env::set_var("PI_HOME", v) },
            None => unsafe { std::env::remove_var("PI_HOME") },
        }
    }
}
