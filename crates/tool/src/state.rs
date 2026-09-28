//! Where pi keeps what it accumulates between runs, and the id sanitization
//! that names files inside it. Shared by the transcript store in `pi` and the
//! spill layer in `tool`, which both need the same answer without a cycle.

use std::path::{Path, PathBuf};

/// The pi root: `$PI_HOME` when set, else `~/.pi`.
pub fn dir() -> Option<PathBuf> {
    std::env::var_os("PI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".pi")))
}

/// The journal inside each session's directory, named once for the store that
/// writes it and the sweep that looks for it by name.
pub const JOURNAL_FILE: &str = "journal.jsonl";

/// Write bytes where only this user can read them, whole or not at all.
///
/// Under a temp name and renamed, so the final path never carries the wrong
/// permissions: a crash between the write and the chmod would leave the
/// contents world-readable at the name everything else reads.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let pid = std::process::id();
    let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(had) => format!("{had}.{pid}.tmp"),
        None => format!("{pid}.tmp"),
    });
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
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
/// workspace: every character that is not a letter or a digit becomes `-`, so
/// `/home/u/pi-rs` is `-home-u-pi-rs`. Claude Code buckets its projects the
/// same way, and that fold is not injective here either — `/a/b` and `/a-b`
/// land in one bucket — which is accepted rather than solved: what a bucket
/// holds is read off the workspace each transcript records, never off its
/// name. Distinct from `file_stem` (which mints `_` for the same characters)
/// because the two name different things: a file a session owns, and the
/// bucket that groups them.
pub fn key_of(path: &Path) -> String {
    path.display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::{file_stem, key_of};
    use std::path::Path;

    // A path folded to one directory name, and the fold is deliberately not
    // injective: `/home/u/pi-rs` and `/home/u/pi/rs` share a bucket the way
    // they do under Claude Code's project buckets. Accepted, because the
    // transcripts inside carry the workspace each was recorded under and the
    // bucket name is never read as the answer.
    #[test]
    fn a_workspace_key_is_its_path_with_every_separator_dashed() {
        assert_eq!(key_of(Path::new("/home/dev/pi-rs")), "-home-dev-pi-rs");
        assert_eq!(key_of(Path::new("/")), "-");
        assert_eq!(key_of(Path::new(".")), "-");
        assert_eq!(
            key_of(Path::new("/home/u/pi-rs")),
            key_of(Path::new("/home/u/pi/rs"))
        );
    }

    // The stem is the id when it is well formed, and something inert when it
    // is not: an id can never name a path outside its directory.
    #[test]
    fn an_id_cannot_name_a_path_outside_its_directory() {
        assert_eq!(file_stem("1787426708-4135307"), "1787426708-4135307");
        assert_eq!(file_stem("../../etc/cron.d/x"), "______etc_cron_d_x");
        assert_eq!(file_stem(".."), "__");
        assert_eq!(file_stem(""), "unnamed");
    }
}
