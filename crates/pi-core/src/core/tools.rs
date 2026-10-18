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

/// Run `tool` as the user asked for it by name, with no model in between:
/// what it answered, or why it did not. Free of `Core`, as `run_bash` is.
///
/// Not a model's call, so `agent`'s journal never sees it; it is kept here.
pub async fn run_named(
    tool: std::sync::Arc<dyn tool::Tool>,
    args: serde_json::Value,
    ctx: &tool::Ctx,
) -> Result<String, String> {
    let name = tool.name().to_string();
    tracing::info!(target: "pi::tool", tool = %name, args = %args, "call by the user");
    match tool.execute(args, ctx).await {
        Ok(out) => {
            tracing::info!(target: "pi::tool", tool = %name, preview = %out.preview(), "ok");
            Ok(out.flatten())
        }
        Err(why) => {
            tracing::warn!(target: "pi::tool", tool = %name, detail = %why, "failed");
            Err(why.to_string())
        }
    }
}
