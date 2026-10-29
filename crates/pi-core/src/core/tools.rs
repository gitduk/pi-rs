//! What a surface knows of a tool by its name, so it never names one itself.

use std::sync::LazyLock;

use tool::Tier;

/// Whether the tool `name` changes files. What it changed is the result, so
/// a surface shows it whole rather than in brief.
pub fn modifies(name: &str) -> bool {
    // Built-ins and memory only: no other source offers a tool at the write tier.
    static WRITERS: LazyLock<Vec<String>> = LazyLock::new(|| {
        let tools = toolbox::builtin();
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
    let bash = name == toolbox::bash::Bash::NAME;
    output
        .lines()
        .filter(move |l| !(bash && super::bash::is_stream_tag(l)))
}

/// Run a tool explicitly, recording the call and its outcome once.
pub async fn run_named(
    tool: std::sync::Arc<dyn tool::Tool>,
    args: serde_json::Value,
    ctx: &tool::Ctx,
) -> Option<Result<String, String>> {
    let name = tool.name().to_string();
    tracing::info!(target: "pi::tool", tool = %name, args = %args, "call by the user");
    let said = tool.execute(args, ctx).await;
    // Only a call the stop cut short is silent: one that finished first
    // has done what it did, and says so.
    Some(match said {
        Err(tool::ToolError::Cancelled) => {
            tracing::info!(target: "pi::tool", tool = %name, "stopped");
            return None;
        }
        Ok(out) => {
            tracing::info!(target: "pi::tool", tool = %name, preview = %out.preview(), "ok");
            Ok(out.flatten())
        }
        Err(why) => {
            tracing::warn!(target: "pi::tool", tool = %name, detail = %why, "failed");
            Err(why.to_string())
        }
    })
}
