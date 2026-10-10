//! Parallel checkouts of one repository, and moving the session between them.
//!
//! A worktree lives at `<repo>.worktrees/<name>`, beside the repository, on
//! a branch of the same name — one word names the directory, branch and arg.

use super::Core;
use crate::input::Step;
use crate::input::refused;
use pi_store::listing::Listing;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, bail};

/// What the directory holding the worktrees adds to the repository's name.
const SUFFIX: &str = ".worktrees";

/// Beside the repository, never inside: nested, a checkout is searched,
/// written and `git add`ed as part of the main one.
fn home(root: &Path) -> Result<PathBuf> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        bail!("{} has no parent to keep worktrees beside", root.display());
    };
    Ok(parent.join(format!("{}{SUFFIX}", name.to_string_lossy())))
}

/// One checkout of the repository.
#[derive(Debug, Clone)]
pub struct Tree {
    pub path: PathBuf,
    /// What `/worktree` takes to reach it: the path under `home`, or the
    /// directory name for the main checkout, which lives outside it.
    pub name: String,
    /// None when the checkout is on a detached HEAD.
    pub branch: Option<String>,
    /// The repository's own working tree, the one that is not a worktree.
    pub main: bool,
}

impl Tree {
    /// The branch to name beside this checkout, or None when naming it would
    /// only repeat the worktree's own name back at the reader.
    pub fn branch_note(&self) -> Option<&str> {
        match self.branch.as_deref() {
            Some(b) if b != self.name => Some(b),
            Some(_) => None,
            None => Some("detached HEAD"),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    // -C avoids resolving against a stale inherited cwd; GIT_ADVICE=0 drops
    // hints so refusals are shown exactly as git wrote them.
    let out = Command::new("git")
        .env("GIT_ADVICE", "0")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("git: {e}"))?;
    Ok(out)
}

fn stderr_of(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    let said: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if said.is_empty() {
        "git failed".to_string()
    } else {
        said.join("; ")
    }
}

fn checked(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args)?;
    if !out.status.success() {
        bail!("{}", stderr_of(&out));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
// Whether the branch `name` exists in the repository `dir` belongs to.
fn branch_exists(dir: &Path, name: &str) -> Result<bool> {
    Ok(git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )?
    .status
    .success())
}

/// Every checkout of the repository `dir` belongs to, the main one first.
///
/// The main one leading is what lets a worktree find its way back out:
/// `--show-toplevel` would answer with the checkout asking, not the one that
/// the others hang off.
pub fn list(dir: &Path) -> Result<Vec<Tree>> {
    let listed = checked(dir, &["worktree", "list", "--porcelain"])?;
    // `prunable` (dir gone, metadata lingers until `worktree prune`) is kept
    // while parsing and dropped at the end, so reordered fields aren't stranded.
    let mut out: Vec<(Tree, bool)> = Vec::new();
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
            out.push((
                Tree {
                    path,
                    name,
                    branch: None,
                    main,
                },
                false,
            ));
        } else if let Some(reference) = line.strip_prefix("branch ")
            && let Some((last, _)) = out.last_mut()
        {
            last.branch = Some(reference.trim_start_matches("refs/heads/").to_string());
        } else if line.starts_with("prunable")
            && let Some((_, gone)) = out.last_mut()
        {
            *gone = true;
        }
    }
    Ok(out
        .into_iter()
        .filter(|(_, gone)| !gone)
        .map(|(t, _)| t)
        .collect())
}

// Under `home` the name is the path below it (`feat/one`); any other
// checkout, main or made elsewhere, answers to its directory name.
fn name_of(path: &Path, root: &Path, main: bool) -> String {
    if !main
        && let Ok(home) = home(root)
        && let Ok(rel) = path.strip_prefix(home)
    {
        return rel.display().to_string();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// Refuse a name that would not stay under `home`, or that git would not
// take as a branch; checked before joining so the error names the word.
fn vetted(name: &str) -> Result<&str> {
    let name = name.trim().trim_end_matches('/');
    if name.is_empty() {
        bail!("a worktree needs a name");
    }
    if name.starts_with('/') || name.starts_with('-') {
        bail!("`{name}` cannot start with `/` or `-`");
    }
    if name
        .split('/')
        .any(|part| part.is_empty() || part == ".." || part == ".")
    {
        bail!("`{name}` would leave the worktrees directory");
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
/// subdirectory is still in that checkout, and one added inside the main
/// checkout by hand is contained by both, so only the longest match answers.
pub fn holding<'a>(trees: &'a [Tree], path: &Path) -> Option<&'a Tree> {
    trees
        .iter()
        .filter(|t| path.starts_with(&t.path))
        .max_by_key(|t| t.path.as_os_str().len())
}

/// The main checkout of the repository holding `dir`, or `dir` outside one.
/// Read off `.git` rather than asked of git: it runs on every resolve.
pub fn main_root(dir: &Path) -> PathBuf {
    for here in dir.ancestors() {
        let git = here.join(".git");
        if git.is_dir() {
            return here.to_path_buf();
        }
        // A worktree's `.git` is a file naming `<main>/.git/worktrees/<name>`.
        if let Ok(link) = std::fs::read_to_string(&git) {
            let named = link.trim().strip_prefix("gitdir: ").map(|p| here.join(p));
            let main = named.as_deref().and_then(|p| {
                let worktrees = p.parent()?;
                let dot_git = worktrees.parent()?;
                (worktrees.ends_with("worktrees") && dot_git.ends_with(".git"))
                    .then(|| dot_git.parent())
                    .flatten()
            });
            return main.map_or_else(|| here.to_path_buf(), Path::to_path_buf);
        }
    }
    dir.to_path_buf()
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

/// The checkout `name` refers to, creating the worktree and the branch if they
/// are not there yet. Idempotent: running it twice means "go there".
pub fn enter(dir: &Path, name: &str) -> Result<Tree> {
    let name = vetted(name)?;
    let mut trees = list(dir)?;

    if let Some(i) = trees.iter().position(|t| t.name == name) {
        return Ok(trees.swap_remove(i));
    }

    // `home` hangs off the repository, not off whichever checkout asked,
    // so a worktree created from inside another is its sibling.
    let root = match trees.first() {
        Some(main) => main.path.clone(),
        None => bail!("not a git repository"),
    };
    let path = home(&root)?.join(name);
    let target = path.to_string_lossy().into_owned();
    // An existing branch is checked out rather than re-created: `-b` on one
    // that exists fails, and asking twice means the same feature both times.
    let added = if branch_exists(dir, name)? {
        checked(dir, &["worktree", "add", &target, name])
    } else {
        checked(dir, &["worktree", "add", "-b", name, &target])
    };
    if let Err(e) = added {
        // Git speaks for the path it owns; an unregistered directory sitting
        // there is the one case worth naming ourselves.
        if path.exists() {
            bail!("{} exists but is not a registered worktree", path.display());
        }
        return Err(e);
    }
    // Read back rather than assembled here: one place decides what a checkout
    // is called and which branch it is on, and git canonicalizes the path.
    let found = list(dir)?
        .into_iter()
        .find(|t| t.name == name)
        .ok_or_else(|| {
            // Reached when git resolved the path elsewhere — a symlinked
            // worktrees directory does that. Left registered rather than adopted.
            anyhow::anyhow!(
                "git put the checkout somewhere other than {} — is its parent a symlink?",
                path.display()
            )
        })?;
    Ok(found)
}
/// What removing a checkout took, for the caller's receipt.
#[derive(Debug)]
pub struct Removed {
    /// Where the checkout was, so the caller can sweep what was recorded
    /// under it.
    pub path: PathBuf,
    /// The branch deleted with it, None when the checkout was detached or the
    /// branch was already gone.
    pub branch: Option<String>,
    /// A step that came up short, for the receipt — a branch the deletion
    /// left behind and why.
    pub note: Option<String>,
}

/// Remove the checkout `name` refers to, and its branch once merged;
/// unmerged, the branch stays and the receipt says so.
///
/// A dirty checkout's refusal from git is passed through, not forced.
pub fn remove(dir: &Path, name: &str) -> Result<Removed> {
    let name = vetted(name)?;
    let trees = list(dir)?;
    // The main checkout answers to its directory name, and a linked tree may
    // share it; removal means a linked one, so that one wins.
    let Some(at) = trees
        .iter()
        .position(|t| !t.main && t.name == name)
        .or_else(|| trees.iter().position(|t| t.name == name))
    else {
        bail!("`{name}` is not one of this repository's worktrees");
    };
    let tree = &trees[at];
    if tree.main {
        bail!("`{name}` is the main checkout — it cannot be removed");
    }
    // From inside the checkout this session is in, removal would leave a live
    // run standing in a directory that just went.
    if let Some(here) = holding(&trees, dir)
        && here.path == tree.path
    {
        bail!("`{name}` is where this session is — /worktree out of it first");
    }
    // Git answers from the repository, not from whichever checkout asked.
    let root = trees[0].path.clone();
    let target = tree.path.to_string_lossy().into_owned();
    let removed = git(&root, &["worktree", "remove", &target])?;
    if !removed.status.success() {
        bail!("{}", stderr_of(&removed));
    }
    // With the checkout gone the branch it held is free — unless it has no
    // ref (an orphan with no commit yet), which leaves nothing to delete.
    let mut branch = None;
    let mut note = None;
    if let Some(on) = &tree.branch
        && branch_exists(&root, on)?
    {
        let dropped = git(&root, &["branch", "-d", on])?;
        if dropped.status.success() {
            branch = Some(on.clone());
        } else {
            note = Some(format!("branch {on} was left — {}", stderr_of(&dropped)));
        }
    }
    Ok(Removed {
        path: tree.path.clone(),
        branch,
        note,
    })
}

impl Core {
    // Each tree keeps its own transcript: paths in it are workspace-relative
    // and locks are keyed by absolute path, so a shared one would misresolve.
    pub(super) fn enter_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane_mut().root().to_path_buf();
        let tree = enter(&from, name).map_err(|e| refused("worktree", e))?;
        // Built before the comparison: both sides are then canonical, and a
        // path git and the workspace spell differently is still one directory.
        let ws = tool::Workspace::new(&tree.path)
            .and_then(|ws| ws.with_write_roots(&self.config.writable()))
            .map_err(|e| refused("worktree", anyhow::anyhow!("{}: {e}", tree.path.display())))?;
        if ws.root() == from {
            return Ok(Step::Flash(format!("already in {}", tree.name)));
        }
        // Saved before the move, against the root it belongs to; an empty
        // session has nothing to keep, and a run in flight saves its own.
        if self.lane().session().is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "the leaving session was not saved");
        }
        // Already open: the lane that holds it comes back whole. Nothing is
        // said — the screen changing, bar included, says where you are.
        if let Some(i) = self.lanes.iter().position(|lane| lane.root() == ws.root()) {
            self.current = i;
            self.in_force();
            return Ok(Step::Handled(Listing::default()));
        }
        let said = self.open_lane(ws, (!tree.main).then(|| tree.name.clone()))?;
        Ok(Step::Swap(Listing::say(said)))
    }
    // Removes the checkout, its branch, and every transcript under it; a
    // dirty checkout's refusal from git is passed on, not forced past.
    pub(super) fn remove_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane().root().to_path_buf();
        if let Some(target) = list(&from)
            .ok()
            .and_then(|trees| trees.into_iter().find(|t| !t.main && t.name == name))
        {
            let running = self.lanes.iter().enumerate().any(|(i, lane)| {
                i != self.current && lane.root().starts_with(&target.path) && lane.is_running()
            });
            if running {
                return Err(format!(
                    "`{name}` is running in another lane of this run — stop it first"
                ));
            }
        }
        let removed = remove(&from, name).map_err(|e| refused("worktree", e))?;
        for i in (0..self.lanes.len()).rev() {
            if i != self.current && self.lanes[i].root().starts_with(&removed.path) {
                self.remove_lane(i);
            }
        }
        let dropped = self.store.drop_under(&removed.path);
        if let Some(jobs) = &self.jobs {
            jobs.drop_under(&removed.path);
        }
        let mut said = vec![
            format!("removed {name}"),
            removed.path.display().to_string(),
        ];
        if let Some(branch) = removed.branch {
            said.push(format!("branch {branch} deleted"));
        }
        if let Some(note) = removed.note {
            said.push(note);
        }
        if dropped > 0 {
            said.push(format!("{dropped} session record(s) dropped"));
        }
        Ok(Step::Worktrees(Listing::say(said)))
    }
    // The checkouts `/worktree` can move to, the repository's own first, the
    // one the session is in marked.
    pub(super) fn worktree_listing(&self) -> Vec<String> {
        let here = self.lane().root();
        let trees = match list(here) {
            Ok(t) => t,
            Err(e) => return vec![refused("worktree", e)],
        };
        // By containment rather than equality: a run started in a subdirectory
        // is still in that checkout, and it is the one to mark.
        let at = holding(&trees, here).map(|t| t.path.clone());
        let width = trees
            .iter()
            .map(|t| unicode_width::UnicodeWidthStr::width(t.name.as_str()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = trees
            .iter()
            .map(|t| {
                let mark = if at.as_ref() == Some(&t.path) {
                    "*"
                } else {
                    " "
                };
                let name = pi_store::text::pad(&t.name, width);
                let on = t
                    .branch_note()
                    .map(|on| format!("  {on}"))
                    .unwrap_or_default();
                format!("{mark} {name}{on}").trim_end().to_string()
            })
            .collect();
        let home = trees
            .first()
            .and_then(|main| home(&main.path).ok())
            .map_or_else(
                || "beside the repository".into(),
                |h| h.display().to_string(),
            );
        out.push(format!(
            "/worktree <name> works in one, creating it in {home}/ if it is not there"
        ));
        out.push(
            "/worktree rm <name> removes one — its checkout, sessions, and its branch once merged"
                .into(),
        );
        out
    }
}

// A repository one level inside a temp dir, so the worktrees kept beside it
// land inside that dir too and go with it.
#[cfg(test)]
pub(crate) struct TestRepo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

#[cfg(test)]
impl TestRepo {
    pub(crate) fn path(&self) -> &Path {
        &self.root
    }
}

#[cfg(test)]
fn test_repo() -> TestRepo {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    let at = root.as_path();
    checked(at, &["init", "-q", "--initial-branch=main", "."]).unwrap();
    checked(at, &["config", "user.email", "t@example.com"]).unwrap();
    checked(at, &["config", "user.name", "t"]).unwrap();
    std::fs::write(at.join("a.txt"), "hi").unwrap();
    checked(at, &["add", "-A"]).unwrap();
    checked(at, &["commit", "-qm", "init"]).unwrap();
    TestRepo { _dir: dir, root }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::{Recording, a_repl};

    fn repo() -> TestRepo {
        test_repo()
    }

    #[test]
    fn a_new_name_gets_a_branch_and_a_checkout() {
        let dir = repo();
        let tree = enter(dir.path(), "feature-one").unwrap();
        assert_eq!(tree.branch.as_deref(), Some("feature-one"));
        assert!(!tree.main);
        assert!(tree.path.join("a.txt").is_file());
        assert_eq!(tree.path, home(dir.path()).unwrap().join("feature-one"));
    }

    // Memory is keyed by this: a worktree answering for itself would get a
    // memory of its own, and nobody would notice the split.
    #[test]
    fn a_worktree_and_a_subdirectory_answer_with_the_main_checkout() {
        let dir = repo();
        let tree = enter(dir.path(), "feature-one").unwrap();
        std::fs::create_dir(tree.path.join("sub")).unwrap();
        assert_eq!(main_root(&tree.path.join("sub")), dir.path());
        assert_eq!(main_root(dir.path()), dir.path());
        let outside = tempfile::tempdir().unwrap();
        assert_eq!(main_root(outside.path()), outside.path());
    }

    #[test]
    fn entering_the_same_name_twice_goes_back_to_it() {
        let dir = repo();
        let first = enter(dir.path(), "feature-one").unwrap();
        let again = enter(dir.path(), "feature-one").unwrap();
        assert_eq!(first.path, again.path);
        assert_eq!(again.branch.as_deref(), Some("feature-one"));
    }

    #[test]
    fn an_existing_branch_is_checked_out_rather_than_recreated() {
        let dir = repo();
        checked(dir.path(), &["branch", "already"]).unwrap();
        let tree = enter(dir.path(), "already").unwrap();
        assert_eq!(tree.branch.as_deref(), Some("already"));
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
        let tree = enter(dir.path(), "feat/one").unwrap();
        assert_eq!(tree.path, home(dir.path()).unwrap().join("feat/one"));
        assert_eq!(tree.branch.as_deref(), Some("feat/one"));
        let trees = list(dir.path()).unwrap();
        assert!(trees.iter().any(|t| t.name == "feat/one"));
    }

    #[test]
    fn a_worktree_reaches_the_main_checkout_and_its_siblings() {
        let dir = repo();
        let inside = enter(dir.path(), "feature-one").unwrap();
        // From within a worktree the main checkout is still the first listed,
        // which is what lets `/worktree <repo>` get back out.
        let trees = list(&inside.path).unwrap();
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[0].path, dir.path().canonicalize().unwrap());
        assert!(trees[0].main);
    }

    // Deleted from the shell (not git), it stays registered as `prunable`
    // until `worktree prune` runs; listing it would offer a dead directory.
    #[test]
    fn a_checkout_deleted_behind_gits_back_stops_being_listed() {
        let dir = repo();
        enter(dir.path(), "fix-1").unwrap();
        let gone = enter(dir.path(), "fix-2").unwrap();
        assert_eq!(list(dir.path()).unwrap().len(), 3);

        std::fs::remove_dir_all(&gone.path).expect("the directory goes");
        let trees = list(dir.path()).unwrap();
        assert_eq!(
            trees.len(),
            2,
            "{:?}",
            trees.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(!trees.iter().any(|t| t.name == "fix-2"));
        // The one beside it is untouched, and the main checkout still leads.
        assert!(trees[0].main);
        assert!(trees.iter().any(|t| t.name == "fix-1"));
    }

    #[test]
    fn a_worktree_lives_beside_the_repository_not_inside_it() {
        let dir = repo();
        let tree = enter(dir.path(), "one").unwrap();
        assert!(
            !tree.path.starts_with(dir.path()),
            "{}",
            tree.path.display()
        );
        assert_eq!(
            tree.path.parent(),
            dir.path()
                .parent()
                .map(|p| p.join("repo.worktrees"))
                .as_deref()
        );
    }

    #[test]
    fn a_checkout_made_elsewhere_answers_to_its_directory_name() {
        // By hand, or where an older pi kept them: still reached by a word.
        let dir = repo();
        let old = dir.path().join(".worktrees/legacy");
        let target = old.to_string_lossy().into_owned();
        checked(dir.path(), &["worktree", "add", "-b", "legacy", &target]).unwrap();
        let tree = enter(dir.path(), "legacy").unwrap();
        assert_eq!(tree.path, old);
    }

    #[test]
    fn a_worktree_created_from_inside_another_is_its_sibling() {
        // Not nested under the one it was asked from: `home` hangs off the
        // repository, and asking from anywhere in it means the same place.
        let dir = repo();
        let first = enter(dir.path(), "one").unwrap();
        let second = enter(&first.path, "two").unwrap();
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
        std::fs::remove_dir_all(home(dir.path()).unwrap().join("one")).unwrap();
        checked(dir.path(), &["worktree", "prune"]).unwrap();
        checked(dir.path(), &["-C", "elsewhere", "checkout", "-q", "one"]).unwrap();
        assert!(enter(dir.path(), "one").is_err());
    }

    #[test]
    fn a_subdirectory_is_held_by_the_checkout_it_is_in() {
        let dir = repo();
        let tree = enter(dir.path(), "one").unwrap();
        let deep = tree.path.join("crates/pi");
        std::fs::create_dir_all(&deep).unwrap();
        let trees = list(dir.path()).unwrap();
        // Equality would miss: the run is below the checkout's root.
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
        let squatting = home(dir.path()).unwrap().join("taken");
        std::fs::create_dir_all(&squatting).unwrap();
        // A file inside makes it something git would have to displace; an
        // empty directory it would simply take over in place.
        std::fs::write(squatting.join("keep"), b"").unwrap();
        assert!(enter(dir.path(), "taken").is_err());
    }

    #[test]
    fn remove_deletes_the_checkout_and_the_branch_it_was_on() {
        let dir = repo();
        let tree = enter(dir.path(), "one").unwrap();
        assert!(tree.path.is_dir());
        assert!(branch_exists(dir.path(), "one").unwrap());

        let removed = remove(dir.path(), "one").unwrap();
        assert!(!removed.path.exists());
        assert_eq!(removed.branch.as_deref(), Some("one"));
        assert!(!branch_exists(dir.path(), "one").unwrap());
        // The main checkout is the only one left.
        let trees = list(dir.path()).unwrap();
        assert_eq!(trees.len(), 1);
        assert!(trees[0].main);
    }

    #[test]
    fn remove_a_nested_name_takes_that_checkout_only() {
        let dir = repo();
        enter(dir.path(), "feat/one").unwrap();
        enter(dir.path(), "keep").unwrap();
        let removed = remove(dir.path(), "feat/one").unwrap();
        assert_eq!(removed.path, home(dir.path()).unwrap().join("feat/one"));
        assert!(!removed.path.exists());
        let trees = list(dir.path()).unwrap();
        assert_eq!(trees.len(), 2, "the main checkout and the survivor");
        assert!(trees.iter().any(|t| t.name == "keep"));
        assert!(!trees.iter().any(|t| t.name == "feat/one"));
    }

    #[test]
    fn an_unmerged_branch_outlives_its_checkout() {
        // Its commits exist nowhere else: the checkout goes, the branch stays,
        // and the receipt gives git's reason and none of its advice.
        let dir = repo();
        let tree = enter(dir.path(), "one").unwrap();
        checked(&tree.path, &["commit", "-q", "--allow-empty", "-m", "work"]).unwrap();
        let removed = remove(dir.path(), "one").unwrap();
        assert!(!tree.path.exists());
        assert_eq!(removed.branch, None);
        assert!(branch_exists(dir.path(), "one").unwrap());
        let note = removed.note.expect("the receipt says why");
        assert!(note.contains("not fully merged"), "{note}");
        assert!(!note.contains("branch -D"), "no advice: {note}");
    }

    #[test]
    fn an_unborn_branch_leaves_nothing_to_delete_or_say() {
        // An orphan checkout names a branch that has no commit, so no ref.
        let dir = repo();
        let path = home(dir.path()).unwrap().join("two");
        let at = path.to_string_lossy().into_owned();
        checked(
            dir.path(),
            &["worktree", "add", "--orphan", "-b", "two", &at],
        )
        .unwrap();
        let removed = remove(dir.path(), "two").unwrap();
        assert!(!path.exists());
        assert_eq!(removed.branch, None);
        assert_eq!(removed.note, None);
    }

    #[test]
    fn a_checkout_with_changes_is_left_alone() {
        let dir = repo();
        let tree = enter(dir.path(), "one").unwrap();
        std::fs::write(tree.path.join("dirty.txt"), "uncommitted").unwrap();
        assert!(remove(dir.path(), "one").is_err());
        assert!(tree.path.is_dir());
        assert!(
            branch_exists(dir.path(), "one").unwrap(),
            "the branch stays with the tree"
        );
    }

    #[test]
    fn the_main_checkout_and_the_one_this_session_is_in_are_refused() {
        let dir = repo();
        let main = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(remove(dir.path(), &main).is_err());

        let inside = enter(dir.path(), "one").unwrap();
        assert!(remove(&inside.path, "one").is_err());
        assert!(inside.path.is_dir());
        // From the main checkout the same tree is removable again.
        remove(dir.path(), "one").unwrap();
    }

    #[test]
    fn a_detached_checkout_goes_without_a_branch() {
        let dir = repo();
        let target = home(dir.path()).unwrap().join("det");
        checked(
            dir.path(),
            &["worktree", "add", "--detach", &target.to_string_lossy()],
        )
        .unwrap();
        let removed = remove(dir.path(), "det").unwrap();
        assert_eq!(removed.branch, None);
        assert!(!removed.path.exists());
        // Removing it frees the name for a fresh worktree.
        let reborn = enter(dir.path(), "det").unwrap();
        assert_eq!(reborn.branch.as_deref(), Some("det"));
    }

    #[test]
    fn a_name_that_is_not_a_worktree_is_refused() {
        let dir = repo();
        assert!(remove(dir.path(), "nope").is_err());
        // An invalid name is refused the same way entering refuses it.
        assert!(remove(dir.path(), "../escape").is_err());
    }

    #[test]
    fn a_linked_tree_sharing_the_repos_name_wins_over_the_main_checkout() {
        // The main checkout answers to its directory name, so a linked tree
        // that chose the same word must still be removable by it.
        let dir = repo();
        let main = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let target = home(dir.path()).unwrap().join(&main);
        checked(
            dir.path(),
            &["worktree", "add", "-b", "twin", &target.to_string_lossy()],
        )
        .unwrap();

        let removed = remove(dir.path(), &main).unwrap();
        assert_eq!(removed.path, target);
        assert!(!removed.path.exists());
        assert!(!branch_exists(dir.path(), "twin").unwrap());
        let trees = list(dir.path()).unwrap();
        assert_eq!(trees.len(), 1);
        assert!(trees[0].main, "the main checkout survives");
    }

    #[test]
    fn remove_worktree_closes_idle_lane_in_same_run() {
        let dir = crate::core::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("fix-tools").unwrap();
        assert_eq!(core.lanes.len(), 2);
        assert_eq!(core.current, 1);

        core.current = 0;
        core.in_force();

        let res = core.remove_worktree("fix-tools");
        assert!(res.is_ok(), "remove_worktree failed: {res:?}");
        assert_eq!(core.lanes.len(), 1);
        assert_eq!(core.current, 0);
        assert!(!home(dir.path()).unwrap().join("fix-tools").exists());
    }

    #[test]
    fn remove_worktree_updates_current_index_when_earlier_lane_is_closed() {
        let dir = crate::core::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("feat-one").unwrap();
        core.enter_worktree("feat-two").unwrap();
        assert_eq!(core.lanes.len(), 3);
        assert_eq!(core.current, 2);

        let res = core.remove_worktree("feat-one");
        assert!(res.is_ok(), "remove_worktree failed: {res:?}");
        assert_eq!(core.lanes.len(), 2);
        assert_eq!(core.current, 1);
        assert_eq!(core.lane().worktree(), Some("feat-two"));
    }

    #[test]
    fn remove_worktree_refuses_when_another_lane_is_running() {
        let dir = crate::core::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("fix-tools").unwrap();
        assert_eq!(core.lanes.len(), 2);

        core.lanes[1].begin(tokio_util::sync::CancellationToken::new());

        core.current = 0;
        core.in_force();

        let err = core.remove_worktree("fix-tools").unwrap_err();
        assert!(err.contains("running in another lane"), "{err}");
        assert_eq!(core.lanes.len(), 2);
        assert!(home(dir.path()).unwrap().join("fix-tools").exists());
    }
}
