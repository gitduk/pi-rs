//! What a surface knows of a tool by its name, so it never names one itself.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use pi_store::config;
use tool::Tier;

/// Project directories before pi's own, each with whether it is the checkout's:
/// what a run reads comes from the checkout first.
pub fn project_first(project: Option<&PathBuf>, home: Option<PathBuf>) -> Vec<(PathBuf, bool)> {
    let project = project.map(|dir| (dir.clone(), true));
    project
        .into_iter()
        .chain(home.map(|dir| (dir, false)))
        .collect()
}

/// Whether two directories are the one directory, seen from different routes:
/// a home that is the checkout's own `.pi` would otherwise be read twice.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    a.canonicalize()
        .ok()
        .is_some_and(|a| b.canonicalize().ok() == Some(a))
}

/// The directories a named kind of file is read from — tools, skills — each
/// with whether it is the checkout's own.
pub fn dirs(workspace: &Path, name: &str) -> Vec<(PathBuf, bool)> {
    let home = pi_store::dir().map(|root| root.join(name));
    let project = config::project_dir(workspace, name)
        .filter(|dir| !home.as_deref().is_some_and(|home| same_dir(dir, home)));
    project_first(project.as_ref(), home)
}

/// Whether the tool `name` changes files. What it changed is the result, so
/// a surface shows it whole rather than in brief.
pub fn modifies(name: &str) -> bool {
    // Built-ins and memory only: no other source offers a tool at the write tier.
    static WRITERS: LazyLock<Vec<String>> = LazyLock::new(|| {
        let tools = pi_tools::builtin();
        tools
            .names()
            .into_iter()
            .filter(|n| tools.get(n).is_some_and(|t| t.tier() == Tier::Write))
            .chain([super::memory::Remember::NAME.to_string()])
            .collect()
    });
    WRITERS.iter().any(|w| w == name)
}

/// The lines of what the tool `name` printed that a screen shows.
pub fn shown<'a>(name: &str, output: &'a str) -> impl Iterator<Item = &'a str> {
    let bash = name == pi_tools::bash::Bash::NAME;
    output
        .lines()
        .filter(move |l| !(bash && super::bash::is_stream_tag(l)))
}

/// Run an explicit tool call and return its output to the caller.
pub async fn run_named(
    tool: std::sync::Arc<dyn tool::Tool>,
    args: serde_json::Value,
    ctx: &tool::Ctx,
) -> Option<Result<String, String>> {
    let said = tool.execute(args, ctx).await;
    // Only a call the stop cut short is silent: one that finished first
    // has done what it did, and says so.
    Some(match said {
        Err(tool::ToolError::Cancelled) => return None,
        Ok(out) => Ok(out.flatten()),
        Err(why) => Err(why.to_string()),
    })
}
