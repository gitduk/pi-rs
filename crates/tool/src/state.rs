//! Writing files kept between runs: privately, whole, under a name an id
//! cannot escape. Where they live is the host's to say, not this crate's.

use std::path::Path;

/// Write bytes where only this user can read them, whole or not at all.
///
/// Create a private temporary file before writing, then replace the destination.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // The pid keeps a resumed run's fresh counter from landing on the earlier
    // process's temp file; the count keeps two writers of one path in this
    // process — a batch of tool calls runs at once — from landing on each
    // other's.
    static WRITTEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nth = WRITTEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let tmp = path.with_extension(match path.extension().and_then(|e| e.to_str()) {
        Some(had) => format!("{had}.{pid}.{nth}.tmp"),
        None => format!("{pid}.{nth}.tmp"),
    });
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let written = std::io::Write::write_all(&mut file, bytes);
    drop(file);
    let replaced = written.and_then(|()| std::fs::rename(&tmp, path));
    if replaced.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    replaced
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
    fn an_id_cannot_name_a_path_outside_its_directory() {
        assert_eq!(file_stem("1787426708-4135307"), "1787426708-4135307");
        assert_eq!(file_stem("../../etc/cron.d/x"), "______etc_cron_d_x");
        assert_eq!(file_stem(".."), "__");
        assert_eq!(file_stem(""), "unnamed");
    }
}
