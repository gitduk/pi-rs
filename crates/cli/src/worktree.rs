//! Parallel checkouts of one repository, and moving the session between them.
//!
//! A worktree lives at `<repo>/.worktrees/<name>` on a branch of the same name,
//! so one word names the directory, the branch and the command argument.
//! Git refuses to check one branch out twice, so the alternative to a branch
//! per worktree is a detached HEAD — commits reachable only through the reflog.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

/// Where worktrees live, relative to the repository root.
pub const DIR: &str = ".worktrees";

/// One checkout of the repository.
#[derive(Debug, Clone)]
pub struct Tree {
    pub path: PathBuf,
    /// What `/worktree` takes to reach it: the path under `.worktrees`, or the
    /// directory name for the main checkout, which lives outside it.
    pub name: String,
    /// None when the checkout is on a detached HEAD.
    pub branch: Option<String>,
    /// The repository's own working tree, the one that is not a worktree.
    pub main: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    // -C rather than the inherited cwd: a stale working directory silently
    // resolves against the wrong repository.
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git: {e}"))?;
    Ok(out)
}

fn stderr_of(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text.trim().lines().next_back().unwrap_or("").trim();
    if line.is_empty() {
        "git failed".to_string()
    } else {
        line.to_string()
    }
}

fn checked(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args)?;
    if !out.status.success() {
        bail!("{}", stderr_of(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Every checkout of the repository `dir` belongs to, the main one first.
///
/// The main one leading is what lets a worktree find its way back out:
/// `--show-toplevel` would answer with the checkout asking, not the one that
/// owns `.worktrees`.
pub fn list(dir: &Path) -> Result<Vec<Tree>> {
    let listed = checked(dir, &["worktree", "list", "--porcelain"])?;
    let mut out: Vec<Tree> = Vec::new();
    let mut root = PathBuf::new();
    // Records are blank-line separated, `worktree <path>` always first.
    for line in listed.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(path);
            let main = out.is_empty();
            if main {
                root = path.clone();
            }
            let name = name_of(&path, &root, main);
            out.push(Tree {
                path,
                name,
                branch: None,
                main,
            });
        } else if let Some(reference) = line.strip_prefix("branch ")
            && let Some(last) = out.last_mut()
        {
            last.branch = Some(reference.trim_start_matches("refs/heads/").to_string());
        }
    }
    Ok(out)
}

// Under `.worktrees` the name is the path below it, so `feat/one` keeps both
// halves; the main checkout is not under it and answers to its directory name.
fn name_of(path: &Path, root: &Path, main: bool) -> String {
    if main {
        return path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
    }
    path.strip_prefix(root.join(DIR))
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

/// Refuse a name that would not stay under `.worktrees`, or that git would not
/// take as a branch. The check is on the name rather than the joined path
/// because the error should say which word was wrong.
fn vetted(name: &str) -> Result<&str> {
    let name = name.trim().trim_end_matches('/');
    if name.is_empty() {
        bail!("a worktree needs a name");
    }
    if name.starts_with('/') || name.starts_with('-') {
        bail!("`{name}` cannot start with `/` or `-`");
    }
    if name.split('/').any(|part| part.is_empty() || part == ".." || part == ".") {
        bail!("`{name}` is not a path under {DIR}/");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/')))
    {
        bail!("`{bad}` is not allowed in a worktree name");
    }
    Ok(name)
}

/// Which of `trees` holds `path`.
///
/// By longest containing path rather than by equality: a run started in a
/// subdirectory is still in that checkout, and the main one contains every
/// other, so only the longest match answers.
pub fn holding<'a>(trees: &'a [Tree], path: &Path) -> Option<&'a Tree> {
    trees
        .iter()
        .filter(|t| path.starts_with(&t.path))
        .max_by_key(|t| t.path.as_os_str().len())
}

/// The worktree `dir` sits in, or None in the repository's own checkout and
/// None outside a repository, where there is nothing to name.
pub fn current(dir: &Path) -> Option<String> {
    let here = dir.canonicalize().ok()?;
    let trees = list(&here).ok()?;
    holding(&trees, &here)
        .filter(|t| !t.main)
        .map(|t| t.name.clone())
}

/// What entering a name did, so the caller can say it.
#[derive(Debug)]
pub enum Entered {
    /// It was already there.
    Existing,
    /// The branch existed and now has a checkout.
    Checkout,
    /// Both the branch and the checkout are new.
    Created,
}

/// The checkout `name` refers to, creating the worktree and the branch if they
/// are not there yet. Idempotent: running it twice means "go there".
pub fn enter(dir: &Path, name: &str) -> Result<(Tree, Entered)> {
    let name = vetted(name)?;
    let mut trees = list(dir)?;

    if let Some(i) = trees.iter().position(|t| t.name == name) {
        return Ok((trees.swap_remove(i), Entered::Existing));
    }

    // `.worktrees` hangs off the repository, not off whichever checkout asked,
    // so a worktree created from inside another is its sibling.
    let root = match trees.first() {
        Some(main) => main.path.clone(),
        None => bail!("not a git repository"),
    };
    let path = root.join(DIR).join(name);
    if path.exists() {
        bail!("{} exists but is not a registered worktree", path.display());
    }

    // An existing branch is checked out rather than re-created: `-b` on one
    // that exists fails, and asking twice means the same feature both times.
    let known = git(
        dir,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{name}")],
    )?
    .status
    .success();
    let target = path.to_string_lossy().into_owned();
    let added = if known {
        git(dir, &["worktree", "add", &target, name])?
    } else {
        git(dir, &["worktree", "add", "-b", name, &target])?
    };
    if !added.status.success() {
        bail!("{}", stderr_of(&added));
    }
    // Read back rather than assembled here: one place decides what a checkout
    // is called and which branch it is on, and git canonicalizes the path.
    let found = list(dir)?
        .into_iter()
        .find(|t| t.name == name)
        .ok_or_else(|| {
            // Reached when git resolved the path elsewhere — a symlinked
            // `.worktrees` does that. Left registered rather than adopted.
            anyhow::anyhow!(
                "git put the checkout somewhere other than {} — is {DIR} a symlink?",
                path.display()
            )
        })?;
    let how = if known {
        Entered::Checkout
    } else {
        Entered::Created
    };
    Ok((found, how))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let at = dir.path();
        checked(at, &["init", "-q", "--initial-branch=main", "."]).unwrap();
        checked(at, &["config", "user.email", "t@example.com"]).unwrap();
        checked(at, &["config", "user.name", "t"]).unwrap();
        std::fs::write(at.join("a.txt"), "hi").unwrap();
        checked(at, &["add", "-A"]).unwrap();
        checked(at, &["commit", "-qm", "init"]).unwrap();
        dir
    }

    #[test]
    fn a_new_name_gets_a_branch_and_a_checkout() {
        let dir = repo();
        let (tree, how) = enter(dir.path(), "feature-one").unwrap();
        assert!(matches!(how, Entered::Created));
        assert_eq!(tree.branch.as_deref(), Some("feature-one"));
        assert!(!tree.main);
        assert!(tree.path.join("a.txt").is_file());
        assert!(tree.path.ends_with(".worktrees/feature-one"));
    }

    #[test]
    fn entering_the_same_name_twice_goes_back_to_it() {
        let dir = repo();
        let (first, _) = enter(dir.path(), "feature-one").unwrap();
        let (again, how) = enter(dir.path(), "feature-one").unwrap();
        assert_eq!(first.path, again.path);
        assert!(matches!(how, Entered::Existing));
        assert_eq!(again.branch.as_deref(), Some("feature-one"));
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_recreated() {
        let dir = repo();
        checked(dir.path(), &["branch", "already"]).unwrap();
        let (_, how) = enter(dir.path(), "already").unwrap();
        assert!(matches!(how, Entered::Checkout));
    }

    #[test]
    fn the_main_checkout_leads_the_list_and_answers_to_its_directory_name() {
        let dir = repo();
        enter(dir.path(), "feature-one").unwrap();
        let trees = list(dir.path()).unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees[0].main);
        assert_eq!(
            trees[0].name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
        assert_eq!(trees[0].branch.as_deref(), Some("main"));
        assert_eq!(trees[1].name, "feature-one");
    }

    #[test]
    fn a_nested_name_keeps_both_halves() {
        let dir = repo();
        let (tree, _) = enter(dir.path(), "feat/one").unwrap();
        assert!(tree.path.ends_with(".worktrees/feat/one"));
        assert_eq!(tree.branch.as_deref(), Some("feat/one"));
        let trees = list(dir.path()).unwrap();
        assert!(trees.iter().any(|t| t.name == "feat/one"));
    }

    #[test]
    fn a_worktree_reaches_the_main_checkout_and_its_siblings() {
        let dir = repo();
        let (inside, _) = enter(dir.path(), "feature-one").unwrap();
        // From within a worktree the main checkout is still the first listed,
        // which is what lets `/worktree <repo>` get back out.
        let trees = list(&inside.path).unwrap();
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[0].path, dir.path().canonicalize().unwrap());
        assert!(trees[0].main);
    }

    #[test]
    fn a_worktree_created_from_inside_another_is_its_sibling() {
        // Not nested under the one it was asked from: `.worktrees` hangs off
        // the repository, and asking from anywhere in it means the same place.
        let dir = repo();
        let (first, _) = enter(dir.path(), "one").unwrap();
        let (second, _) = enter(&first.path, "two").unwrap();
        assert_eq!(second.path.parent(), first.path.parent());
        assert_eq!(list(dir.path()).unwrap().len(), 3);
    }

    #[test]
    fn a_branch_another_worktree_holds_is_refused_by_git() {
        // Git will not check one branch out twice, and the reason it gives is
        // better than anything this could say for it.
        let dir = repo();
        enter(dir.path(), "one").unwrap();
        checked(dir.path(), &["worktree", "add", "--detach", "elsewhere"]).unwrap();
        std::fs::remove_dir_all(dir.path().join(DIR).join("one")).unwrap();
        checked(dir.path(), &["worktree", "prune"]).unwrap();
        checked(dir.path(), &["-C", "elsewhere", "checkout", "-q", "one"]).unwrap();
        let err = enter(dir.path(), "one").unwrap_err().to_string();
        assert!(err.contains("already used by worktree"), "{err}");
    }

    #[test]
    fn a_subdirectory_is_held_by_the_checkout_it_is_in() {
        let dir = repo();
        let (tree, _) = enter(dir.path(), "one").unwrap();
        let deep = tree.path.join("crates/cli");
        std::fs::create_dir_all(&deep).unwrap();
        let trees = list(dir.path()).unwrap();
        // The main checkout contains `.worktrees`, so equality would miss and
        // a plain prefix test would answer with the wrong one.
        let held = holding(&trees, &deep).expect("a checkout holds it");
        assert_eq!(held.name, "one");
        assert_eq!(current(&deep).as_deref(), Some("one"));
        assert_eq!(current(dir.path()), None, "the main checkout names nothing");
    }

    #[test]
    fn a_name_that_would_leave_the_worktrees_directory_is_refused() {
        let dir = repo();
        for bad in ["", "..", "../escape", "/etc", "a//b", "-b", "a b", "a;rm"] {
            assert!(enter(dir.path(), bad).is_err(), "accepted `{bad}`");
        }
    }

    #[test]
    fn a_directory_in_the_way_is_reported_rather_than_entered() {
        let dir = repo();
        let squatting = dir.path().join(DIR).join("taken");
        std::fs::create_dir_all(&squatting).unwrap();
        let err = enter(dir.path(), "taken").unwrap_err().to_string();
        assert!(err.contains("not a registered worktree"), "{err}");
    }
}
