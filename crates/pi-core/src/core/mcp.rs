//! The MCP servers this process runs: one set, kept across reloads, and
//! started again only when `[mcp]` itself changed.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use pi_store::config::Config;
use pi_store::listing::{Listing, Row};

static RUNNING: Mutex<Option<Arc<mcp::Servers>>> = Mutex::new(None);

/// The tools of whichever servers are running when asked: a registry holds
/// this rather than a set, so a set replaced by [`sync`] stops at once.
pub struct Live;

impl tool::Source for Live {
    fn tools(&self) -> Vec<Arc<dyn tool::Tool>> {
        running().map_or_else(Vec::new, |s| tool::Source::tools(&*s))
    }
}

/// Run the servers `config` names, in the main checkout of `root`: the set
/// already up when it names the same, else a fresh start. Called only once
/// a config is in force, so a refused reload leaves the servers alone.
pub fn sync(config: &Config, root: &Path) {
    let mut running = RUNNING.lock().unwrap_or_else(PoisonError::into_inner);
    if config.mcp.is_empty() {
        *running = None;
        return;
    }
    let specs: Vec<(String, mcp::Spec)> = config
        .mcp
        .iter()
        .map(|(name, s)| {
            let spec = match (&s.command, &s.url) {
                (Some(command), _) => mcp::Spec::Stdio {
                    command: command.clone(),
                    args: s.args.clone(),
                    env: s.env(),
                },
                (None, url) => mcp::Spec::Http {
                    url: url.clone().unwrap_or_default(),
                    headers: s.headers(),
                },
            };
            (name.clone(), spec)
        })
        .collect();
    if running.as_ref().is_some_and(|s| s.runs(&specs)) {
        return;
    }
    // One set serves every lane, so it runs in the main checkout.
    let main = crate::core::worktree::main_root(root);
    *running = Some(Arc::new(mcp::Servers::start(specs, main)));
}

/// Wait, up to `within`, for the running servers to come up or fail.
pub async fn settled(within: std::time::Duration) {
    let running = RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    if let Some(servers) = running {
        servers.settled(within).await;
    }
}

fn running() -> Option<Arc<mcp::Servers>> {
    RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// `/mcp`: bare, every server with its tools; `restart [name]` connects one,
/// or all, again.
pub fn command(arg: &str) -> Listing {
    let Some(servers) = running() else {
        return Listing::say(["no MCP servers — add an [mcp.<name>] to ~/.pi/settings.toml"]);
    };
    match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => {
            let prompts = servers.prompts();
            let mut rows = Vec::new();
            for s in servers.shown() {
                // The model reads `<server>__` and `[server]` on each tool;
                // here the server heads its own rows, so neither is repeated.
                let tag = format!("[{}] ", s.name);
                let prefix = format!("{}__", s.name);
                rows.push(Row::new([s.name.clone(), s.status]));
                for (name, description) in s.tools {
                    let name = name.strip_prefix(&prefix).unwrap_or(&name);
                    let first = description.lines().next().unwrap_or_default();
                    let first = first.strip_prefix(&tag).unwrap_or(first);
                    rows.push(Row::new([format!("  {name}"), first.to_string()]));
                }
                for p in prompts.iter().filter(|p| p.server == s.name) {
                    let first = p.description.lines().next().unwrap_or_default();
                    rows.push(Row::new([format!("  {}", p.word()), first.to_string()]));
                }
            }
            Listing::of(rows)
        }
        ["restart", rest @ ..] if rest.len() <= 1 => match servers.restart(rest.first().copied()) {
            Ok(1) if !rest.is_empty() => Listing::say([format!("{} restarting", rest[0])]),
            Ok(n) => Listing::say([format!("{n} MCP servers restarting")]),
            Err(e) => Listing::say([e]),
        },
        _ => Listing::say(["/mcp lists the servers; /mcp restart [name] reconnects one, or all"]),
    }
}

/// Every prompt the running servers offer.
pub fn prompts() -> Vec<mcp::Prompt> {
    running().map_or_else(Vec::new, |s| s.prompts())
}

/// The prompt `word` (`/<server>:<name>`) runs, if a running server has it.
pub fn prompt(word: &str) -> Option<mcp::Prompt> {
    prompts().into_iter().find(|p| p.word() == word)
}

/// A prompt's text, asked of its server while the caller waits: a typed
/// command has nothing to send until it comes back.
pub fn fetch(prompt: &mcp::Prompt, args: &str) -> Result<String, String> {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    let handle = tokio::runtime::Handle::try_current()
        .ok()
        .filter(|h| h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .ok_or("an MCP prompt needs pi's own runtime")?;
    tokio::task::block_in_place(|| {
        handle.block_on(async {
            tokio::time::timeout(WAIT, prompt.text(args))
                .await
                .unwrap_or_else(|_| Err(format!("{} did not answer in 10s", prompt.word())))
        })
    })
}

/// What each running server is doing, for `/status`.
pub fn summary() -> Vec<String> {
    RUNNING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|s| s.summary())
        .unwrap_or_default()
}
