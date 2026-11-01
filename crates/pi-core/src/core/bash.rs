//! What a `!` command ran, and what came back.
//!
//! Direct commands wait for completion with the shell's workspace checks and
//! rewrite. Their recorded output and rendered rows share one derivation.

use agent::session::Session;
use tool::ToolError;

/// A direct command's recorded content, actual output, and execution result.
pub struct Bashed {
    /// What the model reads: the command *and* its output.
    pub text: String,
    /// Actual output lines, without placeholders or execution diagnostics.
    pub output: Vec<String>,
    pub code: i32,
}

/// Run what `!` named to completion; `ctx` carries its cancellation token.
///
/// Free of `Core` so the surface can spawn it: holding `&mut Core` across the
/// await pinned the whole loop.
pub async fn run_bash(ctx: &tool::Ctx, command: &str) -> Result<Bashed, ToolError> {
    let out = toolbox::bash::foreground_result(command, ".", ctx).await?;
    let output = out
        .content()
        .lines()
        .filter(|line| !is_stream_tag(line))
        .map(str::to_string)
        .collect();
    let code = out.ran.code;
    let out = out.into_output();
    let body = out.flatten();
    Ok(Bashed {
        output,
        code,
        // The command as typed: rtk's rewrite is a transparent proxy, and the
        // journal keeps how it actually ran.
        text: format!(
            "Ran `{}`\n{}",
            out.preview(),
            if body.is_empty() {
                "(no output)"
            } else {
                &body
            }
        ),
    })
}

/// Recorded content without the command header or stream delimiters.
pub fn bash_said(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter(|l| !is_stream_tag(l))
        .map(str::to_string)
        .collect()
}

/// Whether a line of a command's output is one of the tags the model reads
/// its streams by, which a screen leaves out.
pub fn is_stream_tag(line: &str) -> bool {
    matches!(line, "<stdout>" | "</stdout>" | "<stderr>" | "</stderr>")
}

/// File a finished `!` command in the transcript.
///
/// `text` empty means it never ran — cancelled, or the runner refused — and
/// there is nothing the model should answer with in view.
pub fn record_bash(session: &mut Session, command: &str, text: String) {
    if text.is_empty() {
        return;
    }
    session.push_bash(agent::session::Prompt {
        text,
        images: Vec::new(),
        shown: Some(format!("!{command}")),
        relayed: None,
    });
}
