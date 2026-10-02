//! The model's shell command, in its `rtk` form, when rtk is installed.
//!
//! The rewrite table lives in rtk's own registry and is reached by running
//! `rtk rewrite` — a copy here would drift from the one every other agent
//! uses. What is left on this side is the delegation: probe once, ask per
//! command, and run the command as written whenever rtk has nothing to say.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::OnceCell;

/// rtk as it installs itself. A path would be a config field nothing else in
/// pi needs: the binary is on `PATH` or it is not.
const BINARY: &str = "rtk";

/// `rtk rewrite` answers with one command line. This caps what a broken or
/// hostile binary can make pi hold; it is not a format limit.
const MAX_ANSWER: u64 = 64 * 1024;

/// A lookup, not work: long enough for a loaded machine, short enough that a
/// wedged binary does not hold a command that would otherwise have run.
const TIMEOUT: Duration = Duration::from_secs(2);

/// `command` in its rtk form, or `None` to run it as written.
///
/// Asked from `cwd`, which is where `.rtk/filters.toml` lives; fails open
/// throughout — no binary, an old one, no answer, or no filter all return `None`.
pub async fn rewrite(command: &str, cwd: &Path) -> Option<String> {
    // The switch rtk's own hooks read, so a machine that turned rtk off for
    // one agent turns it off here too.
    if command.trim().is_empty() || disabled() || command.starts_with("rtk ") {
        return None;
    }
    if !installed().await {
        return None;
    }
    let (code, answer) = ask(&["rewrite", command], Some(cwd)).await.ok().flatten()?;
    // 0 and 3 (host-confirmed) rewrite; 1 is no rtk equivalent; 2 is another
    // agent's own deny — not pi's concern, since pi's tier is the real gate.
    if code != 0 && code != 3 {
        return None;
    }
    let rewritten = answer.trim();
    if rewritten.is_empty() || rewritten == command {
        return None;
    }
    Some(rewritten.to_string())
}

fn disabled() -> bool {
    std::env::var_os("RTK_DISABLED").is_some_and(|v| v.to_str() == Some("1"))
}

/// Whether rtk answers at all, asked once and cached — except a timed-out
/// probe, left uncached so one slow moment isn't a session without rtk.
async fn installed() -> bool {
    static INSTALLED: OnceCell<bool> = OnceCell::const_new();
    INSTALLED
        .get_or_try_init(|| async { probe().await.ok_or(()) })
        .await
        .copied()
        .unwrap_or(false)
}

/// `Some` when rtk answered at all — an old binary answers here but 1 per
/// command instead, one extra spawn and no behavior change. `None` is a timeout.
async fn probe() -> Option<bool> {
    match ask(&["--version"], None).await {
        Ok(Some((code, _))) => Some(code == 0),
        // It cannot be started at all: asking again would fail the same way.
        Err(_) => Some(false),
        Ok(None) => None,
    }
}

/// `rtk`'s exit code and stdout; `Ok(None)` on timeout, `Err` if it never
/// started. Both pipes are drained live, capped at [`MAX_ANSWER`], deadline [`TIMEOUT`].
async fn ask(args: &[&str], cwd: Option<&Path>) -> Result<Option<(i32, String)>, std::io::Error> {
    let mut cmd = Command::new(BINARY);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        // Piped above, so this is unreachable; asking anyway beats a panic in
        // a tool that runs for a turn.
        return Err(std::io::Error::other("rtk's output pipe was not there"));
    };
    let answered = async {
        let (out, _) = tokio::join!(drain(stdout), drain(stderr));
        let status = child.wait().await.ok()?;
        Some((status.code().unwrap_or(-1), out))
    };
    // A timeout drops the future and, with it, the child it holds.
    Ok(tokio::time::timeout(TIMEOUT, answered)
        .await
        .unwrap_or(None))
}

async fn drain(pipe: impl AsyncRead + Unpin) -> String {
    let mut buf = Vec::new();
    let _ = pipe.take(MAX_ANSWER).read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::rewrite;

    #[tokio::test]
    async fn a_command_already_in_rtk_form_is_left_alone() {
        // Answered without asking rtk — or reading the directory — so this
        // holds with or without it.
        let here = std::env::temp_dir();
        assert_eq!(rewrite("rtk git status", &here).await, None);
        assert_eq!(rewrite("   ", &here).await, None);
    }
}
