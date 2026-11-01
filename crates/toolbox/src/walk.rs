use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use tool::{Tier, ToolError, Workspace};

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

/// A gitignore-aware walk rooted in a directory. Links are not followed, to
/// avoid leaving the root or revisiting files.
///
/// Dotted entries are kept except `.git` itself; the global gitignore applies
/// only inside the workspace; an exclude set prunes whole subtrees at the walk.
pub fn walker(ws: &Workspace, root: &Path, skip: Option<GlobSet>) -> ignore::WalkBuilder {
    let inside = root.starts_with(ws.root());
    build(root, inside, skip, Some(ws.root().to_path_buf()))
}

/// `walker` for a caller holding a path, not a `Workspace`: `global` says
/// whether the user's global gitignore applies at `root`.
pub fn walker_at(root: &Path, global: bool, skip: Option<GlobSet>) -> ignore::WalkBuilder {
    build(root, global, skip, None)
}

fn build(
    root: &Path,
    global: bool,
    skip: Option<GlobSet>,
    base: Option<PathBuf>,
) -> ignore::WalkBuilder {
    let mut b = ignore::WalkBuilder::new(root);
    let top = root.to_path_buf();
    b.hidden(false)
        .follow_links(false)
        .git_ignore(true)
        .git_global(global)
        .require_git(false)
        .filter_entry(move |e| {
            e.file_name() != ".git"
                && skip
                    .as_ref()
                    .is_none_or(|s| !matches(s, e.path(), &top, base.as_deref()))
        });
    b
}

/// Whether `set` matches `path`. A pattern is read against the path relative
/// to the walk `root`, and also relative to `base` (the workspace root) when
/// the path lies under it, so `src/**/mod.rs` means the same wherever the
/// search starts. A root that is itself a file is matched by its name.
pub fn matches(set: &GlobSet, path: &Path, root: &Path, base: Option<&Path>) -> bool {
    let local = match path.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => path.file_name().map(Path::new),
        Ok(rel) => Some(rel),
        Err(_) => None,
    };
    let shared = base
        .and_then(|b| path.strip_prefix(b).ok())
        .filter(|rel| !rel.as_os_str().is_empty());
    local.into_iter().chain(shared).any(|rel| set.is_match(rel))
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
/// twice. More than `MAX_TARGETS` is refused.
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
