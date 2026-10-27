use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::process::Command;

use crate::background::{Again, JobHandle, JobSink, Rerun};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default = "workspace_root")]
    cwd: String,
}

// Optional: a required cwd was still left out, and each miss cost a turn.
fn workspace_root() -> String {
    ".".to_string()
}

const ARGS_HINT: &str = "bash takes `command`, plus `cwd` to run anywhere but the workspace root";

pub struct Bash;

impl Bash {
    pub const NAME: &'static str = "bash";
}

#[async_trait]
impl Tool for Bash {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run a shell command where cwd says, in a fresh shell each call: cd and \
         environment changes do not carry over. Prefer read and write over cat \
         and heredocs, and edit over sed when the lines are picked by what they \
         say: those report failures you can act on. A substitution picked by \
         pattern or position belongs here (sed, perl, awk), and so do moves and \
         deletions (mv, rm)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_ms": { "type": "integer", "description": "Default 120000, max 600000." },
                "cwd": { "type": "string", "description": "Where the command runs, as a workspace-relative or absolute directory; omit it for the workspace root." },
            },
            "required": ["command"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tool::parse_args_hinted(args, ARGS_HINT)?;
        let timeout = tool::limit::timeout(args.timeout_ms, DEFAULT_TIMEOUT, tool::limit::MAX_CALL);
        // The preview names the command as asked: rtk's rewrite is how it
        // ran, which the journal keeps, not what the call said.
        let preview = first_line(&args.command).to_string();
        let (command, cwd) = prepare(args, self.tier(), ctx).await?;

        let before = crate::drift::before(ctx);
        // A failed call tells nothing, so what it changed is left for drift.
        let ran = run(&command, &cwd, timeout, ctx).await?;
        let moved = crate::drift::absorb(ctx, before);
        Ok(crate::drift::tell(ran.output(preview), &moved))
    }
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
pub struct WithBackground {
    jobs: Arc<dyn JobSink>,
    background: bool,
}

impl WithBackground {
    /// The shell whose background calls are kept by `jobs`.
    pub fn new(jobs: Arc<dyn JobSink>) -> Self {
        Self {
            jobs,
            background: true,
        }
    }

    /// A shell whose unfinished calls stay controllable during this run,
    /// without offering to start commands apart from it.
    pub fn scoped(jobs: Arc<dyn JobSink>) -> Self {
        Self {
            jobs,
            background: false,
        }
    }
}

#[async_trait]
impl Tool for WithBackground {
    fn name(&self) -> &str {
        Bash::NAME
    }

    fn description(&self) -> &str {
        Bash.description()
    }

    fn schema(&self) -> Value {
        let mut schema = Bash.schema();
        schema["properties"]["timeout_ms"]["description"] = json!(format!(
            "Soft timeout: how long to wait before returning a running job id and its latest \
             output. Default {}, max {}; 0 uses the default. The command keeps running \
             until it exits or you stop the job. Decide whether to wait longer or stop it.",
            DEFAULT_TIMEOUT.as_millis(),
            tool::limit::MAX_CALL.as_millis()
        ));
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
        schema
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, mut args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
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
        let args: Args = tool::parse_args_hinted(args, ARGS_HINT)?;
        let timeout = tool::limit::timeout(args.timeout_ms, DEFAULT_TIMEOUT, tool::limit::MAX_CALL);
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
                "running in the background as job #{id}; its exit status and output come back on their own later."
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
                "Its exit status and output come back on their own later."
            } else {
                "This run owns the job; wait for it or stop it before finishing your task."
            };
            format!(
                "still running after {elapsed} as job #{id}; the soft timeout did not stop \
                the command. Decide whether to wait for the job or stop it; do not rerun the \
                command. {lifetime} Latest output:\n{output}"
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

    // Only the shell that can leave its turn offers to; the plain one, which
    // a child or a one-shot run gets, never shows the choice.
    #[test]
    fn only_the_backgroundable_shell_offers_background() {
        assert!(Bash.schema()["properties"].get("background").is_none());
        assert!(
            WithBackground::new(Arc::new(Sink::default())).schema()["properties"]["background"]
                .is_object()
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
        let out = WithBackground::new(sink.clone())
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
        WithBackground::new(sink.clone())
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
        let out = WithBackground::new(Arc::new(Sink::default()))
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
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path());
        let ended = sink.ended.notified();
        let out = WithBackground::scoped(sink.clone())
            .execute(
                json!({"command": "echo start >> starts; echo before; while [ ! -f release ]; do sleep 0.01; done; echo after; echo warning >&2; exit 3", "timeout_ms": 50}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.warned);
        assert!(out.flatten().contains("job #7"), "{}", out.flatten());
        assert_eq!(
            sink.said.lock().unwrap().len(),
            1,
            "only registered, not ended"
        );
        std::fs::write(dir.path().join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), ended)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("starts")).unwrap(),
            "start\n"
        );
        let said = sink.said.lock().unwrap();
        assert_eq!(said.len(), 3, "one result and one end: {said:?}");
        for expected in ["exited 3", "before", "after", "warning"] {
            assert!(said[1].contains(expected), "{said:?}");
        }
    }

    #[tokio::test]
    async fn a_managed_shell_still_reports_a_failed_output_spill() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path()).with_spill_root(dir.path().join("spill"));
        std::fs::write(dir.path().join("spill"), "in the way").unwrap();
        let err = WithBackground::scoped(Arc::new(Sink::default()))
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
        WithBackground::new(sink.clone())
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
