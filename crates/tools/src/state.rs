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

/// Where transcripts and their journals are kept, one directory per session.
pub fn sessions() -> Option<PathBuf> {
    dir().map(|d| d.join("sessions"))
}

/// One session's own directory. Named here rather than in `cli`, which is what
/// writes it: the agent points the model at the journal inside when a tool
/// keeps failing, and neither side may guess at the other's layout.
pub fn session_dir(workspace: &Path, id: &str) -> Option<PathBuf> {
    sessions().map(|s| s.join(key_of(workspace)).join(file_stem(id)))
}

/// The tree journals used to have to themselves. Kept only so that the run
/// that finds one can take it: nothing writes here any more.
pub fn stale_logs() -> Option<PathBuf> {
    dir().map(|d| d.join("logs"))
}

/// Write bytes where only this user can read them, whole or not at all.
///
/// Under a temp name and renamed, so the final path never carries the wrong
/// permissions: a crash between the write and the chmod would leave the
/// contents world-readable at the name everything else reads.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(had) => format!("{had}.tmp"),
        None => "tmp".into(),
    });
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
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
            key_of(Path::new("/home/dev/pi-rs")),
            "-home-dev-pi-rs"
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
