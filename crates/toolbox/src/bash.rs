use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::process::Command;

use crate::background::{Again, JobControls, JobHandle, JobSink, Rerun};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const FOREGROUND_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    command: String,
    #[serde(default = "workspace_root")]
    cwd: String,
}

// Optional: a required cwd was still left out, and each miss cost a turn.
fn workspace_root() -> String {
    ".".to_string()
}

const ARGS_HINT: &str = "bash takes `command`, plus `cwd` to run anywhere but the workspace root";

const DESCRIPTION: &str = "Run a shell command where cwd says, in a fresh shell each call: cd and \
    environment changes do not carry over. Prefer read and write over cat \
    and heredocs, and edit over sed when the lines are picked by what they \
    say: those report failures you can act on. A substitution picked by \
    pattern or position belongs here (sed, perl, awk), and so do moves and \
    deletions (mv, rm).";

/// Run a command to completion under the workspace's checks and rewrite.
pub async fn foreground(command: &str, cwd: &str, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
    let preview = first_line(command).to_string();
    let args = Args {
        command: command.to_string(),
        cwd: cwd.to_string(),
    };
    let (command, cwd) = prepare(args, Tier::Exec, ctx).await?;
    let before = crate::drift::before(ctx);
    let ran = run(&command, &cwd, DEFAULT_TIMEOUT, ctx).await?;
    let moved = crate::drift::absorb(ctx, before);
    Ok(crate::drift::tell(ran.output(preview), &moved))
}

// One line: whoever reads the preview, and the log, treat further newlines
// as structure, not text.
fn first_line(command: &str) -> &str {
    command.split('\n').next().unwrap_or_default()
}

// The command as it will run, and where: rtk rewrites it in its own
// vocabulary, from `cwd` for project filters, racing a stop so it isn't delayed.
async fn prepare(
    args: Args,
    tier: Tier,
    ctx: &Ctx,
) -> Result<(String, std::path::PathBuf), ToolError> {
    let cwd = ctx.workspace.resolve(&args.cwd, tier)?;
    let rewritten = tokio::select! {
        r = crate::rtk::rewrite(&args.command, &cwd) => r,
        _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
    };
    Ok((rewritten.unwrap_or(args.command), cwd))
}

/// `bash` for a run that keeps jobs: a soft timeout returns the still-running
/// command's id; `background` can return it immediately.
pub struct Bash {
    jobs: Arc<dyn JobSink>,
    controls: Option<Arc<dyn JobControls>>,
    background: bool,
    description: String,
}

impl Bash {
    pub const NAME: &'static str = "bash";

    /// The shell whose background calls are kept by `jobs`.
    pub fn new(jobs: Arc<dyn JobSink>) -> Self {
        Self {
            jobs,
            controls: None,
            background: true,
            description: format!(
                "{} After at most {} seconds, return a running job id and its latest output; \
                 the command keeps running until it exits or you stop the job. For long-running \
                 commands, leave progress output unredirected instead of piping it through head or tail.",
                DESCRIPTION,
                FOREGROUND_WAIT.as_secs()
            ),
        }
    }

    /// A shell whose unfinished calls stay controllable during this run,
    /// without offering to start commands apart from it.
    pub fn scoped(jobs: Arc<dyn JobSink>) -> Self {
        Self {
            background: false,
            ..Self::new(jobs)
        }
    }

    /// Add the host's controls for the jobs kept by this shell. The host
    /// owns their parameters and behavior; the shell exposes them in one tool.
    pub fn with_controls(mut self, controls: Arc<dyn JobControls>) -> Self {
        self.description.push(' ');
        self.description.push_str(controls.description());
        self.controls = Some(controls);
        self
    }
}

/// Queue a command independently of the calling turn, starting it after
/// `after`. The job is registered now, so it can be stopped before launch.
async fn schedule(
    command: &str,
    cwd: &str,
    after: std::time::Duration,
    ctx: &Ctx,
    jobs: &dyn JobSink,
) -> Result<ToolOutput, ToolError> {
    let preview = first_line(command).to_string();
    let (command, cwd) = prepare(
        Args {
            command: command.into(),
            cwd: cwd.into(),
        },
        Tier::Exec,
        ctx,
    )
    .await?;
    let at = tokio::time::Instant::now()
        .checked_add(after)
        .ok_or_else(|| ToolError::Invalid("the start delay is too large".into()))?;
    let stop = tokio_util::sync::CancellationToken::new();
    let job = jobs.start(
        ctx.workspace.root().to_path_buf(),
        preview.clone(),
        stop.clone(),
        None,
    );
    let id = job.id();
    job.watch(Arc::new(String::new));
    let again = Again { command, cwd };
    let launch = rerun(again.clone(), ctx);
    job.rerun(launch.clone(), Some(again));
    if !after.is_zero() {
        job.status(format!(
            "waiting {} before starting",
            llm::figures::elapsed(after)
        ));
    }
    tokio::spawn(async move {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep_until(at) => {}
        }
        job.status("starting".into());
        if let Err(why) = launch(job.clone(), stop) {
            job.result(format!("could not start: {why}"));
            job.end();
        }
    });
    let when = if after.is_zero() {
        "now".to_string()
    } else {
        format!("in {}", llm::figures::elapsed(after))
    };
    Ok(ToolOutput::text(format!(
        "scheduled as background job #{id}, starting {when}. Its exit status and output \
         come back automatically later. Continue independent work, or report progress and end this turn."
    )).with_preview(format!("{preview} [background #{id}]")))
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
                "cwd": { "type": "string", "description": "Where the command runs, as a workspace-relative or absolute directory; omit it for the workspace root." },
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
        if !self.background {
            return schema;
        }
        schema["properties"]["background"] = json!({
            "type": "boolean",
            "description": "Run it apart from this turn — a build, a deploy, a dev server, a \
                watcher: the call returns at once with a job id, and once it exits its status \
                and output come back as a turn of its own. No timeout applies, and it ends with \
                this process. What it prints stays readable while it runs, so leave it \
                unredirected; use this, not nohup or `&`, for anything meant to keep running.",
        });
        schema["properties"]["wait"] = json!({
            "type": "integer",
            "minimum": 0,
            "default": 0,
            "description": "Seconds to delay before starting command. Requires command and background=true. The call returns a job id immediately; the delay happens in the background. Omit or use 0 to start immediately. Completion reports automatically.",
        });
        schema
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, mut args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let present = |key: &str| args.get(key).is_some_and(|v| !v.is_null());
        if !present("command") {
            if ["cwd", "background", "wait"].iter().any(|key| present(key)) {
                return Err(ToolError::Invalid(
                    "command is required for cwd, background, or a start delay".into(),
                ));
            }
            if let Some(controls) = &self.controls {
                return controls.execute(args, ctx).await;
            }
        } else if let Some(controls) = &self.controls
            && controls.parameters().keys().any(|key| present(key))
        {
            return Err(ToolError::Invalid(
                "run a command or control existing jobs in one call, not both".into(),
            ));
        }
        let after = match args.get("wait").filter(|v| !v.is_null()) {
            Some(v) => Some(v.as_u64().ok_or_else(|| {
                ToolError::Invalid(
                    "wait must be a nonnegative number of seconds before starting command".into(),
                )
            })?),
            None => None,
        };
        let background = args
            .as_object_mut()
            .and_then(|a| a.remove("background"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if background && !self.background {
            return Err(ToolError::Invalid(
                "this run only keeps commands while it is active".into(),
            ));
        }
        if after.is_some() && !background {
            return Err(ToolError::Invalid(
                "a start delay requires background=true".into(),
            ));
        }
        if let Some(object) = args.as_object_mut() {
            object.remove("wait");
            object.retain(|_, value| !value.is_null());
        }
        let args: Args = tool::parse_args_hinted(args, ARGS_HINT)?;
        if args.command.trim().is_empty() {
            return Err(ToolError::Invalid(
                "command must be a nonempty string".into(),
            ));
        }
        if let Some(seconds) = after {
            return schedule(
                &args.command,
                &args.cwd,
                std::time::Duration::from_secs(seconds),
                ctx,
                self.jobs.as_ref(),
            )
            .await;
        }
        let timeout = FOREGROUND_WAIT;
        let preview = first_line(&args.command).to_string();
        let (command, cwd) = prepare(args, self.tier(), ctx).await?;
        let before = crate::drift::before(ctx);
        let launched = launch(&command, &cwd, ctx)?;
        let peek = launched.streams.peek();
        let stop = tokio_util::sync::CancellationToken::new();
        let mut running = Box::pin(wait(launched, stop.clone()));
        if !background {
            tokio::select! {
                biased;
                () = ctx.cancel.cancelled() => {
                    stop.cancel();
                    let _ = running.await;
                    return Err(ToolError::Cancelled);
                }
                ran = &mut running => {
                    let ran = ran?;
                    let moved = crate::drift::absorb(ctx, before);
                    return Ok(crate::drift::tell(ran.output(preview), &moved));
                }
                () = tokio::time::sleep(timeout) => {}
            }
        }
        let job = self.jobs.start(
            ctx.workspace.root().to_path_buf(),
            preview.clone(),
            stop.clone(),
            None,
        );
        let id = job.id();
        let again = Again { command, cwd };
        job.rerun(rerun(again.clone(), ctx), Some(again));
        job.watch(peek.clone());
        supervise(running, job);
        let said = if background {
            format!(
                "running in the background as job #{id}; its exit status and output come back on their own later. \
                 Continue independent work, or report progress and end this turn. Do not poll or wait for it."
            )
        } else {
            let elapsed = llm::figures::elapsed(timeout);
            let output = peek();
            let output = if output.is_empty() {
                "no output yet"
            } else {
                &output
            };
            let lifetime = if self.background {
                "Continue independent work, or report progress and end this turn. Its exit status and \
                 output come back on their own later. Do not poll or wait for it."
            } else {
                "This run owns the job; wait for it or stop it before finishing your task."
            };
            format!(
                "still running after {elapsed} as job #{id}; the soft timeout did not stop \
                the command. Do not rerun the command. {lifetime} Latest output:\n{output}"
            )
        };
        Ok(ToolOutput::text(said).with_preview(format!("{preview} [background #{id}]")))
    }
}

/// How the background command `again` runs again under `ctx`: the first
/// run's way, or a restarted pi's.
pub fn rerun(again: Again, ctx: &Ctx) -> Rerun {
    let ctx = spills_only(ctx);
    Arc::new(move |job, stop| {
        let launched = launch(&again.command, &again.cwd, &ctx)?;
        job.watch(launched.streams.peek());
        supervise(Box::pin(wait(launched, stop)), job);
        Ok(())
    })
}

// What a rerun keeps of the call's context: where output spills, not the
// turn's locks or progress.
fn spills_only(ctx: &Ctx) -> Ctx {
    let fresh = Ctx::new(ctx.workspace.clone());
    match ctx.session() {
        Some(id) => fresh.with_session(id, ctx.spill_root()),
        None => fresh.with_spill_root(ctx.spill_root()),
    }
}

// A shell command just spawned, not yet anyone's job.
struct Launched {
    child: tokio::process::Child,
    group: Option<u32>,
    streams: crate::process::Streams,
    guard: Option<crate::process::Group>,
}

fn launch(command: &str, cwd: &std::path::Path, ctx: &Ctx) -> Result<Launched, ToolError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    crate::process::detach(&mut cmd);
    let mut child = crate::process::spawn(&mut cmd)?;
    let group = child.id();
    let guard = group.map(crate::process::Group);
    tracing::info!(target: "pi::bash", command, cwd = %cwd.display(), ?group, "spawned");
    let streams = crate::process::Streams::of(&mut child, ctx);
    Ok(Launched {
        child,
        group,
        streams,
        guard,
    })
}

async fn wait(
    launched: Launched,
    stop: tokio_util::sync::CancellationToken,
) -> Result<Ran, ToolError> {
    let Launched {
        mut child,
        group,
        mut streams,
        guard: _guard,
    } = launched;
    let status = tokio::select! {
        status = child.wait() => status,
        () = stop.cancelled() => {
            crate::process::reap(group).await;
            // Gone, not just signalled: a restart may want its port.
            let _ = child.wait().await;
            return Err(ToolError::Cancelled);
        }
    };
    let (out, err) = tokio::select! {
        both = streams.checked() => both?,
        () = stop.cancelled() => {
            crate::process::reap(group).await;
            return Err(ToolError::Cancelled);
        }
    };
    tracing::info!(target: "pi::bash", ?group, code = ?status.as_ref().map(|s| s.code()), stdout_bytes = out.total, stderr_bytes = err.total, "exited");
    Ok(Ran {
        code: status?.code().unwrap_or(-1),
        body: body(&out, &err),
    })
}

type Running = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Ran, ToolError>> + Send>>;

fn supervise(running: Running, job: Arc<dyn JobHandle>) {
    tokio::spawn(async move {
        match running.await {
            Ok(ran) => {
                let ended = format!("exited {}", ran.code);
                job.result(if ran.body.is_empty() {
                    format!("{ended}, no output")
                } else {
                    format!("{ended}:\n{}", ran.body)
                });
            }
            Err(ToolError::Cancelled) => return,
            Err(why) => job.result(why.to_string()),
        }
        job.end();
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

/// Run `command` under the workspace's clamps — its own process group, a
/// SIGTERM-then-SIGKILL timeout capped at ten minutes, and the
/// context's cancellation.
pub async fn run(
    command: &str,
    cwd: &std::path::Path,
    timeout: std::time::Duration,
    ctx: &Ctx,
) -> Result<Ran, ToolError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    let exited = crate::process::run(cmd, None, timeout, ctx)
        .await
        .inspect_err(|e| {
            if let ToolError::Timeout { ms } = e {
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
fn body(stdout: &output::Captured, stderr: &output::Captured) -> String {
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
            Bash::scoped(Arc::new(Sink::default())).schema()["properties"]
                .get("background")
                .is_none()
        );
        for shell in [
            Bash::new(Arc::new(Sink::default())),
            Bash::scoped(Arc::new(Sink::default())),
        ] {
            assert!(shell.schema()["properties"].get("timeout_ms").is_none());
        }
        assert!(
            Bash::new(Arc::new(Sink::default())).schema()["properties"]["background"].is_object()
        );
    }

    // The call returns at once; the end comes back as one result, exit code
    // and output together, and the job ends.
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
        assert!(out.flatten().contains("job #7"), "{}", out.flatten());
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .expect("the job ended");
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
            let seen = peek();
            if seen.contains("listening on 8080") && seen.contains("warn") {
                sink.stop.lock().unwrap().take().unwrap().cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never showed: {}", peek());
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

    #[tokio::test]
    async fn a_soft_timeout_preserves_the_process_and_both_output_streams() {
        let mut cases = tokio::task::JoinSet::new();
        for background in [false, true] {
            cases.spawn(async move {
                let dir = tempfile::tempdir().unwrap();
                let sink = Arc::new(Sink::default());
                let ctx = ctx(dir.path());
                let shell = if background {
                    Bash::new(sink.clone())
                } else {
                    Bash::scoped(sink.clone())
                };
                let ended = sink.ended.notified();
                let out = tokio::time::timeout(
                    FOREGROUND_WAIT + Duration::from_secs(5),
                    shell.execute(
                        json!({"command": "echo start >> starts; echo before; while [ ! -f release ]; do sleep 0.01; done; echo after; echo warning >&2; exit 3"}),
                        &ctx,
                    ),
                )
                .await;
                let registered = sink.said.lock().unwrap().len();
                // Let the process finish even when the foreground bound regresses.
                std::fs::write(dir.path().join("release"), "").unwrap();
                let out = out.expect("a long requested wait still returns a job").unwrap();
                assert!(!out.warned);
                assert!(out.flatten().contains("job #7"), "{}", out.flatten());
                assert_eq!(registered, 1, "only registered, not ended");
                tokio::time::timeout(Duration::from_secs(5), ended)
                    .await
                    .expect("the original command finishes in the job");
                assert_eq!(
                    std::fs::read_to_string(dir.path().join("starts")).unwrap(),
                    "start\n"
                );
                let said = sink.said.lock().unwrap();
                assert_eq!(said.len(), 3, "one result and one end: {said:?}");
                for expected in ["exited 3", "before", "after", "warning"] {
                    assert!(said[1].contains(expected), "{said:?}");
                }
            });
        }
        while let Some(case) = cases.join_next().await {
            case.unwrap();
        }
    }

    #[tokio::test]
    async fn a_managed_shell_still_reports_a_failed_output_spill() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path()).with_spill_root(dir.path().join("spill"));
        std::fs::write(dir.path().join("spill"), "in the way").unwrap();
        let err = Bash::scoped(Arc::new(Sink::default()))
            .execute(json!({"command": "seq 1 200000"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("SPILL_FAILED"));
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
