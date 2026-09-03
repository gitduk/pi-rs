use std::path::{Component, Path, PathBuf};

use crate::{Tier, ToolError};

/// The workspace root every relative tool path is resolved against. Absolute
/// paths pass through untouched; nothing else in this crate calls the
/// filesystem directly. `write_roots` widen the boundary every tier above
/// read is held to — `bash`'s working directory as well as what `write` and
/// `edit` may touch.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    write_roots: Vec<PathBuf>,
}

// Collapse `.` and `..` without touching the filesystem, so a path that does
// not exist yet still resolves.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// Canonicalize the deepest existing ancestor and push the rest back on: an
// existing symlink is resolved away, and what does not exist cannot be one.
// None when no ancestor resolved at all — nothing was checked for links, so
// the caller must refuse rather than test a boundary against it.
fn real_until_missing(p: &Path) -> Option<PathBuf> {
    let mut ancestor = p;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(real) => {
                let mut out = real;
                for name in tail.iter().rev() {
                    out.push(name);
                }
                return Some(out);
            }
            Err(_) => match (ancestor.file_name(), ancestor.parent()) {
                (Some(name), Some(parent)) => {
                    tail.push(name);
                    ancestor = parent;
                }
                _ => return None,
            },
        }
    }
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
            write_roots: Vec::new(),
        })
    }

    /// Widen the boundary to extra absolute directories, for every tier above
    /// read: `bash` may work in one as well as `write` and `edit`. Entries
    /// must be absolute; one that does not exist yet is fine — the write tool
    /// creates it on first use. Each is reduced the way `resolve` reduces a
    /// target, so an existing symlink cannot sneak a narrower root past the
    /// check.
    pub fn with_write_roots(mut self, extra: &[impl AsRef<Path>]) -> std::io::Result<Self> {
        for dir in extra {
            let dir = dir.as_ref();
            if !dir.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("`write_roots` entries must be absolute: {}", dir.display()),
                ));
            }
            let reduced = real_until_missing(dir).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("`write_roots` entry cannot be resolved: {}", dir.display()),
                )
            })?;
            self.write_roots.push(reduced);
        }
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path. Relative paths join the workspace
    /// root; absolute paths are used as-is. The tier sets the boundary:
    /// `Tier::Read` may reach anywhere on the filesystem; write and exec
    /// tools must stay inside the workspace root or a configured write root,
    /// and a path that would escape both is refused. Canonicalizing the
    /// deepest existing ancestor is what stops a symlink from pointing
    /// outside the boundary; the remaining components cannot be links because
    /// they do not exist.
    pub fn resolve(&self, input: &str, tier: Tier) -> Result<PathBuf, ToolError> {
        if input.is_empty() {
            return Err(ToolError::Invalid("empty path".into()));
        }
        let raw = Path::new(input);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.root.join(raw)
        };
        let target = normalize(&joined);

        let resolved =
            real_until_missing(&target).ok_or_else(|| ToolError::Escape(input.into()))?;
        if tier != Tier::Read && !self.allows(&resolved) {
            return Err(ToolError::Escape(input.into()));
        }
        Ok(resolved)
    }

    fn allows(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
            || self.write_roots.iter().any(|root| path.starts_with(root))
    }

    /// Workspace-relative form for display. Absolute paths would let the model
    /// echo them back and pin the transcript to one machine.
    pub fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn resolves_relative_and_nonexistent_paths() {
        let (_d, ws) = ws();
        let p = ws.resolve("a/b/c.txt", Tier::Write).unwrap();
        assert!(p.starts_with(ws.root()));
        assert_eq!(ws.display(&p), "a/b/c.txt");
    }

    #[test]
    fn rejects_paths_outside_the_workspace() {
        let (_d, ws) = ws();
        assert!(matches!(
            ws.resolve("../outside", Tier::Write),
            Err(ToolError::Escape(_))
        ));
        assert!(matches!(
            ws.resolve("a/../../outside", Tier::Write),
            Err(ToolError::Escape(_))
        ));
        assert!(matches!(
            ws.resolve("/etc/passwd", Tier::Write),
            Err(ToolError::Escape(_))
        ));
    }

    #[test]
    fn read_tier_allows_paths_outside_the_workspace() {
        let (_d, ws) = ws();
        let outside = ws.root().parent().unwrap().join("outside");
        assert_eq!(ws.resolve("../outside", Tier::Read).unwrap(), outside);
        assert_eq!(ws.resolve("a/../../outside", Tier::Read).unwrap(), outside);
        assert_eq!(
            ws.resolve("/etc/passwd", Tier::Read).unwrap(),
            Path::new("/etc/passwd")
        );
    }

    #[test]
    fn inner_traversal_that_stays_inside_is_allowed() {
        let (_d, ws) = ws();
        let p = ws.resolve("a/b/../c.txt", Tier::Write).unwrap();
        assert_eq!(ws.display(&p), "a/c.txt");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_pointing_outside() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.root().join("escape")).unwrap();
        assert!(matches!(
            ws.resolve("escape/secret", Tier::Write),
            Err(ToolError::Escape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_tier_follows_a_symlink_outside() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.root().join("escape")).unwrap();
        assert_eq!(
            ws.resolve("escape/secret", Tier::Read).unwrap(),
            outside.path().join("secret")
        );
    }

    #[test]
    fn a_configured_write_root_is_reachable() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        let ws = ws.with_write_roots(&[outside.path()]).unwrap();
        let p = ws
            .resolve(&format!("{}/x.txt", outside.path().display()), Tier::Write)
            .unwrap();
        assert_eq!(p, outside.path().join("x.txt"));
    }

    #[test]
    fn a_path_under_no_configured_root_stays_refused() {
        let (_d, ws) = ws();
        let allowed = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ws = ws.with_write_roots(&[allowed.path()]).unwrap();
        assert!(matches!(
            ws.resolve(&format!("{}/x.txt", other.path().display()), Tier::Write),
            Err(ToolError::Escape(_))
        ));
    }

    #[test]
    fn a_write_root_may_not_exist_yet() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        let root = outside.path().join("scratch");
        let ws = ws.with_write_roots(&[&root]).unwrap();
        let p = ws
            .resolve(&format!("{}/x.txt", root.display()), Tier::Write)
            .unwrap();
        assert_eq!(p, root.join("x.txt"));
    }

    /// A write root widens every tier above read, `bash`'s working directory
    /// included. Documented as such because it is what the check does: the
    /// boundary turns on `tier != Read`, not on the tier being `Write`.
    #[test]
    fn a_write_root_widens_the_exec_tier_too() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        let ws = ws.with_write_roots(&[outside.path()]).unwrap();
        let named = format!("{}/sub", outside.path().display());
        assert!(ws.resolve(&named, Tier::Exec).is_ok(), "bash may work there");
        assert!(ws.resolve(&named, Tier::Write).is_ok());
    }

    /// The claim `with_write_roots` makes: a write root is reduced the way a
    /// target is, so a symlink standing where the root is named cannot widen
    /// the boundary to whatever it points at.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_write_root_is_reduced_to_what_it_points_at() {
        let (_d, ws) = ws();
        let real = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = real.path().join("link");
        std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();

        let ws = ws.with_write_roots(&[&link]).unwrap();
        // Named through the link or through its target, the same file is the
        // same decision — the boundary is the directory, not the spelling.
        let through_link = ws
            .resolve(&format!("{}/x.txt", link.display()), Tier::Write)
            .expect("the root it actually names");
        assert_eq!(through_link, elsewhere.path().canonicalize().unwrap().join("x.txt"));

        // And it widened to that target only, not to the link's own parent.
        assert!(matches!(
            ws.resolve(&format!("{}/y.txt", real.path().display()), Tier::Write),
            Err(ToolError::Escape(_))
        ));
    }

    /// A symlink inside a write root cannot carry a write back out of it.
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_a_write_root_cannot_escape_it() {
        let (_d, ws) = ws();
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path(), allowed.path().join("out")).unwrap();

        let ws = ws.with_write_roots(&[allowed.path()]).unwrap();
        assert!(
            matches!(
                ws.resolve(&format!("{}/out/secret", allowed.path().display()), Tier::Write),
                Err(ToolError::Escape(_))
            ),
            "a link out of the write root is still out of it"
        );
    }

    #[test]
    fn a_relative_write_root_is_refused() {
        let (_d, ws) = ws();
        let err = ws.with_write_roots(&["relative"]).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }
}
