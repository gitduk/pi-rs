use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{ToolError, Workspace};

/// Compile a comma-free list of globs. A bare name like `*.rs` should match at
/// any depth, which `**/` prefixing is what makes true.
pub fn globs(patterns: &[String]) -> Result<Option<GlobSet>, ToolError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for p in patterns {
        let expanded = if p.contains('/') {
            p.clone()
        } else {
            format!("**/{p}")
        };
        let glob =
            Glob::new(&expanded).map_err(|e| ToolError::Invalid(format!("bad glob `{p}`: {e}")))?;
        set.add(glob);
    }
    Ok(Some(
        set.build().map_err(|e| ToolError::Invalid(e.to_string()))?,
    ))
}

/// A gitignore-aware walk rooted in a directory. Links are not followed: one
/// pointing elsewhere would leave the root and can revisit the same files.
///
/// Dotted entries are kept — `.github`, `.cargo` and friends are ordinary
/// project files — but `.git` itself is not: an object store is megabytes of
/// noise no model can act on, and it is never what a search meant to find.
pub fn walker(root: &Path) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git");
    b
}

/// Resolve an optional subdirectory argument to a walk root. Searches are
/// read-only, so the walk may leave the workspace.
pub fn root_of(ws: &Workspace, path: &Option<String>) -> Result<PathBuf, ToolError> {
    match path {
        Some(p) => ws.resolve_free(p),
        None => Ok(ws.root().to_path_buf()),
    }
}

/// A NUL in the first block is the same sniff `read` uses; searching a binary
/// yields noise the model cannot act on.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8_000).any(|b| *b == 0)
}
