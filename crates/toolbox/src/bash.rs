use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use tool::{Ctx, Tier, Tool, ToolError, ToolOutput, output};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
}

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
        "Run a shell command in the workspace. Each call is a fresh shell: cd and \
         environment changes do not carry over — pass cwd rather than prefixing \
         cd. Prefer read and write over cat and heredocs, and edit over sed \
         when the lines are picked by what they say: those report failures you \
         can act on. A substitution picked by pattern or position belongs here \
         (sed, perl, awk), and so do moves and deletions (mv, rm)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "timeout_ms": { "type": "integer", "description": "Default 120000, max 600000." },
                "cwd": { "type": "string", "description": "Workspace-relative. Default the workspace root." },
            },
            "required": ["command"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Exec
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tool::parse_args(args)?;
        // A zero would otherwise kill the command before it started.
        let timeout = std::time::Duration::from_millis(
            args.timeout_ms
                .filter(|ms| *ms > 0)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
        );
        let (command, cwd) = prepare(args, self.tier(), ctx).await?;

        let ran = run(&command, &cwd, timeout, ctx).await?;
        let mut body = ran.body;
        if ran.code != 0 {
            body.push_str(&format!("exit {}\n", ran.code));
        }
        // One line: the diff-row renderer and journal treat further newlines as
        // structure, not text.
        let preview = command.split('\n').next().unwrap_or_default();
        if body.is_empty() {
            // Preview stays set: without it the row falls back to body, hiding
            // what a silent command was asked to do.
            return Ok(ToolOutput::text("exit 0, no output").with_preview(preview));
        }
        Ok(ToolOutput::text(body).with_preview(preview))
    }
}

// The command as it will run, and where: rtk rewrites it in its own
// vocabulary, from `cwd` for project filters, racing esc so it isn't delayed.
async fn prepare(
    args: Args,
    tier: Tier,
    ctx: &Ctx,
) -> Result<(String, std::path::PathBuf), ToolError> {
    let cwd = match &args.cwd {
        Some(p) => ctx.workspace.resolve(p, tier)?,
        None => ctx.workspace.root().to_path_buf(),
    };
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
            "description": "Run it apart from this turn — an import, a build, a deploy you need \
                not wait on: the call returns at once with a job id, and the exit status and \
                output come back later as a turn of its own. No timeout applies; its output is \
                still captured, so leave it unredirected.",
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
        let (command, cwd) = prepare(tool::parse_args(args)?, self.tier(), ctx).await?;
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
        let mut stdout = crate::process::drain(child.stdout.take().expect("stdout is piped"), ctx);
        let mut stderr = crate::process::drain(child.stderr.take().expect("stderr is piped"), ctx);

        let preview = command.split('\n').next().unwrap_or_default().to_string();
        let stop = tokio_util::sync::CancellationToken::new();
        let job = sink.start(
            ctx.workspace.root().to_path_buf(),
            preview.clone(),
            stop.clone(),
            None,
        );
        let id = job.id();
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
            "running in the background as job #{id}; its exit status and output come back as \
             a turn of its own, so keep working or end this turn. `jobs` lists it, and `jobs` \
             with `stop` stops it."
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
                json!({"command": "sleep 0.2; echo imported 40 rows; exit 3", "background": true}),
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

    #[tokio::test]
    async fn without_background_it_is_the_plain_shell() {
        let dir = tempfile::tempdir().unwrap();
        let out = WithBackground
            .execute(
                json!({"command": "echo hi", "background": false}),
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
                json!({"command": "true", "background": true}),
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
                json!({"command": "echo $$ > pid; sleep 30", "background": true}),
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
