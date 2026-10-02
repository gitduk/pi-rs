//! The runtime spill layer: an over-long tool output goes to a file the model
//! can read back by locator, never into the transcript whole. The model never
//! decides when this happens — the size threshold does.

use std::path::{Path, PathBuf};

use llm::slice::{head_bytes, tail_bytes};

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

/// Where spills live with no session: a one-shot run has no tree of its own.
///
/// Per process, not fixed: the temp dir is world-writable, so a fixed name
/// might already be held — `allocate`'s `0700` must not touch someone else's.
pub fn temp() -> PathBuf {
    let base = std::env::temp_dir();
    let free = |dir: &PathBuf| std::fs::symlink_metadata(dir).is_err();
    (0..64)
        .map(|n| base.join(format!("pi-spill-{}-{n}", std::process::id())))
        .find(free)
        .unwrap_or_else(|| base.join("pi-spill"))
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

/// Mint a fresh spill path and locator without writing: the directory is
/// made, the file is not. [`persist`] fills it in one shot.
pub(crate) fn allocate(ctx: &Ctx) -> Result<(PathBuf, String), ToolError> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // The pid in the name is what keeps a resumed session's fresh counter from
    // overwriting the files the earlier process spilled.
    let name = format!("{}-{n}", std::process::id());
    let dir = ctx.spill_root.join(ctx.spill_namespace());
    let path = dir.join(format!("{name}.log"));
    std::fs::create_dir_all(&dir)
        .map_err(|e| ToolError::Spill(format!("{}: {e}", dir.display())))?;
    // Spill paths are predictable, so the directory carries user-only rights:
    // set unconditionally, converging a pre-existing too-wide directory too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| ToolError::Spill(format!("{}: {e}", dir.display())))?;
    }
    Ok((path, format!("spill:{}/{}", ctx.spill_namespace(), name)))
}

/// Write `body` to a fresh spill file. Storage failure is a loud error, never
/// a silent fallback: a locator the model cannot read back is worse than none.
/// Exception: `output::bound` folds a spill failure into a truncation notice
/// instead, since the gate must not flood.
pub(crate) fn persist(ctx: &Ctx, body: &[u8]) -> Result<SpillRef, ToolError> {
    let (path, locator) = allocate(ctx)?;
    state::write_private(&path, body)
        .map_err(|e| ToolError::Spill(format!("{}: {e}", path.display())))?;
    Ok(SpillRef {
        bytes: body.len(),
        locator,
    })
}

/// Persist `body` under the session's spill directory, or say there was
/// nothing worth keeping.
pub fn write(ctx: &Ctx, body: &str) -> Result<Option<SpillRef>, ToolError> {
    if body.len() <= MAX_OUTPUT {
        return Ok(None);
    }
    Ok(Some(persist(ctx, body.as_bytes())?))
}

/// The elided-body view: head, a named omission, tail. The one wording both
/// `prune` and a streamed capture show, so it stays spelled one way.
pub(crate) fn elided(head: &str, whole: usize, tail: &str) -> String {
    format!(
        "{head}\n… {} bytes omitted …\n{tail}",
        whole.saturating_sub(head.len() + tail.len())
    )
}

/// Keep both ends of an over-long body: the head says what it was about, the
/// tail how it ended. Under the threshold the body comes back untouched.
pub fn prune(body: &str) -> String {
    if body.len() <= MAX_OUTPUT {
        return body.to_string();
    }
    let half = MAX_OUTPUT / 2;
    let (h, t) = (head_bytes(body, half), tail_bytes(body, half));
    elided(h, body.len(), t)
}

/// A body assembled from items that must not be split, held to the
/// transcript's budget with the whole of it spilled for recall.
///
/// Whole items, not bytes, so a mid-item cut never leaves unmatchable text.
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
    let kept = fits(items.iter(), |i: &&String| i.len(), room);
    // What is kept is already the front of `full`, and every item boundary is a
    // character boundary: slice it rather than build the same bytes twice.
    let head: usize = items[..kept].iter().map(String::len).sum();
    Ok(format!(
        "{}{notice}{}",
        &full[..head],
        say(items.len() - kept)
    ))
}

/// How many of `items` fit in `room`, taken in the order given.
///
/// The one thing every view that spends a transcript budget has in common:
/// where the cut lands and what marks it differ per view, but "stop when the
/// bytes run out" does not.
pub fn fits<T>(
    items: impl Iterator<Item = T>,
    size: impl Fn(&T) -> usize,
    mut room: usize,
) -> usize {
    items
        .map_while(|item| {
            room = room.checked_sub(size(&item))?;
            Some(())
        })
        .count()
}

#[cfg(test)]
mod tests {
    // A name already held is skipped, not reused — else the `0700` below
    // would be asked of a directory this process does not own.
    #[test]
    fn a_temp_root_already_held_is_skipped() {
        let held = temp();
        std::fs::create_dir_all(&held).unwrap();
        let next = temp();
        assert_ne!(next, held, "a name already taken is not ours to use");
        assert!(
            std::fs::symlink_metadata(&next).is_err(),
            "and the one it picks is free"
        );
        let _ = std::fs::remove_dir(&held);
    }

    use super::{locate, prune, temp};
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
        let child = Ctx::new(ws.clone()).with_session("p-1-subagent-0", root.clone());
        let parent = Ctx::new(ws).with_session("p-1", root);

        let big = "x".repeat(super::MAX_OUTPUT + 1);
        let spilled = super::write(&child, &big).unwrap().expect("over the cap");
        assert_eq!(
            std::fs::read_to_string(parent.spill_path(&spilled.locator).unwrap()).unwrap(),
            big
        );
    }

    // Under the cap the body passes untouched.
    #[test]
    fn prune_keeps_both_ends_and_names_what_it_took() {
        assert_eq!(prune("small"), "small");

        let body = "head-line\n".repeat(10_000);
        let got = prune(&body);
        assert!(got.ends_with("head-line\n"), "{got}");
        assert!(got.contains("bytes omitted"), "{got}");
        assert!(got.len() < body.len(), "prune must shrink");
    }
}
