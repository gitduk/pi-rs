#[cfg(not(unix))]
use crate::background::RunHandle;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::process::Command;

use crate::background::{Again, Controls, Environment, Invocation, OutputMode, Rerun, RunSink};

use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    #[serde(default = "workspace_root")]
    cwd: String,
    #[serde(default)]
    stdin: Option<String>,
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
}

// Optional: a required cwd was still left out, and each miss cost a turn.
fn workspace_root() -> String {
    ".".to_string()
}

/// The shell every command runs under as `SHELL -c command`.
pub const SHELL: &str = "bash";

const ARGS_HINT: &str = "`bash` requires `command`. Use optional `cwd` for the working directory. Use optional `stdin` for input text.";

const DESCRIPTION: &str = "Each call runs a shell command in a new shell. \
    Use `cwd` to set the working directory instead of a leading `cd`. \
    If you omit `cwd`, the command runs in the workspace root. \
    Directory changes and environment changes do not persist between calls.\n\n\
    Prefer `read` to `cat`. Prefer `write` to heredocs. \
    Prefer `edit` to `sed` when line content determines which lines to change. \
    These tools report failures you can use to correct errors. \
    Use `bash` for substitutions by pattern or position (`sed`, `perl`, `awk`). \
    Use `bash` to move files (`mv`) or delete files (`rm`).";

/// Run a written command to completion under the workspace's checks and rewrite.
pub async fn foreground(command: &str, cwd: &str, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
    Ok(foreground_result(command, cwd, ctx).await?.into_output())
}

pub struct Foreground {
    pub ran: Ran,
    preview: String,
    changes: Option<crate::drift::Report>,
}

impl Foreground {
    pub fn content(&self) -> String {
        let mut body = self.ran.body.clone();
        if let Some(changes) = &self.changes {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&changes.content);
        }
        body
    }

    pub fn into_output(self) -> ToolOutput {
        let out = self.ran.output(self.preview);
        match self.changes {
            Some(changes) => changes.append(out),
            None => out,
        }
    }
}

/// The command's content and exit status, before status text is added to the
/// tool result. Workspace change reports remain part of the content.
pub async fn foreground_result(
    command: &str,
    cwd: &str,
    ctx: &Ctx,
) -> Result<Foreground, ToolError> {
    let preview = first_line(command).to_string();
    let args = Args {
        command: command.to_string(),
        cwd: cwd.to_string(),
        stdin: None,
        description: None,
    };
    let prepared = prepare(args, Tier::Exec, ctx, true).await?;
    let before = crate::drift::before(ctx);
    let ran = run(&prepared.command, &prepared.cwd, None, ctx).await?;
    let moved = crate::drift::absorb(ctx, before);
    Ok(Foreground {
        ran,
        preview,
        changes: crate::drift::report(&moved),
    })
}

// One line: whoever reads the preview, and the log, treat further newlines
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
    })
}

/// A shell with explicit foreground and background execution.
pub struct Bash {
    executions: Option<Arc<dyn RunSink>>,
    controls: Option<Arc<dyn Controls>>,
    description: String,
}

impl Bash {
    pub const NAME: &'static str = "bash";

    /// The shell whose background calls are kept by `executions`.
    pub fn new(executions: Arc<dyn RunSink>) -> Self {
        Self {
            executions: Some(executions),
            controls: None,
            description: format!(
                "{DESCRIPTION}\n\nThe call waits for the command to exit. \
                 Stop the run to cancel the command. \
                 The tool stops a foreground command after {}. \
                 Use `background=true` for a command you expect to take longer.\n\n\
                 With `background=true`, the command runs independently of this turn. \
                 The call returns its command ID immediately. \
                 Its results, including a failure at startup, open a later turn. \
                 Continue independent work or end this turn. \
                 Do not wait for the background result. \
                 Leave output unredirected so it can be read while the command runs.",
                llm::figures::elapsed(tool::limit::MAX_CALL)
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
                 The tool stops the command after {}.",
                llm::figures::elapsed(tool::limit::MAX_CALL)
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
    let preview = args
        .label()
        .map_or_else(|| invocation.title().into_owned(), str::to_string);
    let mut again = prepare(args, Tier::Exec, ctx, call.written).await?;
    again.environment = call.environment;
    again.output = call.output;
    let stop = tokio_util::sync::CancellationToken::new();
    let execution = executions.start(ctx.workspace.root().to_path_buf(), invocation, stop.clone());
    let id = execution.id();
    execution.watch(crate::background::Peek::text(String::new()));
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
            .with_preview(format!("{preview} [background #{id}]")),
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
        if self.executions.is_none() {
            return schema;
        }
        schema["properties"]["background"] = json!({
            "type": "boolean",
            "description": "Run the command independently of this turn. \
                Use this mode for builds, deployments, development servers, or watchers. \
                The call returns a command ID immediately. \
                Each result the command reports, and its exit status and output, arrive automatically with other queued results, \
                also after this run ends. A command that fails at startup reports that the same way. \
                The command stops when this process exits.\n\n\
                Output remains readable while the command runs. Leave output unredirected. \
                Use `background=true` instead of `nohup` or `&` for commands that must continue after this turn.",
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
            if ["cwd", "stdin", "background"]
                .iter()
                .any(|key| present(key))
            {
                return Err(ToolError::Invalid(
                    "command is required for cwd, stdin, or background".into(),
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
        let preview = args
            .label()
            .unwrap_or_else(|| first_line(&args.command))
            .to_string();
        let mut prepared = prepare(args, self.tier(), ctx, call.written).await?;
        prepared.environment = call.environment;
        prepared.output = call.output;
        let before = crate::drift::before(ctx);
        let launched = launch(&prepared, ctx)?;
        // A command the model waits for holds the turn, and in a run with
        // nobody at the keyboard nothing else would ever end it.
        let ran = wait(launched, ctx.cancel.clone(), Some(tool::limit::MAX_CALL)).await?;
        let moved = crate::drift::absorb(ctx, before);
        Ok(crate::drift::tell(ran.output(preview), &moved))
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
            crate::channel::process::background(cmd, input, &ctx, execution, stop, again.output)
        }
        #[cfg(not(unix))]
        {
            let launched = launch(&again, &ctx)?;
            execution.watch(launched.streams.peek());
            supervise(Box::pin(wait(launched, stop, None)), execution);
            Ok(())
        }
    })
}

// What a rerun keeps of the call's context: where output spills, not the
// turn's locks or progress.
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
    tracing::info!(target: "pi::bash", command = %again.command, cwd = %again.cwd.display(), group = ?launched.group, "spawned");
    Ok(launched)
}

async fn wait(
    launched: Launched,
    stop: tokio_util::sync::CancellationToken,
    deadline: Option<std::time::Duration>,
) -> Result<Ran, ToolError> {
    let group = launched.group;
    let ends = crate::process::Ends { stop, deadline };
    let exited = crate::process::settle(launched, ends).await?;
    let (out, err) = (exited.stdout, exited.stderr);
    let code = exited.status.code().unwrap_or(-1);
    tracing::info!(target: "pi::bash", ?group, code, stdout_bytes = out.total, stderr_bytes = err.total, "exited");
    Ok(Ran {
        code,
        body: body(&out, &err),
    })
}

#[cfg(not(unix))]
type Running = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Ran, ToolError>> + Send>>;

#[cfg(not(unix))]
fn supervise(running: Running, execution: Arc<dyn RunHandle>) {
    tokio::spawn(async move {
        match running.await {
            Ok(ran) => {
                let ended = format!("exited {}", ran.code);
                execution.channel().result(if ran.body.is_empty() {
                    format!("{ended}, no output")
                } else {
                    format!("{ended}:\n{}", ran.body)
                });
            }
            Err(ToolError::Cancelled) => return,
            Err(why) => execution.channel().result(why.to_string()),
        }
        execution.end();
    });
}

/// What a finished command left behind.
pub struct Ran {
    pub code: i32,
    /// What the command printed, plus a spill note for what did not fit and
    /// any note explaining the failure. Empty when it succeeded silently.
    pub body: String,
}

impl Ran {
    fn output(mut self, preview: String) -> ToolOutput {
        if self.code != 0 {
            self.body.push_str(&format!("exit {}\n", self.code));
        }
        if self.body.is_empty() {
            self.body.push_str("exit 0, no output");
        }
        let out = ToolOutput::text(self.body).with_preview(preview);
        if self.code != 0 { out.warned() } else { out }
    }
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
                tracing::warn!(target: "pi::bash", command = %command, timeout_ms = ms, "timed out");
            }
        })?;
    let code = exited.status.code().unwrap_or(-1);
    let (stdout, stderr) = (exited.stdout, exited.stderr);

    // The exit code and the command, always. What the command printed is
    // in the transcript; what it was run against — the directory — is not.
    tracing::info!(
        target: "pi::bash",
        command = %command,
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
    use crate::testing::{Sink, ctx};
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
            assert!(shell.schema()["properties"].get("timeout_ms").is_none());
        }
        assert!(
            Bash::new(Arc::new(Sink::default())).schema()["properties"]["background"].is_object()
        );
    }

    #[tokio::test]
    async fn foreground_content_preserves_output_and_changes_without_exit_notices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tracked.txt");
        std::fs::write(&path, "before\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(10))
            .unwrap();
        let ctx = ctx(dir.path());
        ctx.note_view(&path, "before\n");
        let result = foreground_result(
            "printf 'exit 3'; printf 'after\\n' > tracked.txt; exit 3",
            ".",
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(result.ran.code, 3);
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
        assert!(!out.preview().contains("[background"));
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
