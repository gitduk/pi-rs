//! MCP servers owned by one workspace, retained through configuration reloads.

use crate::input::commands::gist;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pi_store::config::Config;
use pi_store::listing::{Listing, Row};

use crate::input::Step;

/// A live workspace source shared by its registry and configuration owner.
#[derive(Clone, Default)]
pub struct Live {
    inner: Arc<Mutex<Current>>,
}

#[derive(Default)]
struct Current {
    running: Option<Running>,
    generation: u64,
}

struct Running {
    servers: Arc<mcp::Servers>,
    root: PathBuf,
    seen: u64,
}

impl Running {
    fn new(servers: Arc<mcp::Servers>, root: PathBuf) -> Self {
        let seen = servers.generation();
        Self {
            servers,
            root,
            seen,
        }
    }
}

impl Current {
    fn replace(&mut self, running: Option<Running>) -> Option<Running> {
        self.generation += 1;
        std::mem::replace(&mut self.running, running)
    }

    fn generation(&mut self) -> u64 {
        if let Some(running) = &mut self.running {
            let now = running.servers.generation();
            if now != running.seen {
                running.seen = now;
                self.generation += 1;
            }
        }
        self.generation
    }
}

impl tool::Source for Live {
    fn tools(&self) -> Vec<Arc<dyn tool::Tool>> {
        self.running().map_or_else(Vec::new, |servers| {
            tool::Source::tools(&*servers)
                .into_iter()
                .map(|tool| {
                    Arc::new(OwnedTool {
                        tool,
                        _servers: servers.clone(),
                    }) as Arc<dyn tool::Tool>
                })
                .collect()
        })
    }
}

impl Live {
    /// Apply the workspace's configured servers, retaining unchanged connections.
    pub fn sync(&self, config: &Config, root: &Path) {
        let specs = specifications(config);
        let old = {
            let mut current = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if specs.is_empty() {
                if current.running.is_none() {
                    return;
                }
                current.replace(None)
            } else {
                if current
                    .running
                    .as_ref()
                    .is_some_and(|running| running.root == root && running.servers.runs(&specs))
                {
                    return;
                }
                let servers = Arc::new(mcp::Servers::start(specs, root.to_path_buf()));
                current.replace(Some(Running::new(servers, root.to_path_buf())))
            }
        };
        // A selected capability retains its connection set until that call lets go.
        drop(old);
    }

    /// The workspace revision of its connection set and advertised lists.
    pub fn generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .generation()
    }

    /// Wait, up to `within`, for the running servers to come up or fail.
    pub async fn settled(&self, within: std::time::Duration) {
        if let Some(servers) = self.running() {
            servers.settled(within).await;
        }
    }

    fn running(&self) -> Option<Arc<mcp::Servers>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running
            .as_ref()
            .map(|running| running.servers.clone())
    }

    /// `/mcp`: bare, every server with its tools; `restart [name]` connects one,
    /// or all, again.
    pub fn command(&self, arg: &str) -> Step {
        let Some(servers) = self.running() else {
            // Asked for, it says how to add one; refused, only that there is none.
            return match arg.trim() {
                "" => Step::Handled(Listing::say([
                    "no MCP servers — add an [mcp.<name>] to ~/.pi/settings.toml",
                ])),
                _ => Step::Notice("no MCP servers".into()),
            };
        };
        match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => Step::Handled(describe(&servers)),
            ["restart", rest @ ..] if rest.len() <= 1 => {
                Step::Notice(match servers.restart(rest.first().copied()) {
                    Ok(1) if !rest.is_empty() => format!("{} restarting", rest[0]),
                    Ok(n) => format!("{n} MCP servers restarting"),
                    Err(e) => e,
                })
            }
            _ => Step::Handled(Listing::say([
                "/mcp lists the servers; /mcp restart [name] reconnects one, or all",
            ])),
        }
    }

    /// Every prompt the running servers offer.
    pub fn prompts(&self) -> Vec<mcp::Prompt> {
        self.running().map_or_else(Vec::new, |s| s.prompts())
    }

    /// Names accepted by the running set, including servers that are starting
    /// or failed. Requested only while a server-name argument is being edited.
    pub fn choices(&self) -> Vec<crate::input::commands::Choice> {
        self.running().map_or_else(Vec::new, |s| {
            s.names()
                .map(|name| crate::input::commands::Choice {
                    name: name.to_string(),
                    note: String::new(),
                })
                .collect()
        })
    }

    /// What each running server is doing, for `/status`.
    pub fn summary(&self) -> Vec<String> {
        self.running().map_or_else(Vec::new, |s| s.summary())
    }
}

fn describe(servers: &mcp::Servers) -> Listing {
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
            let first = description.strip_prefix(&tag).unwrap_or(&description);
            rows.push(Row::new([format!("  {name}"), gist(first)]));
        }
        for p in prompts.iter().filter(|p| p.server == s.name) {
            rows.push(Row::new([format!("  {}", p.word()), gist(&p.description)]));
        }
    }
    Listing::of(rows)
}

fn specifications(config: &Config) -> Vec<(String, mcp::Spec)> {
    config
        .mcp
        .iter()
        .map(|(name, server)| {
            let spec = match (&server.command, &server.url) {
                (Some(command), _) => mcp::Spec::Stdio {
                    command: command.clone(),
                    args: server.args.clone(),
                    env: server.env(),
                },
                (None, url) => mcp::Spec::Http {
                    url: url.clone().unwrap_or_default(),
                    headers: server.headers(),
                },
            };
            (name.clone(), spec)
        })
        .collect()
}

struct OwnedTool {
    tool: Arc<dyn tool::Tool>,
    _servers: Arc<mcp::Servers>,
}

#[async_trait::async_trait]
impl tool::Tool for OwnedTool {
    fn name(&self) -> &str {
        self.tool.name()
    }
    fn description(&self) -> &str {
        self.tool.description()
    }
    fn schema(&self) -> serde_json::Value {
        self.tool.schema()
    }
    fn tier(&self) -> tool::Tier {
        self.tool.tier()
    }
    fn model_request(
        &self,
        args: &serde_json::Value,
    ) -> (message::ModelRequest, Vec<message::Block>) {
        self.tool.model_request(args)
    }
    fn concurrency(&self) -> tool::Concurrency {
        self.tool.concurrency()
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &tool::Ctx,
    ) -> Result<tool::ToolOutput, tool::ToolError> {
        self.tool.execute(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_one_workspace_and_other_workspaces_remain_independent() {
        let first = Live::default();
        let clone = first.clone();
        let second = Live::default();
        let servers = Arc::new(mcp::Servers::start(Vec::new(), PathBuf::from("/first")));
        first
            .inner
            .lock()
            .unwrap()
            .replace(Some(Running::new(servers.clone(), "/first".into())));
        assert!(Arc::ptr_eq(&clone.running().unwrap(), &servers));
        assert!(second.running().is_none());
        assert_eq!(first.generation(), clone.generation());
        assert_eq!(second.generation(), 0);
        let before = first.generation();
        clone.sync(&Config::default(), Path::new("/first"));
        assert!(first.running().is_none());
        assert!(first.generation() > before);
        assert_eq!(second.generation(), 0);
        let cleared = first.generation();
        clone.sync(&Config::default(), Path::new("/first"));
        assert_eq!(
            first.generation(),
            cleared,
            "an unchanged empty set has no new revision"
        );
    }

    #[test]
    fn configuration_specs_are_pure_and_keep_workspace_launch_settings() {
        let config = Config::in_force(toml::from_str(
            "[mcp.local]\ncommand='fixture'\nargs=['arg']\n[mcp.remote]\nurl='https://fixture.invalid/mcp'\n",
        ).unwrap()).unwrap();
        let specs = specifications(&config);
        assert!(specs.iter().any(|(name, spec)| name == "local"
            && matches!(spec,
            mcp::Spec::Stdio { command, args, .. } if command == "fixture" && args == &["arg"])));
        assert!(specs.iter().any(|(name, spec)| name == "remote"
            && matches!(spec,
            mcp::Spec::Http { url, .. } if url == "https://fixture.invalid/mcp")));
    }

    #[test]
    fn a_selected_tool_keeps_its_connection_owner_after_reload() {
        let live = Live::default();
        let servers = Arc::new(mcp::Servers::start(Vec::new(), PathBuf::from("/workspace")));
        let weak = Arc::downgrade(&servers);
        live.inner
            .lock()
            .unwrap()
            .replace(Some(Running::new(servers.clone(), "/workspace".into())));
        let call = OwnedTool {
            tool: Arc::new(pi_tools::read::Read),
            _servers: servers,
        };
        live.sync(&Config::default(), Path::new("/workspace"));
        assert!(live.running().is_none());
        assert!(
            weak.upgrade().is_some(),
            "the selected call still owns its connections"
        );
        assert_eq!(tool::Tool::name(&call), "read");
        drop(call);
        assert!(
            weak.upgrade().is_none(),
            "the retired set closes after the call releases it"
        );
    }
}
