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

// How long a call runs before it is listed as a job. One that answers
// sooner never shows: no row flickers by, no job number is spent.
const LISTED_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

/// Run `tool` as the user asked for it by name, with no model in between:
/// what it answered, or why it did not; `None` once stopped as a job, which
/// it is once it outlasts `LISTED_AFTER` where `jobs` keeps them. Free of
/// `Core`, as `run_bash` is.
///
/// Not a model's call, so `agent`'s journal never sees it; it is kept here.
pub async fn run_named(
    tool: std::sync::Arc<dyn tool::Tool>,
    args: serde_json::Value,
    ctx: &tool::Ctx,
    jobs: Option<&dyn toolbox::background::JobSink>,
) -> Option<Result<String, String>> {
    let name = tool.name().to_string();
    tracing::info!(target: "pi::tool", tool = %name, args = %args, "call by the user");
    let label = called(&name, &args, ctx.workspace.root());
    let call = tool.execute(args, ctx);
    tokio::pin!(call);
    let said = tokio::select! {
        said = &mut call => said,
        () = tokio::time::sleep(LISTED_AFTER) => {
            let job = jobs.map(|sink| {
                let root = ctx.workspace.root().to_path_buf();
                sink.start(root, label, ctx.cancel.clone(), None)
            });
            let said = call.await;
            if let Some(job) = job {
                job.end();
            }
            said
        }
    };
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

// A call as `/tool` takes it: the name, then `key=value` for each argument
// but a path that is just the checkout the job is listed under.
fn called(name: &str, args: &serde_json::Value, root: &std::path::Path) -> String {
    let pairs = (args.as_object().into_iter().flatten()).filter_map(|(key, value)| match value {
        serde_json::Value::String(s) if std::path::Path::new(s) == root => None,
        serde_json::Value::String(s) => Some(format!(" {key}={s}")),
        other => Some(format!(" {key}={other}")),
    });
    std::iter::once(name.to_string()).chain(pairs).collect()
}
