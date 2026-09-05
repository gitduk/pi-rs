//! The runtime spill layer: an over-long tool output goes to a file the model
//! can read back by locator, never into the transcript whole. The model never
//! decides when this happens — the size threshold does.

use std::path::{Path, PathBuf};

use brain::slice::{head_bytes, tail_bytes};

use crate::{Ctx, ToolError, state};

/// Outputs over this many bytes leave only a head, a tail and a locator in the
/// transcript.
pub const MAX_OUTPUT: usize = 30_000;

/// What a spilled output leaves in the transcript: an opaque handle the model
/// hands back to `read` verbatim, and how big the whole thing was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillRef {
    pub locator: String,
    pub bytes: usize,
}

impl SpillRef {
    /// One line for the transcript: the locator, its size, and how to get it
    /// back. The hint is a fixed template over the locator, so it is rendered
    /// rather than stored.
    pub fn note(&self) -> String {
        format!(
            "full output: {} ({} bytes; read it back with `read {}`)",
            self.locator, self.bytes, self.locator
        )
    }
}

/// The root every session shares: the pi state directory's `spill` tree.
/// Spills land under it in each session's own directory — parent and child
/// reach the same root, so a locator minted inside a subagent's run resolves
/// from its parent's. Without a state directory (no `$PI_HOME`/`$HOME`) the
/// process temp directory stands in.
pub fn shared() -> PathBuf {
    state::dir()
        .unwrap_or_else(|| std::env::temp_dir().join("pi-spill"))
        .join("spill")
}

/// Where spills live when no session is in force — tests and embedders never
/// touch the user's state.
pub fn temp() -> PathBuf {
    std::env::temp_dir().join("pi-spill")
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
pub fn write(ctx: &Ctx, body: &str) -> Result<Option<SpillRef>, ToolError> {
    if body.len() <= MAX_OUTPUT {
        return Ok(None);
    }
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The pid in the name is what keeps a resumed session's fresh counter from
    // overwriting the files the earlier process spilled.
    let name = format!("{}-{n}", std::process::id());
    let dir = ctx.spill_root.join(ctx.spill_namespace());
    let path = dir.join(format!("{name}.log"));
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError::Spill(format!("{}: {e}", dir.display())))?;
    // Write under a temp name and rename, so the final path never carries the
    // wrong permissions: a crash between write and chmod would leave the
    // file's contents world-readable beside a 0600 transcript.
    let tmp = path.with_extension("log.tmp");
    std::fs::write(&tmp, body).map_err(|e| ToolError::Spill(format!("{}: {e}", tmp.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)
        .map_err(|e| ToolError::Spill(format!("{}: {e}", path.display())))?;
    Ok(Some(SpillRef {
        bytes: body.len(),
        locator: format!("spill:{}/{}", ctx.spill_namespace(), name),
    }))
}

/// Keep both ends of an over-long body: the head says what it was about, the
/// tail how it ended. Under the threshold the body comes back untouched.
pub fn prune(body: &str) -> String {
    if body.len() <= MAX_OUTPUT {
        return body.to_string();
    }
    let half = MAX_OUTPUT / 2;
    let (h, t) = (head_bytes(body, half), tail_bytes(body, half));
    let dropped = body.len() - h.len() - t.len();
    format!("{h}\n… {dropped} bytes omitted …\n{t}")
}

/// A body assembled from items that must not be split, held to the transcript's
/// budget with the whole of it spilled for recall.
///
/// Whole items rather than bytes, which is what separates this from `prune`: a
/// grep section cut loose from its `[path#TAG]` header carries no tag, and a
/// row cut mid-line still parses as an address an edit would then patch. Items
/// go from the tail, since a view whose items are independent has no end worth
/// keeping the way a file's has.
///
/// The count is said in `unit`s because the caller's own `notice` cannot know
/// it: that one reports what the search's limit dropped, and a body reading
/// "3 more matches" beside a locator holding four hundred is worse than silence.
pub fn fit(ctx: &Ctx, items: &[String], unit: &str, notice: &str) -> Result<String, ToolError> {
    let mut full = items.concat();
    full.push_str(notice);
    let Some(spilled) = write(ctx, &full)? else {
        return Ok(full);
    };
    // The note's length bounds the items, but its count needs them counted first.
    // Priced at its longest — every item gone — so the real one comes in under.
    let found = spilled.note();
    let say = |dropped: usize| {
        format!(
            "… {dropped} of {} {unit} did not fit the window\n{found}\n",
            items.len()
        )
    };
    let room = MAX_OUTPUT.saturating_sub(say(items.len()).len() + notice.len());
    let kept = crate::rows::fits(items.iter(), |i: &&String| i.len(), room);
    // What is kept is already the front of `full`, and every item boundary is a
    // character boundary: slice it rather than build the same bytes twice.
    let head: usize = items[..kept].iter().map(String::len).sum();
    Ok(format!("{}{notice}{}", &full[..head], say(items.len() - kept)))
}

/// The `<label>`-wrapped form bash uses for stdout and stderr. Under the
/// threshold the body is interpolated directly, so a normal output costs one
/// allocation, not a copy through `prune` first.
pub fn clamp(label: &str, body: &str) -> String {
    if body.is_empty() {
        return String::new();
    }
    if body.len() <= MAX_OUTPUT {
        return format!("<{label}>\n{body}\n</{label}>\n");
    }
    format!("<{label}>\n{}\n</{label}>\n", prune(body))
}

#[cfg(test)]
mod tests {
    use super::{locate, prune};
    use crate::ToolError;

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

    #[test]
    fn a_locator_written_by_one_session_reads_in_another() {
        use crate::{Ctx, Workspace};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("spill");
        let ws = Workspace::new(dir.path()).unwrap();
        // Parent and child reach the same root; the session only picks the
        // directory the spill is filed under.
        let child = Ctx::new(ws.clone())
            .with_session("p-1-task-0")
            .with_spill_root(root.clone());
        let parent = Ctx::new(ws).with_session("p-1").with_spill_root(root);

        let big = "x".repeat(super::MAX_OUTPUT + 1);
        let spilled = super::write(&child, &big).unwrap().expect("over the cap");
        assert_eq!(
            std::fs::read_to_string(parent.spill_path(&spilled.locator).unwrap()).unwrap(),
            big
        );
    }
    #[test]
    fn prune_leaves_a_small_body_untouched() {
        let body = "small";
        assert_eq!(prune(body), body);
    }

    #[test]
    fn prune_keeps_both_ends_and_names_what_it_took() {
        let body = "head-line\n".repeat(10_000);
        let got = prune(&body);
        assert!(got.ends_with("head-line\n"), "{got}");

        assert!(got.contains("bytes omitted"), "{got}");
        assert!(got.len() < body.len(), "prune must shrink");
    }
}
