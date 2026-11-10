//! Optional shell controls and restart data for execution hosts.

use std::path::PathBuf;
use std::time::Duration;

/// A shell launch specification retained independently of in-memory callbacks.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Again {
    pub command: String,
    pub cwd: PathBuf,
    pub stdin: Option<String>,
    pub environment: Environment,
    pub output: OutputMode,
    /// The runtime limit a background run and a restart keep; `None` leaves
    /// them untimed.
    #[serde(default)]
    pub timeout: Option<Duration>,
}

/// Per-command script settings; environment-file values stay out of saved state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Environment {
    pub directory: Option<PathBuf>,
    pub arguments: Vec<(String, String)>,
}

/// Which captured streams accompany a successful command result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputMode {
    #[default]
    All,
    Stdout,
}

/// Host-owned controls exposed through a shell. These add argument fields
/// and handle control requests; the shell remains the registered tool.
#[async_trait::async_trait]
pub trait Controls: Send + Sync {
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Map<String, serde_json::Value>;
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &tool::Ctx,
    ) -> Result<tool::ToolOutput, tool::ToolError>;
}
