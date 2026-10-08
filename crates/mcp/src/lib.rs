//! MCP servers as a source of tools: each server's tools, offered as
//! `<server>__<tool>` while its connection is up.
//!
//! A server starts in the background; its tools join the set once it has
//! listed them, and leave when it stops. One that cannot start, or stopped,
//! stands in the set as a single tool whose description says why.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, ClientConfig, Implementation, ProtocolVersion};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt, NotificationContext, Peer, RoleClient};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// How a server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spec {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// The servers of one config, running.
pub struct Servers {
    servers: Vec<Arc<Server>>,
}

struct Server {
    name: String,
    spec: Spec,
    cwd: PathBuf,
    state: Mutex<State>,
    // Which connection may write `state`: a restart moves it on, so the one
    // it replaced cannot land a late word over the new one's.
    epoch: AtomicU64,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}

/// One server as `/mcp` shows it.
pub struct Shown {
    pub name: String,
    /// `starting`, `N tools` or `down: why`.
    pub status: String,
    /// Each tool's name as offered, and its description.
    pub tools: Vec<(String, String)>,
}

enum State {
    Starting,
    Up(Vec<Arc<dyn Tool>>),
    Down(String),
}

impl Servers {
    /// Start every server in the background, stdio ones in `cwd`.
    pub fn start(specs: Vec<(String, Spec)>, cwd: PathBuf) -> Self {
        let servers = specs
            .into_iter()
            .map(|(name, spec)| {
                let server = Arc::new(Server {
                    name,
                    spec,
                    cwd: cwd.clone(),
                    state: Mutex::new(State::Starting),
                    epoch: AtomicU64::new(0),
                    task: Mutex::new(None),
                });
                connect(&server);
                server
            })
            .collect();
        Self { servers }
    }

    /// Drop the connection to `name`, or to every server when `None`, and
    /// connect again. Says how many it restarted, or that `name` is unknown.
    pub fn restart(&self, name: Option<&str>) -> Result<usize, String> {
        let chosen: Vec<_> = self
            .servers
            .iter()
            .filter(|s| name.is_none_or(|n| s.name == n))
            .collect();
        if chosen.is_empty() {
            return Err(format!("no MCP server `{}`", name.unwrap_or_default()));
        }
        for server in &chosen {
            if let Some(task) = lock(&server.task).take() {
                task.abort();
            }
            {
                let mut state = lock(&server.state);
                server.epoch.fetch_add(1, Ordering::SeqCst);
                *state = State::Starting;
            }
            connect(server);
        }
        Ok(chosen.len())
    }

    /// Every server, its status and the tools it offers.
    pub fn shown(&self) -> Vec<Shown> {
        self.servers
            .iter()
            .map(|s| {
                let (status, tools) = match &*lock(&s.state) {
                    State::Starting => ("starting".to_string(), Vec::new()),
                    State::Up(tools) => (
                        format!("{} tools", tools.len()),
                        tools
                            .iter()
                            .map(|t| (t.name().to_string(), t.description().to_string()))
                            .collect(),
                    ),
                    State::Down(why) => (format!("down: {why}"), Vec::new()),
                };
                Shown {
                    name: s.name.clone(),
                    status,
                    tools,
                }
            })
            .collect()
    }

    /// Whether these run exactly `specs`: the same names reached the same way.
    pub fn runs(&self, specs: &[(String, Spec)]) -> bool {
        self.servers.len() == specs.len()
            && self
                .servers
                .iter()
                .zip(specs)
                .all(|(s, (name, spec))| s.name == *name && s.spec == *spec)
    }

    /// Wait, up to `within`, for every server to be up or down: a one-shot run
    /// asks once, and a server still starting would have no tools to offer it.
    pub async fn settled(&self, within: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + within;
        while tokio::time::Instant::now() < deadline
            && self
                .servers
                .iter()
                .any(|s| matches!(*lock(&s.state), State::Starting))
        {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// One line per server, for `/status`.
    pub fn summary(&self) -> Vec<String> {
        self.shown()
            .into_iter()
            .map(|s| format!("{} {}", s.name, s.status))
            .collect()
    }
}

impl Drop for Servers {
    // A dropped connection stops its server: a stdio child is killed with it.
    fn drop(&mut self) {
        for server in &self.servers {
            if let Some(task) = lock(&server.task).take() {
                task.abort();
            }
        }
    }
}

impl tool::Source for Servers {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut out = Vec::new();
        for server in &self.servers {
            match &*lock(&server.state) {
                State::Starting => {}
                State::Up(tools) => out.extend(tools.iter().cloned()),
                State::Down(why) => out.push(Arc::new(Down {
                    name: tool_name(&server.name, "unavailable"),
                    description: format!(
                        "MCP server `{}` is not running: {why}. Its tools come back once \
                         [mcp.{}] in ~/.pi/settings.toml starts; calling this only says so.",
                        server.name, server.name
                    ),
                })),
            }
        }
        out
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn connect(server: &Arc<Server>) {
    let epoch = server.epoch.load(Ordering::SeqCst);
    let task = tokio::spawn(run(server.clone(), epoch));
    *lock(&server.task) = Some(task.abort_handle());
}

// Write `state` only while `epoch` is still the server's connection.
fn set(server: &Server, epoch: u64, state: State) {
    let mut now = lock(&server.state);
    if server.epoch.load(Ordering::SeqCst) == epoch {
        *now = state;
    }
}

// Connect, list, then wait for the connection to end, saying why it did.
async fn run(server: Arc<Server>, epoch: u64) {
    let handler = Handler {
        server: Arc::downgrade(&server),
        epoch,
    };
    let lifecycle = ClientLifecycleMode::Auto {
        preferred_versions: vec![ProtocolVersion::LATEST],
        legacy_version: Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
    };
    let (running, stderr) = match &server.spec {
        Spec::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .envs(env.iter().cloned())
                .current_dir(&server.cwd);
            let spawned = TokioChildProcess::builder(cmd)
                .stderr(std::process::Stdio::piped())
                .spawn();
            let (transport, stderr) = match spawned {
                Ok(spawned) => spawned,
                Err(e) => return down(&server, epoch, format!("`{command}` did not start: {e}")),
            };
            let stderr = stderr.map(|s| tokio::spawn(tail(s, server.name.clone())));
            (
                handler.serve_with_lifecycle(transport, lifecycle).await,
                stderr,
            )
        }
        Spec::Http { url, headers } => {
            // The crypto reqwest uses elsewhere in pi; rmcp's own would build
            // aws-lc. Installing twice is a no-op.
            let _ = rustls::crypto::ring::default_provider().install_default();
            let mut config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.as_str(),
                );
            let mut map = std::collections::HashMap::new();
            for (k, v) in headers {
                match (
                    http::HeaderName::try_from(k.as_str()),
                    http::HeaderValue::try_from(v.as_str()),
                ) {
                    (Ok(k), Ok(v)) => {
                        map.insert(k, v);
                    }
                    _ => {
                        return down(
                            &server,
                            epoch,
                            format!("header `{k}` is not a valid header"),
                        );
                    }
                }
            }
            config = config.custom_headers(map);
            let transport = StreamableHttpClientTransport::from_config(config);
            (
                handler.serve_with_lifecycle(transport, lifecycle).await,
                None,
            )
        }
    };
    let running = match running {
        Ok(running) => running,
        Err(e) => {
            let said = match stderr {
                Some(t) => t.await.unwrap_or_default(),
                None => String::new(),
            };
            return down(&server, epoch, with_stderr(e.to_string(), &said));
        }
    };
    list(&server, epoch, running.peer()).await;
    let ended = match running.waiting().await {
        Ok(reason) => format!("stopped ({reason:?})"),
        Err(e) => format!("stopped: {e}"),
    };
    down(&server, epoch, ended);
}

// The server's tools, read again: at start, and whenever it says they changed.
async fn list(server: &Arc<Server>, epoch: u64, peer: &Peer<RoleClient>) {
    match peer.list_all_tools().await {
        Ok(listed) => {
            let tools = listed
                .into_iter()
                .map(|t| {
                    Arc::new(Remote {
                        name: tool_name(&server.name, &t.name),
                        remote: t.name.to_string(),
                        description: format!(
                            "[{}] {}",
                            server.name,
                            t.description.as_deref().unwrap_or("")
                        ),
                        schema: Value::Object((*t.input_schema).clone()),
                        peer: peer.clone(),
                    }) as Arc<dyn Tool>
                })
                .collect();
            set(server, epoch, State::Up(tools));
        }
        Err(e) => down(server, epoch, format!("could not list its tools: {e}")),
    }
}

fn down(server: &Server, epoch: u64, why: String) {
    tracing::warn!(target: "pi::mcp", server = %server.name, %why, "server down");
    set(server, epoch, State::Down(why));
}

fn with_stderr(why: String, said: &str) -> String {
    let said = said.trim();
    if said.is_empty() {
        why
    } else {
        format!("{why}; it said: {said}")
    }
}

// Read a server's stderr to its end: every line to the journal, the last few
// hundred bytes kept to say why it would not start.
async fn tail(mut stderr: tokio::process::ChildStderr, name: String) -> String {
    const KEEP: usize = 512;
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        tracing::debug!(target: "pi::mcp", server = %name, said = %String::from_utf8_lossy(&chunk[..n]));
        kept.extend_from_slice(&chunk[..n]);
        if kept.len() > 2 * KEEP {
            kept.drain(..kept.len() - KEEP);
        }
    }
    let text = String::from_utf8_lossy(&kept);
    llm::slice::tail_bytes(&text, KEEP).to_string()
}

/// `<server>__<tool>`, in the characters every provider takes, within 64.
fn tool_name(server: &str, tool: &str) -> String {
    format!("{server}__{tool}")
        .chars()
        .map(|c| if tool::name_char(c) { c } else { '_' })
        .take(64)
        .collect()
}

// Hears a server say its tools changed.
#[derive(Clone)]
struct Handler {
    server: Weak<Server>,
    epoch: u64,
}

impl rmcp::ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            Default::default(),
            Implementation::new("pi", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, context: NotificationContext<RoleClient>) {
        if let Some(server) = self.server.upgrade() {
            list(&server, self.epoch, &context.peer).await;
        }
    }
}

// One tool of a running server.
struct Remote {
    name: String,
    remote: String,
    description: String,
    schema: Value,
    peer: Peer<RoleClient>,
}

#[async_trait]
impl Tool for Remote {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    // What a server's tool does is the server's to say, and nothing checks it.
    fn tier(&self) -> Tier {
        Tier::Exec
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let mut params = CallToolRequestParams::new(self.remote.clone());
        if let Value::Object(map) = args {
            params = params.with_arguments(map);
        }
        let result = tokio::select! {
            r = self.peer.call_tool(params) => r,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        }
        .map_err(|e| ToolError::Invalid(format!("{}: {e}", self.name)))?;
        let content = serde_json::to_value(&result.content).unwrap_or_default();
        let mut out = blocks(&content);
        if out.is_empty()
            && let Some(structured) = result.structured_content
        {
            out.push(llm::message::ToolResultContent::Json { value: structured });
        }
        if result.is_error == Some(true) {
            return Err(ToolError::Invalid(flatten(&out)));
        }
        if out.is_empty() {
            out.push(text(format!("{}: no output", self.name)));
        }
        Ok(ToolOutput {
            content: out,
            preview: None,
            spent: Default::default(),
        })
    }
}

// MCP content blocks as a tool result: text and images as they are,
// anything else (resources, audio) as the JSON it came as.
fn blocks(content: &Value) -> Vec<llm::message::ToolResultContent> {
    use llm::message::{Image, ToolResultContent as C};
    let Some(items) = content.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| {
            let field = |k: &str| item.get(k).and_then(Value::as_str);
            match field("type") {
                Some("text") => text(field("text").unwrap_or_default().to_string()),
                Some("image") => match (field("data"), field("mimeType")) {
                    (Some(data), Some(mime)) => C::Image(Image::Base64 {
                        media_type: mime.to_string(),
                        data: data.to_string(),
                    }),
                    _ => C::Json {
                        value: item.clone(),
                    },
                },
                _ => C::Json {
                    value: item.clone(),
                },
            }
        })
        .collect()
}

fn text(body: String) -> llm::message::ToolResultContent {
    llm::message::ToolResultContent::Text(llm::message::Text { text: body })
}

fn flatten(out: &[llm::message::ToolResultContent]) -> String {
    out.iter()
        .map(|c| match c {
            llm::message::ToolResultContent::Text(t) => t.text.clone(),
            llm::message::ToolResultContent::Json { value } => value.to_string(),
            llm::message::ToolResultContent::Image(_) => "[image]".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// What stands for a server that is not running.
struct Down {
    name: String,
    description: String,
}

#[async_trait]
impl Tool for Down {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    fn schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _: Value, _: &Ctx) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Invalid(self.description.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_name_is_one_providers_take() {
        assert_eq!(tool_name("github", "create_issue"), "github__create_issue");
        assert_eq!(tool_name("fs", "read.file"), "fs__read_file");
        assert_eq!(tool_name("a", &"x".repeat(100)).len(), 64);
    }

    #[test]
    fn content_blocks_keep_text_and_images() {
        let content = serde_json::json!([
            { "type": "text", "text": "hello" },
            { "type": "image", "data": "aGk=", "mimeType": "image/png" },
            { "type": "resource_link", "uri": "file:///x" },
        ]);
        let out = blocks(&content);
        assert_eq!(out.len(), 3);
        assert_eq!(flatten(&out[..1]), "hello");
        assert!(matches!(&out[1], llm::message::ToolResultContent::Image(_)));
        assert!(flatten(&out[2..]).contains("file:///x"));
    }
}
