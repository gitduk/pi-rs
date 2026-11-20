use crate::runtime::background::RunHandle;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::process::Command;

use crate::runtime::background::{
    Again, Controls, Environment, Invocation, OutputMode, Rerun, RunSink,
};

use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    #[serde(default = "workspace_root")]
    cwd: String,
    #[serde(default)]
    stdin: Option<String>,
    // Bounds a foreground call by default and a background one only when set;
    // zero reads as unset.
    #[serde(default)]
    timeout_ms: Option<u64>,
    // Not offered in the schema; accepted because models trained on other
    // shells send it, and shown in place of the command when they do.
    #[serde(default)]
    description: Option<String>,
}

impl Args {
    fn label(&self) -> Option<&str> {
        self.description
            .as_deref()
            .map(first_line)
            .map(str::trim)
            .filter(|label| !label.is_empty())
    }

    // The limit the call named, already clamped: `None` leaves a background
    // command untimed while a foreground call falls back to the default.
    fn named_timeout(&self) -> Option<std::time::Duration> {
        self.timeout_ms
            .filter(|ms| *ms > 0)
            .map(|ms| std::time::Duration::from_millis(ms).min(tool::limit::MAX_CALL))
    }
}

// Optional: a required cwd was still left out, and each miss cost a turn.
fn workspace_root() -> String {
    ".".to_string()
}

/// The shell every command runs under as `SHELL -c command`.
pub const SHELL: &str = "bash";

const ARGS_HINT: &str = "`bash` requires `command`. Use optional `cwd` for the working directory. Use optional `stdin` for input text.";

const DESCRIPTION: &str = "When the call passes `cwd`, the command runs in that directory; \
    otherwise it runs in the workspace root.\n\n\
    Use `bash` for substitutions by pattern or position (`sed`, `perl`, `awk`). \
    Use `bash` to move files (`mv`) or delete files (`rm`).";

// The default and the cap, said once for the description and the schema field.
fn foreground_limit() -> String {
    format!(
        "A foreground command stops after {} unless the call sets `timeout_ms`, \
         which may name up to {}.",
        llm::figures::elapsed(tool::limit::DEFAULT_CALL),
        llm::figures::elapsed(tool::limit::MAX_CALL),
    )
}

fn background_limit() -> &'static str {
    "A background command has no deadline unless the call sets `timeout_ms`, \
     which then stops it the same way."
}

/// Run a written command to completion under the workspace's checks and rewrite.
pub async fn foreground(command: &str, cwd: &str, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
    Ok(foreground_result(command, cwd, ctx).await?.into_output())
}

pub struct Foreground {
    pub completion: Completion,
    body: String,
    /// What the command printed, tags left out: the rows a screen shows.
    output: Vec<String>,
    screen: String,
    changes: Option<crate::drift::Report>,
}

/// Whether the command exited or was handed to its background runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Exited(i32),
    Background { id: u64 },
}

impl Foreground {
    pub fn content(&self) -> String {
        let mut body = self.body.clone();
        if let Some(changes) = &self.changes {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&changes.content);
        }
        body
    }

    pub fn into_output(self) -> ToolOutput {
        let Foreground {
            completion,
            body,
            output,
            screen,
            changes,
        } = self;
        let out = match completion {
            Completion::Exited(code) => finished(code, body, screen),
            Completion::Background { .. } => ToolOutput::text(body).with_screen(screen),
        };
        match changes {
            Some(changes) => changes.append(out, &output),
            None => out,
        }
    }
}

/// The command's content and completion, before exit diagnostics are added to
/// the tool result. Workspace change reports remain part of the content.
pub async fn foreground_result(
    command: &str,
    cwd: &str,
    ctx: &Ctx,
) -> Result<Foreground, ToolError> {
    Bash::foreground()
        .foreground_result(command, cwd, ctx)
        .await
}

// What a live view needs of a window: the newest lines, never half a
// character.
const LIVE_TAIL: usize = 4096;
// How often a running call tells its caller what it printed.
const LIVE_EVERY: std::time::Duration = std::time::Duration::from_millis(200);

fn tail(text: &str, bytes: usize) -> &str {
    let mut at = text.len().saturating_sub(bytes);
    while !text.is_char_boundary(at) {
        at += 1;
    }
    &text[at..]
}

// Stops the live view task when `finish` leaves, by whatever path.
struct LiveTask(tokio::task::JoinHandle<()>);

impl Drop for LiveTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// One line: whoever reads the screen, and the log, treat further newlines
// as structure, not text.
fn first_line(command: &str) -> &str {
    command.split('\n').next().unwrap_or_default()
}

// The command as it will run, and where. `written` says the model wrote the
// command: only then does rtk rewrite it, in its own vocabulary, from `cwd`
// for project filters, racing a stop so it isn't delayed. A command pi built
// for a caller runs as built — rtk's table answers for what a model types.
async fn prepare(args: Args, tier: Tier, ctx: &Ctx, written: bool) -> Result<Again, ToolError> {
    let cwd = ctx.workspace.resolve(&args.cwd, tier)?;
    let rewritten = match written {
        false => None,
        true => tokio::select! {
            r = crate::rtk::rewrite(&args.command, &cwd) => r,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        },
    };
    Ok(Again {
        command: rewritten.unwrap_or(args.command),
        cwd,
        stdin: args.stdin,
        environment: Environment::default(),
        output: OutputMode::All,
        timeout: None,
    })
}

/// A shell with explicit foreground and background execution.
pub struct Bash {
    executions: Option<Arc<dyn RunSink>>,
    controls: Option<Arc<dyn Controls>>,
    description: String,
}

impl Bash {
    /// Run a command in the foreground, allowing the host to request a handoff.
    pub async fn foreground_result(
        &self,
        command: &str,
        cwd: &str,
        ctx: &Ctx,
    ) -> Result<Foreground, ToolError> {
        let args = Args {
            command: command.to_string(),
            cwd: cwd.to_string(),
            stdin: None,
            timeout_ms: None,
            description: None,
        };
        let prepared = prepare(args, Tier::Exec, ctx, true).await?;
        self.finish(
            prepared,
            Invocation::shell(command),
            first_line(command).to_string(),
            None,
            ctx,
        )
        .await
    }

    pub const NAME: &'static str = "bash";

    /// The shell whose background calls are kept by `executions`.
    pub fn new(executions: Arc<dyn RunSink>) -> Self {
        Self {
            executions: Some(executions),
            controls: None,
            description: format!(
                "{DESCRIPTION}\n\nThe call waits for the command to exit. \
                 Stop the run to cancel the command. \
                 {} \
                 The user can move a running foreground command to the background; \
                 the call then returns its command ID. \
                 Use `background=true` to start in the background immediately.\n\n\
                 With `background=true`, the command runs independently of this turn. \
                 The call returns its command ID immediately. \
                 Its results, including a failure at startup, open a later turn. \
                 {} \
                 Continue independent work or end this turn. \
                 Do not wait for the background result. \
                 Leave output unredirected so it can be read while the command runs. \
                 Report the result when it arrives instead of asking the user to inspect it. \
                 Do not use `sleep` to wait for a result; sleep only guesses how long a command takes. \
                 Do not change files a running background command uses; restart the command after the change. \
                 To check that a server is ready, run one foreground command that waits for the ready condition, \
                 such as a request with retries.",
                foreground_limit(),
                background_limit()
            ),
        }
    }

    /// A shell that waits for each command and has no background runtime.
    pub fn foreground() -> Self {
        Self {
            executions: None,
            controls: None,
            description: format!(
                "{DESCRIPTION}\n\nThe call waits for the command to exit. \
                 Stop the run to cancel the command. \
                 {}",
                foreground_limit()
            ),
        }
    }

    /// Add the host's controls for the executions kept by this shell. The host
    /// owns their parameters and behavior; the shell exposes them in one tool.
    pub fn with_controls(mut self, controls: Arc<dyn Controls>) -> Self {
        self.description.push_str("\n\n");
        self.description.push_str(controls.description());
        self.controls = Some(controls);
        self
    }
}

/// How a shell call reached the tool, apart from the command itself.
struct Call {
    environment: Environment,
    output: OutputMode,
    /// The command is words someone wrote rather than a line pi built for a
    /// caller, so it is rtk's to rewrite.
    written: bool,
}

/// Queue a command independently of the calling turn. The execution is
/// registered before it starts, so it can be stopped from the moment it exists.
async fn schedule(
    args: Args,
    ctx: &Ctx,
    executions: &dyn RunSink,
    call: Call,
    invocation: Invocation,
) -> Result<ToolOutput, ToolError> {
    let screen = args
        .label()
        .map_or_else(|| invocation.title().into_owned(), str::to_string);
    let timeout = args.named_timeout();
    let mut again = prepare(args, Tier::Exec, ctx, call.written).await?;
    again.environment = call.environment;
    again.output = call.output;
    again.timeout = timeout;
    let stop = tokio_util::sync::CancellationToken::new();
    let execution = executions.start(ctx.into(), invocation, stop.clone());
    let id = execution.id();
    execution.watch(crate::runtime::background::Peek::text(String::new()));
    let launch = rerun(again.clone(), ctx);
    execution.rerun(launch.clone(), again);
    if let Err(error) = launch(execution.clone(), stop) {
        execution
            .channel()
            .result(format!("could not start: {error}"));
        execution.end();
    }
    Ok(
        ToolOutput::text(format!("Started background command #{id}."))
            .with_screen(format!("{screen} [background #{id}]")),
    )
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        Bash::NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "stdin": {"type":"string", "description":"Send this text to the command on standard input. The tool then closes standard input."},
                "cwd": { "type": "string", "description": "Set the working directory for this call. Use a path relative to the workspace root or an absolute path. If you omit `cwd`, the command runs in the workspace root." },
            },
            "required": ["command"],
            "additionalProperties": false,
        });
        if let Some(controls) = &self.controls {
            schema.as_object_mut().expect("object").remove("required");
            schema["properties"]
                .as_object_mut()
                .expect("properties")
                .extend(controls.parameters());
        }
        schema["properties"]["timeout_ms"] = json!({
            "type": "integer",
            "minimum": 1,
            "description": format!(
                "Set the runtime limit in milliseconds. {} {}",
                foreground_limit(),
                background_limit(),
            ),
        });
        if self.executions.is_none() {
            return schema;
        }
        schema["properties"]["background"] = json!({
            "type": "boolean",
            "description": "Run independently of this turn and return a command ID immediately. \
                Results, exit status, and output arrive automatically, including after this run ends. \
                Output remains readable while the command runs. Leave output unredirected to receive it. \
                The command stops when this process exits. \
                Use this option for commands that must continue after this turn.",
        });
        schema
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        self.execute_call(args, Environment::default(), OutputMode::All, None, ctx)
            .await
    }
}

impl Bash {
    pub(crate) async fn execute_call(
        &self,
        mut args: Value,
        environment: Environment,
        output: OutputMode,
        invocation: Option<Invocation>,
        ctx: &Ctx,
    ) -> Result<ToolOutput, ToolError> {
        let present = |key: &str| args.get(key).is_some_and(|v| !v.is_null());
        if !present("command") {
            if ["cwd", "stdin", "background", "timeout_ms"]
                .iter()
                .any(|key| present(key))
            {
                return Err(ToolError::Invalid(
                    "command is required for cwd, stdin, background, or timeout_ms".into(),
                ));
            }
            if let Some(controls) = &self.controls {
                return controls.execute(args, ctx).await;
            }
        } else if let Some(controls) = &self.controls
            && let Some(key) = controls.parameters().keys().find(|key| present(key))
        {
            // Named: "one or the other" leaves the caller guessing which half
            // of its call to drop.
            return Err(ToolError::Invalid(format!(
                "`{key}` controls existing background commands; one call runs a command or \
                 controls them, not both"
            )));
        }
        let background = match args.as_object_mut().and_then(|a| a.remove("background")) {
            Some(Value::Bool(value)) => value,
            Some(Value::Null) | None => false,
            Some(_) => {
                return Err(ToolError::Invalid(
                    "background must be true or false".into(),
                ));
            }
        };
        if background && self.executions.is_none() {
            return Err(ToolError::Invalid(
                "this run only keeps commands while it is active".into(),
            ));
        }
        if let Some(object) = args.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        let args: Args = tool::parse_args_hinted(args, ARGS_HINT)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::Invalid(
                "command must be a nonempty string".into(),
            ));
        }
        // No invocation of its own means the model called the shell directly,
        // so the command is the model's own words.
        let call = Call {
            environment,
            output,
            written: invocation.is_none(),
        };
        if background {
            let invocation = invocation.unwrap_or_else(|| Invocation::shell(args.command.clone()));
            return schedule(
                args,
                ctx,
                self.executions.as_deref().expect("background mode checked"),
                call,
                invocation,
            )
            .await;
        }
        let screen = args
            .label()
            .unwrap_or_else(|| first_line(&args.command))
            .to_string();
        let invocation = invocation.unwrap_or_else(|| Invocation::shell(args.command.clone()));
        let kept = args.named_timeout();
        // A restart keeps only the limit the call named; the default bounds this wait.
        let timeout = kept.unwrap_or(tool::limit::DEFAULT_CALL);
        let mut prepared = prepare(args, self.tier(), ctx, call.written).await?;
        prepared.environment = call.environment;
        prepared.output = call.output;
        prepared.timeout = kept;
        Ok(self
            .finish(prepared, invocation, screen, Some(timeout), ctx)
            .await?
            .into_output())
    }

    async fn finish(
        &self,
        prepared: Again,
        invocation: Invocation,
        mut screen: String,
        deadline: Option<std::time::Duration>,
        ctx: &Ctx,
    ) -> Result<Foreground, ToolError> {
        let before = crate::drift::before(ctx);
        let launched = launch(&prepared, ctx)?;
        // What the call prints while it runs, shown where the caller can
        // read it; the window is the same one a background execution keeps.
        // Nobody listening means nobody pays for reading it every tick.
        // The guard aborts the task on every way out after it starts, `?`
        // included; before it starts there is nothing to stop.
        let live = launched.streams.peek();
        let _live = ctx.hears_output().then(|| {
            let ctx = ctx.clone();
            LiveTask(tokio::spawn(async move {
                let mut seen = 0;
                loop {
                    tokio::time::sleep(LIVE_EVERY).await;
                    let printed = live.since(0);
                    if printed.total != seen {
                        seen = printed.total;
                        ctx.say_output(tail(&printed.text, LIVE_TAIL));
                    }
                }
            }))
        });
        let (completion, body, output) = if let Some(executions) = &self.executions {
            let mut requests = ctx.background_requests();
            let peek = launched.streams.peek();
            // The turn can stop this wait, but must not own the cancellation
            // of a command that has already been handed to the host.
            let stop = tokio_util::sync::CancellationToken::new();
            let unbounded = tokio_util::sync::CancellationToken::new();
            let mut running: Running = Box::pin(wait(
                launched,
                stop.clone(),
                deadline,
                Some(unbounded.clone()),
            ));
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    stop.cancel();
                    running.await?.completed()
                }
                ran = &mut running => ran?.completed(),
                _ = requests.recv() => {
                    unbounded.cancel();
                    let execution = executions.start(
                        ctx.into(), invocation, stop,
                    );
                    let id = execution.id();
                    execution.watch(peek);
                    execution.rerun(rerun(prepared.clone(), ctx), prepared);
                    supervise(running, execution);
                    screen = format!("{screen} [background #{id}]");
                    (Completion::Background { id },
                     format!("The user moved this command to the background as command #{id}. Continue independent work; its result will arrive later."),
                     Vec::new())
                }
            }
        } else {
            // Without a host the command cannot outlive this call.
            wait(launched, ctx.cancel.clone(), deadline, None)
                .await?
                .completed()
        };
        let moved = crate::drift::absorb(ctx, before);
        Ok(Foreground {
            completion,
            body,
            output,
            screen,
            changes: crate::drift::report(&moved),
        })
    }
}

/// How the background command `again` runs again under `ctx`: the first
/// run's way, or a restarted pi's.
pub fn rerun(again: Again, ctx: &Ctx) -> Rerun {
    let ctx = crate::process::spills_only(ctx);
    Arc::new(move |execution, stop| {
        #[cfg(unix)]
        {
            let cmd = command(&again);
            let input = again
                .stdin
                .as_deref()
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            crate::scripts::fd3::background(
                cmd,
                input,
                &ctx,
                execution,
                stop,
                again.output,
                again.timeout,
            )
        }
        #[cfg(not(unix))]
        {
            let launched = launch(&again, &ctx)?;
            execution.watch(launched.streams.peek());
            supervise(
                Box::pin(wait(launched, stop, again.timeout, None)),
                execution,
            );
            Ok(())
        }
    })
}

// A shell command just spawned, not yet anyone's execution.
type Launched = crate::process::Launched;

fn command(again: &Again) -> Command {
    let mut cmd = Command::new(SHELL);
    cmd.arg("-c").arg(&again.command).current_dir(&again.cwd);
    let shared: Vec<_> = again
        .environment
        .directory
        .as_deref()
        .map(crate::scripts::env::read)
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, _)| std::env::var_os(name).is_none())
        .collect();
    cmd.envs(shared.iter().map(|(name, value)| (name, value)));
    cmd.envs(
        again
            .environment
            .arguments
            .iter()
            .filter(|(name, _)| {
                std::env::var_os(name).is_none() && shared.iter().all(|(key, _)| key != name)
            })
            .map(|(name, value)| (name, value)),
    );
    cmd
}

fn launch(again: &Again, ctx: &Ctx) -> Result<Launched, ToolError> {
    let stdin = again.stdin.as_ref().map(|s| s.as_bytes().to_vec());
    let launched = crate::process::launch(command(again), stdin, ctx)?;
    tracing::info!(target: "pi::bash", command_bytes = again.command.len(), cwd = %again.cwd.display(), group = ?launched.group, "spawned");
    Ok(launched)
}

async fn wait(
    launched: Launched,
    stop: tokio_util::sync::CancellationToken,
    deadline: Option<std::time::Duration>,
    unbounded: Option<tokio_util::sync::CancellationToken>,
) -> Result<Ran, ToolError> {
    let group = launched.group;
    let ends = crate::process::Ends {
        stop,
        deadline,
        unbounded,
    };
    let exited = crate::process::settle(launched, ends).await?;
    let (out, err) = (exited.stdout, exited.stderr);
    let code = exited.status.code().unwrap_or(-1);
    tracing::info!(target: "pi::bash", ?group, code, stdout_bytes = out.total, stderr_bytes = err.total, "exited");
    Ok(Ran {
        code,
        body: body(&out, &err),
        output: screen_output([&out, &err]),
    })
}

type Running = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Ran, ToolError>> + Send>>;

fn supervise(running: Running, execution: Arc<dyn RunHandle>) {
    use ::execution::background::{Outcome, Report};
    ::execution::background::spawn(execution, async move {
        let (outcome, output) = match running.await {
            Ok(ran) => {
                let ended = format!("exited {}", ran.code);
                let outcome = if ran.code == 0 {
                    Outcome::Succeeded
                } else {
                    Outcome::Failed
                };
                let body = if ran.body.is_empty() {
                    format!("{ended}, no output")
                } else {
                    format!("{ended}:\n{}", ran.body)
                };
                (outcome, ToolOutput::text(body))
            }
            Err(ToolError::Cancelled) => (
                Outcome::Cancelled,
                ToolOutput::text("execution cancelled").warned(),
            ),
            Err(why) => (Outcome::Failed, ToolOutput::text(why.to_string()).warned()),
        };
        Report { outcome, output }
    });
}

/// What a finished command left behind.
pub struct Ran {
    pub code: i32,
    /// What the command printed, plus a spill note for what did not fit and
    /// any note explaining the failure. Empty when it succeeded silently.
    pub body: String,
    /// The same lines, tags and spill notes left out: what a screen shows.
    output: Vec<String>,
}

impl Ran {
    fn completed(self) -> (Completion, String, Vec<String>) {
        (Completion::Exited(self.code), self.body, self.output)
    }
}

// The result an exited command leaves: what it printed, and the exit when it
// did not end well.
fn finished(code: i32, mut body: String, screen: String) -> ToolOutput {
    if code != 0 {
        body.push_str(&format!("exit {}\n", code));
    }
    if body.is_empty() {
        body.push_str("exit 0, no output");
    }
    let out = ToolOutput::text(body).with_screen(screen);
    if code != 0 { out.warned() } else { out }
}

/// Run `command` to completion under the workspace's clamps — its own process
/// group, the context's cancellation, and `timeout` when there is one, past
/// which the group is taken down with SIGTERM then SIGKILL.
pub async fn run(
    command: &str,
    cwd: &std::path::Path,
    timeout: Option<std::time::Duration>,
    ctx: &Ctx,
) -> Result<Ran, ToolError> {
    let mut cmd = Command::new(SHELL);
    cmd.arg("-c").arg(command).current_dir(cwd);
    let exited = crate::process::run(cmd, None, timeout, ctx)
        .await
        .inspect_err(|e| {
            if let ToolError::Timeout { ms, .. } = e {
                tracing::warn!(target: "pi::bash", command_bytes = command.len(), timeout_ms = ms, "timed out");
            }
        })?;
    let code = exited.status.code().unwrap_or(-1);
    let (stdout, stderr) = (exited.stdout, exited.stderr);

    // Keep execution context and output sizes without copying command content.
    tracing::info!(
        target: "pi::bash",
        command_bytes = command.len(),
        cwd = %cwd.display(),
        code,
        stdout_bytes = stdout.total,
        stderr_bytes = stderr.total,
        "exited"
    );

    // No exit line in the body: `code` says that, and a caller that renders
    // it as well would say it twice.
    Ok(Ran {
        code,
        body: body(&stdout, &stderr),
        output: screen_output([&stdout, &stderr]),
    })
}

// What a command printed, each stream in its own section, then where any
// that did not fit was spilled.
pub(crate) fn body(stdout: &output::Captured, stderr: &output::Captured) -> String {
    let mut body = String::new();
    body.push_str(&section("stdout", stdout));
    body.push_str(&section("stderr", stderr));
    for captured in [stdout, stderr] {
        if let Some(s) = &captured.spill {
            body.push_str(&format!("{}\n", s.note()));
        }
    }
    body
}

// The same lines as `body` holds, without the tags and spill notes: what a
// screen shows of them.
fn screen_output(captures: [&output::Captured; 2]) -> Vec<String> {
    captures
        .into_iter()
        .flat_map(|captured| captured.text.lines().map(str::to_string))
        .collect()
}

fn section(label: &str, s: &output::Captured) -> String {
    let body = s.text.trim_end();
    if body.is_empty() {
        return String::new();
    }
    format!("<{label}>\n{body}\n</{label}>\n")
}

#[cfg(test)]
mod background_tests {
    use super::*;
    use crate::testing::{Sink, ctx, seen};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn only_the_backgroundable_shell_offers_background() {
        assert!(
            Bash::foreground().schema()["properties"]
                .get("background")
                .is_none()
        );
        for shell in [Bash::new(Arc::new(Sink::default())), Bash::foreground()] {
            assert!(shell.schema()["properties"]["timeout_ms"].is_object());
        }
        assert!(
            Bash::new(Arc::new(Sink::default())).schema()["properties"]["background"].is_object()
        );
    }

    #[tokio::test]
    async fn foreground_content_preserves_output_and_changes_without_exit_notices() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let path = seen(dir.path(), &ctx, "tracked.txt", "before\n");
        let result = foreground_result(
            "printf 'exit 3'; printf 'after\\n' > tracked.txt; exit 3",
            ".",
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(result.completion, Completion::Exited(3));
        let content = result.content();
        assert_eq!(content.matches("exit 3").count(), 1, "{content}");
        assert!(
            content.contains("tracked.txt") && content.contains("after"),
            "{content}"
        );
        assert_eq!(ctx.viewed(&path).as_deref(), Some("after\n"));
        let output = result.into_output();
        assert!(output.warned);
        assert_eq!(output.flatten().matches("exit 3").count(), 2);
        // What the screen draws: the command, what it printed, then the
        // file's rows — each marked, so none reads as another's.
        let screen = output.screen();
        let rows: Vec<&str> = screen.lines().collect();
        assert_eq!(
            rows[0],
            "printf 'exit 3'; printf 'after\\n' > tracked.txt; exit 3"
        );
        assert_eq!(rows[1], "· exit 3");
        assert_eq!(rows[2], "tracked.txt +1 -1", "{rows:?}");
        assert_eq!(rows[3], "-1 before", "{rows:?}");
        assert_eq!(rows[4], "+1 after", "{rows:?}");
    }

    // The call returns at once; the end comes back as one result, exit code
    // and output together, and the execution ends.
    #[tokio::test]
    async fn a_background_command_reports_its_end_as_one_result() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let ended = sink.ended.notified();
        let out = Bash::new(sink.clone())
            .execute(
                json!({"cwd": ".", "command": "sleep 0.2; echo imported 40 rows; exit 3", "background": true}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(out.flatten().contains("command #7"), "{}", out.flatten());
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the execution ended");
        let said = sink.said.lock().unwrap();
        assert!(said[0].starts_with("start sleep 0.2"), "{said:?}");
        assert!(
            said[1].contains("exited 3") && said[1].contains("imported 40 rows"),
            "{said:?}"
        );
        assert_eq!(said[2], "end");
    }

    // A server never exits: what it printed so far must be readable meanwhile.
    #[tokio::test]
    async fn a_running_command_shows_what_it_printed_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        Bash::new(sink.clone())
            .execute(
                json!({"cwd": ".", "command": "echo listening on 8080; echo warn >&2; sleep 30", "background": true}),
                &ctx,
            )
            .await
            .unwrap();
        let peek = sink.output.lock().unwrap().clone().expect("watched");
        for _ in 0..100 {
            let seen = peek.latest();
            if seen.contains("listening on 8080") && seen.contains("warn") {
                sink.stop.lock().unwrap().take().unwrap().cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never showed: {}", peek.latest());
    }

    #[tokio::test]
    async fn without_background_it_is_the_plain_shell() {
        let dir = tempfile::tempdir().unwrap();
        let out = Bash::new(Arc::new(Sink::default()))
            .execute(
                json!({"cwd": ".", "command": "echo hi", "background": false}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.flatten().contains("hi"), "{}", out.flatten());
    }

    async fn started(dir: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.join("starts").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the shell started");
    }

    #[tokio::test]
    async fn direct_commands_keep_running_past_the_tool_deadline() {
        for managed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let sink = Arc::new(Sink::default());
            let ctx = ctx(dir.path());
            let shell = if managed {
                Bash::new(sink.clone())
            } else {
                Bash::foreground()
            };
            let mut call = tokio::spawn(async move {
                shell.foreground_result(
                    "echo ready > starts; while [ ! -f release ]; do sleep 0.01; done; echo done; exit 3",
                    ".", &ctx,
                ).await
            });
            started(dir.path()).await;
            tokio::time::pause();
            tokio::time::advance(tool::limit::DEFAULT_CALL + Duration::from_secs(1)).await;
            tokio::time::resume();
            let early = tokio::time::timeout(Duration::from_millis(100), &mut call).await;
            std::fs::write(dir.path().join("release"), "").unwrap();
            let result = match early {
                Ok(result) => result.unwrap(),
                Err(_) => tokio::time::timeout(Duration::from_secs(5), call)
                    .await
                    .unwrap()
                    .unwrap(),
            }
            .expect("a direct command has no tool deadline");
            assert_eq!(result.completion, Completion::Exited(3));
            assert!(result.content().contains("done"));
            assert!(
                sink.said.lock().unwrap().is_empty(),
                "time alone does not hand off a command"
            );
        }
    }

    #[tokio::test]
    async fn tool_calls_keep_their_foreground_deadline() {
        for managed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ctx = ctx(dir.path());
            let shell = if managed {
                Bash::new(Arc::new(Sink::default()))
            } else {
                Bash::foreground()
            };
            let call = tokio::spawn(async move {
                shell
                    .execute(
                        json!({"command": "echo ready > starts; echo before; sleep 6000"}),
                        &ctx,
                    )
                    .await
            });
            started(dir.path()).await;
            tokio::time::pause();
            tokio::time::advance(tool::limit::DEFAULT_CALL + Duration::from_secs(1)).await;
            tokio::time::resume();
            let error = tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            let ToolError::Timeout { ms, printed } = error else {
                panic!("expected timeout, got {error:?}")
            };
            assert_eq!(ms, tool::limit::DEFAULT_CALL.as_millis() as u64);
            assert!(printed.contains("before"), "{printed}");
        }
    }

    #[tokio::test]
    async fn a_foreground_call_takes_the_timeout_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let call = tokio::spawn(async move {
            Bash::foreground()
                .execute(
                    json!({"command": "echo ready > starts; sleep 6000", "timeout_ms": 120000}),
                    &ctx,
                )
                .await
        });
        started(dir.path()).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        tokio::time::resume();
        let error = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        let ToolError::Timeout { ms, .. } = error else {
            panic!("expected timeout, got {error:?}")
        };
        assert_eq!(ms, 120_000);
    }

    // A running call hands out what it prints, and stops when it ends.
    #[tokio::test]
    async fn a_running_call_reports_what_it_prints() {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let ctx = crate::testing::ctx(dir.path()).with_output(Arc::new(move |text| {
            sink.lock().unwrap().push(text);
        }));
        Bash::foreground()
            .execute(
                json!({"command": "echo one; sleep 0.5; echo two; sleep 0.5"}),
                &ctx,
            )
            .await
            .unwrap();
        let printed = seen.lock().unwrap().clone();
        assert!(printed.iter().any(|t| t.contains("one")), "{printed:?}");
        assert!(printed.iter().any(|t| t.contains("two")), "{printed:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            seen.lock().unwrap().len(),
            printed.len(),
            "its end stops the reports"
        );
    }

    #[tokio::test]
    async fn a_background_command_takes_the_timeout_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let ended = sink.ended.notified();
        Bash::new(sink.clone())
            .execute(
                json!({"command": "echo ready > starts; sleep 6000", "background": true, "timeout_ms": 2000}),
                &ctx,
            )
            .await
            .unwrap();
        started(dir.path()).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::time::resume();
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the timed-out command ended");
        let said = sink.said.lock().unwrap();
        assert!(
            said.iter().any(|line| line.contains("timed out")),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn a_requested_handoff_continues_once_with_its_output_and_outlives_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        assert_eq!(ctx.background_running(), 0, "idle requests are not queued");
        let call = {
            let sink = sink.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                Bash::new(sink)
                    .execute(json!({"command": "echo start >> starts; echo before; while [ ! -f release ]; do sleep 0.01; done; echo after; echo warning >&2; exit 3"}), &ctx)
                    .await
            })
        };
        started(dir.path()).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(
            !call.is_finished(),
            "elapsed time must not move the command"
        );
        assert!(sink.said.lock().unwrap().is_empty());
        tokio::time::resume();
        assert_eq!(ctx.background_running(), 1);
        let out = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(out.flatten().contains("command #7"), "{}", out.flatten());
        assert!(out.screen().contains("[background #7]"));
        let peek = sink.output.lock().unwrap().clone().expect("watched");
        assert!(peek.latest().contains("before"), "{}", peek.latest());
        ctx.cancel.cancel();
        tokio::time::pause();
        tokio::time::advance(tool::limit::DEFAULT_CALL + Duration::from_secs(1)).await;
        tokio::time::resume();
        assert_eq!(
            sink.said.lock().unwrap().len(),
            1,
            "handoff lifts the foreground deadline"
        );
        assert_eq!(
            ctx.background_running(),
            0,
            "the handed-off call no longer listens"
        );
        let ended = sink.ended.notified();
        std::fs::write(dir.path().join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("starts")).unwrap(),
            "start\n"
        );
        let said = sink.said.lock().unwrap();
        assert_eq!(said.len(), 3, "{said:?}");
        for expected in ["before", "after", "warning", "exited 3"] {
            assert!(said[1].contains(expected), "{said:?}");
        }
        assert_eq!(said[2], "end");
    }

    #[tokio::test]
    async fn stopping_before_handoff_cancels_without_registering_a_command() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let call = {
            let sink = sink.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                Bash::new(sink)
                    .execute(json!({"command": "echo start > starts; sleep 60"}), &ctx)
                    .await
            })
        };
        started(dir.path()).await;
        ctx.cancel.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, ToolError::Cancelled), "{error:?}");
        assert!(sink.said.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_host_can_stop_a_command_after_requested_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let call = {
            let sink = sink.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move {
                Bash::new(sink)
                    .execute(json!({"command": "echo $$ > starts; sleep 60"}), &ctx)
                    .await
            })
        };
        started(dir.path()).await;
        let pid: i32 = std::fs::read_to_string(dir.path().join("starts"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(ctx.background_running(), 1);
        tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        sink.stop
            .lock()
            .unwrap()
            .take()
            .expect("registered")
            .cancel();
        tokio::time::timeout(Duration::from_secs(5), async {
            // SAFETY: signal 0 only checks whether this process still exists.
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the host stopped the command");
        assert_eq!(
            sink.said.lock().unwrap().len(),
            1,
            "stops do not report a result"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_exited_shell_reclaims_descendants_holding_its_output_pipes() {
        let mut cases = tokio::task::JoinSet::new();
        for mode in 0..3 {
            for command in [
                "sleep 30 & echo done; echo warning >&2; exit 3",
                // stdout closes before stderr: do not await it twice at the deadline.
                "sleep 30 >&- & echo done; echo warning >&2; exit 3",
                // Descendants may finish printing within the exit grace period.
                "(sleep 0.1; echo done; echo warning >&2) & exit 3",
            ] {
                cases.spawn(async move {
                    let dir = tempfile::tempdir().unwrap();
                    let sink = Arc::new(Sink::default());
                    let ctx = ctx(dir.path());
                    let shell = if mode == 1 {
                        Bash::foreground()
                    } else {
                        Bash::new(sink.clone())
                    };
                    let ended = sink.ended.notified();
                    let result = tokio::time::timeout(Duration::from_secs(5), async {
                        let out = shell
                            .execute(json!({"command": command, "background": mode == 2}), &ctx)
                            .await
                            .unwrap();
                        if mode == 2 {
                            ended.await;
                            sink.said.lock().unwrap()[1].clone()
                        } else {
                            assert!(out.warned);
                            assert!(sink.said.lock().unwrap().is_empty());
                            out.flatten()
                        }
                    })
                    .await;
                    if let Some(stop) = sink.stop.lock().unwrap().take() {
                        stop.cancel();
                    }
                    let said = result.expect("output pipes are reclaimed after shell exit");
                    for expected in [
                        "done",
                        "warning",
                        if mode == 2 { "exited 3" } else { "exit 3" },
                    ] {
                        assert!(said.contains(expected), "{said}");
                    }
                });
            }
        }
        while let Some(case) = cases.join_next().await {
            case.unwrap();
        }
    }

    #[tokio::test]
    async fn foreground_waits_for_explicit_completion() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let call = {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                Bash::foreground()
                    .execute(
                        json!({"command": "echo start >> starts; echo before; while [ ! -f release ]; do sleep 0.01; done; echo after; echo warning >&2; exit 3"}),
                        &ctx,
                    )
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("starts").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the foreground command started");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let returned_early = call.is_finished();
        // Release the process even if the foreground wait regresses.
        std::fs::write(dir.path().join("release"), "").unwrap();
        let out = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            !returned_early,
            "a foreground call must wait for its command"
        );
        assert!(out.warned);
        assert!(!out.screen().contains("[background"));
        for expected in ["before", "after", "warning", "exit 3"] {
            assert!(out.flatten().contains(expected), "{}", out.flatten());
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("starts")).unwrap(),
            "start\n"
        );
    }

    #[tokio::test]
    async fn a_managed_shell_still_reports_a_failed_output_spill() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path()).with_spill_root(dir.path().join("spill"));
        std::fs::write(dir.path().join("spill"), "in the way").unwrap();
        let err = Bash::foreground()
            .execute(json!({"command": "seq 1 200000"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("SPILL_FAILED"));
    }

    #[tokio::test]
    async fn background_shell_reports_output_capture_failure_in_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path()).with_spill_root(dir.path().join("spill"));
        std::fs::write(dir.path().join("spill"), "in the way").unwrap();
        let sink = Arc::new(Sink::default());
        let ended = sink.ended.notified();
        Bash::new(sink.clone())
            .execute(json!({"command":"seq 1 200000", "background":true}), &ctx)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the failed capture ends the command");
        assert!(
            sink.said
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("could not spill oversized output"))
        );
    }

    #[tokio::test]
    async fn stopping_a_background_command_takes_its_group_and_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        Bash::new(sink.clone())
            .execute(
                json!({"cwd": ".", "command": "echo $$ > pid; sleep 30", "background": true}),
                &ctx,
            )
            .await
            .unwrap();
        let pid_file = dir.path().join("pid");
        while !pid_file.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        sink.stop.lock().unwrap().take().expect("started").cancel();
        for _ in 0..50 {
            // SAFETY: signal 0 only asks whether the process exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                assert_eq!(sink.said.lock().unwrap().len(), 1, "nothing after the stop");
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{pid} still runs");
    }
}
