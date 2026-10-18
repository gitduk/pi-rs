use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::process::Command;

use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    cwd: String,
}

// Said when `cwd` is missing: the refusal is how a call learns it is required.
const ARGS_HINT: &str = "bash takes `command` and `cwd`; `cwd` is \".\" for the workspace root";

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
                "cwd": { "type": "string", "description": "Where the command runs: \".\" for the workspace root, else a workspace-relative or absolute directory." },
            },
            "required": ["command", "cwd"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tool::parse_args_hinted(args, ARGS_HINT)?;
        // A zero would otherwise kill the command before it started.
        let timeout = std::time::Duration::from_millis(
            args.timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        );
        // The preview names the command as asked: rtk's rewrite is how it
        // ran, which the journal keeps, not what the call said.
        let preview = first_line(&args.command).to_string();
        let (command, cwd) = prepare(args, self.tier(), ctx).await?;

        let ran = run(&command, &cwd, timeout, ctx).await?;
        let mut body = ran.body;
        if ran.code != 0 {
            body.push_str(&format!("exit {}\n", ran.code));
        }
        if body.is_empty() {
            // Preview stays set: without it the row falls back to body, hiding
            // what a silent command was asked to do.
            return Ok(ToolOutput::text("exit 0, no output").with_preview(preview));
        }
        let out = ToolOutput::text(body).with_preview(preview);
        Ok(if ran.code != 0 { out.warned() } else { out })
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

/// `bash` for a run that keeps jobs: a call may leave its turn with
/// `background`, its exit status and output coming back as a turn of its own.
pub struct WithBackground;

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
        if !background {
            return Bash.execute(args, ctx).await;
        }
        let Some(sink) = ctx.jobs() else {
            return Err(ToolError::Invalid(
                "this run cannot keep a command in the background; run it without `background`"
                    .into(),
            ));
        };
        let args: Args = tool::parse_args_hinted(args, ARGS_HINT)?;
        let preview = first_line(&args.command).to_string();
        let (command, cwd) = prepare(args, self.tier(), ctx).await?;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = crate::process::spawn(&mut cmd)?;
        let group = child.id();
        let tail = crate::process::Tail::default();
        let out_pipe = tail.tee(child.stdout.take().expect("stdout is piped"));
        let err_pipe = tail.tee(child.stderr.take().expect("stderr is piped"));
        let mut stdout = crate::process::drain(out_pipe, ctx);
        let mut stderr = crate::process::drain(err_pipe, ctx);

        let stop = tokio_util::sync::CancellationToken::new();
        let job = sink.start(
            ctx.workspace.root().to_path_buf(),
            preview.clone(),
            stop.clone(),
            None,
        );
        let id = job.id();
        job.watch(Arc::new(move || tail.read()));
        let guard = group.map(crate::process::Group);
        let name = preview.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let status = tokio::select! {
                status = child.wait() => status,
                () = stop.cancelled() => {
                    crate::process::reap(group).await;
                    return;
                }
            };
            let Some((out, err)) =
                crate::process::drained(&mut stdout, &mut stderr, group, &stop).await
            else {
                return;
            };
            let ended = match status {
                Ok(status) => format!("`{name}` exited {}", status.code().unwrap_or(-1)),
                Err(e) => format!("`{name}` could not be waited on: {e}"),
            };
            let said = body(&out, &err);
            job.result(
                if said.is_empty() {
                    format!("{ended}, no output")
                } else {
                    format!("{ended}:\n{said}")
                },
                Default::default(),
            );
            job.end();
        });
        Ok(ToolOutput::text(format!(
            "running in the background as job #{id}; its exit status and output come back on \
             their own later."
        ))
        .with_preview(format!("{preview} [background #{id}]")))
    }
}

/// What a finished command left behind.
pub struct Ran {
    pub code: i32,
    /// What the command printed, plus a spill note for what did not fit and
    /// any note explaining the failure. Empty when it succeeded silently.
    pub body: String,
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
        assert!(WithBackground.schema()["properties"]["background"].is_object());
    }

    // The call returns at once; the end comes back as one result, exit code
    // and output together, and the job ends.
    #[tokio::test]
    async fn a_background_command_reports_its_end_as_one_result() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        let ended = sink.ended.notified();
        let out = WithBackground
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
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        WithBackground
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
        let out = WithBackground
            .execute(
                json!({"cwd": ".", "command": "echo hi", "background": false}),
                &ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.flatten().contains("hi"), "{}", out.flatten());
    }

    #[tokio::test]
    async fn background_is_refused_where_the_run_keeps_no_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let err = WithBackground
            .execute(
                json!({"cwd": ".", "command": "true", "background": true}),
                &ctx(dir.path()),
            )
            .await
            .expect_err("refused");
        assert!(err.to_string().contains("cannot keep"), "{err}");
    }

    #[tokio::test]
    async fn stopping_a_background_command_takes_its_group_and_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Sink::default());
        let ctx = ctx(dir.path()).with_jobs(sink.clone());
        WithBackground
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
