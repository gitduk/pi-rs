//! The runtime spill layer: an over-long tool output goes to a file the model
//! can read back by locator, never into the transcript whole. The model never
//! decides when this happens — the size threshold does.

use std::path::{Path, PathBuf};

use brain::slice::{head_bytes, tail_bytes};

use crate::{Ctx, ToolError};

/// Outputs over this many bytes leave only a head, a tail and a locator in the
/// transcript.
pub const MAX_OUTPUT: usize = 30_000;

/// What a spilled output leaves in the transcript: an opaque handle the model
/// hands back to `read` verbatim, how big the whole thing was, and how to get
/// it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillRef {
    pub locator: String,
    pub bytes: usize,
    pub retrieval_hint: String,
}

impl SpillRef {
    /// One line for the transcript: the locator, its size, and the hint.
    pub fn note(&self) -> String {
        format!(
            "full output: {} ({} bytes; {})",
            self.locator, self.bytes, self.retrieval_hint
        )
    }
}

/// A session id as a directory name. Ids are minted here as `{ts}-{pid}`, so
/// this changes nothing for a real one; it is the same guard `file_stem`
/// applies before a stored id opens a file.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
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
        "default".into()
    } else {
        cleaned
    }
}

/// Where spills live when the caller named no session: the process temp
/// directory, so tests and embedders never touch the user's state.
pub fn default_root(session: Option<&str>) -> PathBuf {
    match session {
        Some(ns) => state_dir().join("spill").join(ns),
        None => std::env::temp_dir().join("pi-spill"),
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|b| b.join("pi"))
        .unwrap_or_else(|| std::env::temp_dir().join("pi-state"))
}

/// Resolve an opaque `spill:<ns>/<n>` locator to the file it names. Both parts
/// are validated rather than sanitized: a locator the model typed is not one
/// our own writer minted, and a path that sneaks `..` in would escape the
/// spill root.
pub fn locate(root: &Path, locator: &str) -> Result<PathBuf, ToolError> {
    let rest = locator
        .strip_prefix("spill:")
        .filter(|r| !r.is_empty())
        .ok_or_else(|| ToolError::Invalid(format!("not a spill locator: `{locator}`")))?;
    let (ns, tail) = rest
        .split_once('/')
        .ok_or_else(|| ToolError::Invalid(format!("not a spill locator: `{locator}`")))?;
    let valid = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    if !valid(ns) || !valid(tail) {
        return Err(ToolError::Invalid(format!(
            "not a spill locator: `{locator}`"
        )));
    }
    let path = root.join(ns).join(format!("{tail}.log"));
    if !path.starts_with(root) {
        return Err(ToolError::Invalid(format!(
            "spill locator escapes its root: `{locator}`"
        )));
    }
    Ok(path)
}

/// Persist `body` under the session's spill directory, or say there was
/// nothing worth keeping. Storage failure is a loud error, never a silent
/// fallback: a locator the model cannot read back is worse than no spill.
pub fn write(ctx: &Ctx, ns: &str, body: &str) -> Result<Option<SpillRef>, ToolError> {
    if body.len() <= MAX_OUTPUT {
        return Ok(None);
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The pid in the name is what keeps a resumed session's fresh counter from
    // overwriting the files the earlier process spilled.
    let name = format!("{}-{n}", std::process::id());
    let dir = ctx.spill_root.join(ns);
    let path = dir.join(format!("{name}.log"));
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError::Spill(format!("{}: {e}", dir.display())))?;
    std::fs::write(&path, body)
        .map_err(|e| ToolError::Spill(format!("{}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Spilled outputs carry file contents; keep them as private as the
        // transcript that points at them.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let locator = format!("spill:{ns}/{name}");
    Ok(Some(SpillRef {
        bytes: body.len(),
        retrieval_hint: format!("read it back with `read {locator}`"),
        locator,
    }))
}

/// Keep both ends of an over-long body: the head says what it was about, the
/// tail how it ended. Under the threshold the body comes back untouched.
pub fn prune(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let half = max / 2;
    let (h, t) = (head_bytes(body, half), tail_bytes(body, half));
    let dropped = body.len() - h.len() - t.len();
    format!("{h}\n… {dropped} bytes elided …\n{t}")
}

/// The `<label>`-wrapped form bash uses for stdout and stderr.
pub fn clamp(label: &str, body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    format!("<{label}>\n{}\n</{label}>\n", prune(body, MAX_OUTPUT))
}

#[cfg(test)]
mod tests {
    use super::{locate, sanitize};
    use crate::ToolError;

    #[test]
    fn a_wild_session_id_stays_inside_the_spill_root() {
        assert_eq!(sanitize(".."), "__");

        assert_eq!(sanitize(""), "default");
    }

    #[test]
    fn a_locator_resolves_inside_its_root() {
        let root = std::path::Path::new("/state/spill");
        assert_eq!(
            locate(root, "spill:1787426708-4135307/123-0").unwrap(),
            root.join("1787426708-4135307").join("123-0.log")
        );
    }

    #[test]
    fn a_locator_cannot_walk_out_of_its_root() {
        let root = std::path::Path::new("/state/spill");
        for bad in [
            "spill:",
            "spill:abc",
            "spill:abc/",
            "spill:a/b/../../etc",
            "spill:../other/1-0",
            "spill:abc/1-0/../2-0",
            "spill:abc/1 0",
        ] {
            assert!(
                matches!(locate(root, bad), Err(ToolError::Invalid(_))),
                "{bad} must be refused"
            );
        }
    }
}
