//! Writing files kept between runs: privately, whole, under a name an id
//! cannot escape. Where they live is the host's to say, not this crate's.

use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::file_stem;

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
