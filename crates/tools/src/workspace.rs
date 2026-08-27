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
    /// root; absolute paths are used as-is. No boundary is enforced: tools
    /// may reach anywhere on the filesystem. Canonicalizing the deepest
    /// existing ancestor is what makes a path that does not exist yet real;
    /// the remaining components cannot be links because they do not exist.
    pub fn resolve(&self, input: &str) -> Result<PathBuf, ToolError> {
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
    fn allows_paths_outside_the_workspace() {
        let (_d, ws) = ws();
        let outside = ws.root().parent().unwrap().join("outside");
        assert_eq!(ws.resolve("../outside").unwrap(), outside);
        assert_eq!(ws.resolve("a/../../outside").unwrap(), outside);
        assert_eq!(
            ws.resolve("/etc/passwd").unwrap(),
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
    fn a_symlink_pointing_outside_resolves_to_its_target() {
        let (_d, ws) = ws();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), ws.root().join("escape")).unwrap();
        assert_eq!(
            ws.resolve("escape/secret").unwrap(),
            outside.path().join("secret")
        );
    }
}
