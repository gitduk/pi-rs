use std::path::{Component, Path, PathBuf};

use crate::ToolError;

/// The workspace root every relative tool path is resolved against.
/// Absolute paths pass through untouched; nothing else in this crate calls
/// the filesystem directly.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

/// Collapse `.` and `..` without touching the filesystem, so a path that does
/// not exist yet still resolves.
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

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        Ok(Self {
            root: root.as_ref().canonicalize()?,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a model-supplied path. Relative paths join the workspace
    /// root; absolute paths are used as-is. Write and exec tools must stay
    /// inside the workspace; a path that would escape it is refused.
    /// Canonicalizing the deepest existing ancestor is what stops a symlink
    /// from pointing outside the workspace; the remaining components cannot
    /// be links because they do not exist.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, ToolError> {
        self.resolve_inner(input, true)
    }

    /// Like `resolve`, but with no boundary. Read-only tools use this so the
    /// model can read and search anywhere on the filesystem.
    pub fn resolve_free(&self, input: &str) -> Result<PathBuf, ToolError> {
        self.resolve_inner(input, false)
    }

    fn resolve_inner(&self, input: &str, guarded: bool) -> Result<PathBuf, ToolError> {
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

        let mut ancestor = target.as_path();
        let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
        let real = loop {
            match ancestor.canonicalize() {
                Ok(real) => break real,
                Err(_) => match (ancestor.file_name(), ancestor.parent()) {
                    (Some(name), Some(parent)) => {
                        tail.push(name);
                        ancestor = parent;
                    }
                    _ => return Err(ToolError::Escape(input.into())),
                },
            }
        };

        let mut resolved = real;
        for name in tail.iter().rev() {
            resolved.push(name);
        }
        if guarded && !resolved.starts_with(&self.root) {
            return Err(ToolError::Escape(input.into()));
        }
        Ok(resolved)
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
        let p = ws.resolve("a/b/c.txt").unwrap();
        assert!(p.starts_with(ws.root()));
        assert_eq!(ws.display(&p), "a/b/c.txt");
    }

    #[test]
    fn rejects_paths_outside_the_workspace() {
        let (_d, ws) = ws();
        assert!(matches!(
            ws.resolve("../outside"),
            Err(ToolError::Escape(_))
        ));
        assert!(matches!(
            ws.resolve("a/../../outside"),
            Err(ToolError::Escape(_))
        ));
        assert!(matches!(
            ws.resolve("/etc/passwd"),
            Err(ToolError::Escape(_))
        ));
    }

    #[test]
    fn resolve_free_allows_paths_outside_the_workspace() {
        let (_d, ws) = ws();
        let outside = ws.root().parent().unwrap().join("outside");
        assert_eq!(ws.resolve_free("../outside").unwrap(), outside);
        assert_eq!(ws.resolve_free("a/../../outside").unwrap(), outside);
        assert_eq!(
            ws.resolve_free("/etc/passwd").unwrap(),
            Path::new("/etc/passwd")
        );
    }

    #[test]
    fn inner_traversal_that_stays_inside_is_allowed() {
        let (_d, ws) = ws();
        let p = ws.resolve("a/b/../c.txt").unwrap();
        assert_eq!(ws.display(&p), "a/c.txt");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlink_pointing_outside() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.root().join("escape")).unwrap();
        assert!(matches!(
            ws.resolve("escape/secret"),
            Err(ToolError::Escape(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_free_follows_a_symlink_outside() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.root().join("escape")).unwrap();
        assert_eq!(
            ws.resolve_free("escape/secret").unwrap(),
            outside.path().join("secret")
        );
    }
}
