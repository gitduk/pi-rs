use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::{Tier, ToolError, Workspace};

// One sweep per target, so the count itself is a resource to bound.
const MAX_TARGETS: usize = 64;

/// Compile a comma-free list of globs. A bare name like `*.rs` should match at
/// any depth, which `**/` prefixing is what makes true.
pub fn globs(patterns: &[String]) -> Result<Option<GlobSet>, ToolError> {
    compile(patterns, |p| {
        vec![if p.contains('/') {
            p.to_string()
        } else {
            format!("**/{p}")
        }]
    })
}

/// Compile exclusion globs. A bare name excludes the subtree of that name as
/// well as any file carrying it: an exclude names territory, not just files.
pub fn excludes(patterns: &[String]) -> Result<Option<GlobSet>, ToolError> {
    compile(patterns, |p| {
        let head = if p.contains('/') {
            p.to_string()
        } else {
            format!("**/{p}")
        };
        vec![head.clone(), format!("{head}/**")]
    })
}

fn compile(
    patterns: &[String],
    forms: impl Fn(&str) -> Vec<String>,
) -> Result<Option<GlobSet>, ToolError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut set = GlobSetBuilder::new();
    for p in patterns {
        for f in forms(p) {
            let glob =
                Glob::new(&f).map_err(|e| ToolError::Invalid(format!("bad glob `{p}`: {e}")))?;
            set.add(glob);
        }
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
/// The machine-wide global gitignore only applies inside the workspace: a
/// search outside it is scoped to what the user named, not to their config.
/// An exclude set prunes matching entries here, at the walk: one matching
/// directory skips its whole subtree instead of being filtered file by file.
pub fn walker(ws: &Workspace, root: &Path, skip: Option<GlobSet>) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(root.starts_with(ws.root()))
        .require_git(false)
        .filter_entry(move |e| {
            e.file_name() != ".git" && skip.as_ref().is_none_or(|s| !s.is_match(e.path()))
        });
    b
}

/// Resolve an optional subdirectory argument to a walk root. The walk may
/// leave the workspace when the calling tool is `Tier::Read`.
pub fn root_of(ws: &Workspace, path: &Option<String>, tier: Tier) -> Result<PathBuf, ToolError> {
    match path {
        Some(p) => ws.resolve(p, tier),
        None => Ok(ws.root().to_path_buf()),
    }
}

/// Resolve one-or-more targets to distinct walk roots; none at all is the
/// workspace root. Duplicates collapse, so overlaps never search one file
/// twice. More than [`MAX_TARGETS`] is refused.
pub fn roots_of(ws: &Workspace, targets: &[&str], tier: Tier) -> Result<Vec<PathBuf>, ToolError> {
    if targets.len() > MAX_TARGETS {
        return Err(ToolError::Invalid(format!(
            "{} paths is over the {MAX_TARGETS}-path limit",
            targets.len()
        )));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for t in targets {
        let r = ws.resolve(t, tier)?;
        if seen.insert(r.clone()) {
            roots.push(r);
        }
    }
    if roots.is_empty() {
        roots.push(ws.root().to_path_buf());
    }
    Ok(roots)
}

/// A NUL in the first block is the same sniff `read` uses; searching a binary
/// yields noise the model cannot act on.
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8_000).any(|b| *b == 0)
}

#[cfg(test)]
mod exclude_tests {
    use super::excludes;
    use globset::GlobSet;

    fn set(patterns: &[&str]) -> GlobSet {
        excludes(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_bare_name_excludes_files_under_the_directory_of_that_name() {
        let s = set(&["__pycache__"]);
        assert!(s.is_match("app/__pycache__/x.py"));
        assert!(!s.is_match("app/x.py"));
    }

    #[test]
    fn a_bare_name_still_excludes_a_file_carrying_it() {
        assert!(set(&["x.py"]).is_match("deep/nested/x.py"));
    }

    #[test]
    fn a_slashed_pattern_excludes_its_subtree() {
        let s = set(&["app/gen"]);
        assert!(s.is_match("app/gen/out.rs"));
        assert!(!s.is_match("app/general.rs"));
    }
}
