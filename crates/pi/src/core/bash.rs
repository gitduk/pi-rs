//! What a `!` command ran, and what came back.
//!
//! The runner is the model's own `bash` tool: same workspace, same timeout,
//! same rewrite. What is here is the two derivations that must not drift
//! between the live row and the rebuilt one.

use agent::session::Session;
use tool::{Tool, ToolError};

/// What a `!` command left behind, kept apart because the two are easy to
/// confuse: one is written to the transcript, the other drawn on the screen.
pub struct Bashed {
    /// What the model reads: the command *and* its output.
    ///
    /// Storing only the command left the rebuilt scrollback printing
    /// `Ran \`git status\`` as a prompt with the output indented under it —
    /// the live path never did that, because it had the typed line.
    pub text: String,
    /// Said on screen only — `cancelled`, or why the runner refused. There
    /// is no entry to file, so there is nothing to rebuild from either.
    pub flash: Option<String>,
}

/// Run what `!` named. Same runner, workspace and timeout as the model's own
/// `bash` tool; `ctx` carries the token that lets Esc stop it.
///
/// Free of `Core` so the surface can spawn it: holding `&mut Core` across the
/// await pinned the whole loop, which is what left the `!` path with an event
/// loop of its own. Recording the result is the caller's, and needs no await.
pub async fn run_bash(ctx: &tool::Ctx, command: &str) -> Bashed {
    let refused = |flash: String| Bashed {
        text: String::new(),
        flash: Some(flash),
    };
    let out = match toolbox::bash::Bash
        .execute(serde_json::json!({ "command": command }), ctx)
        .await
    {
        Ok(out) => out,
        Err(ToolError::Cancelled) => return refused("cancelled".into()),
        Err(e) => return refused(format!("failed to run `{command}`: {e}")),
    };
    let body = out.flatten();
    Bashed {
        // The command that ran, which rtk may have rewritten: the head the
        // model reads back is about what happened, not what was typed.
        text: format!(
            "Ran `{}`\n{}",
            out.preview(),
            if body.is_empty() {
                "(no output)"
            } else {
                &body
            }
        ),
        flash: None,
    }
}

/// The lines the screen shows for a filed `!` run: everything under the
/// `Ran …` head, minus the stream tags the model reads. The one derivation
/// for the live path and the rebuild, so the two cannot drift.
pub fn bash_said(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter(|l| !matches!(*l, "<stdout>" | "</stdout>" | "<stderr>" | "</stderr>"))
        .map(str::to_string)
        .collect()
}

impl Bashed {
    /// What the screen shows for this run: the derived lines, plus the flash
    /// for one that never ran. Everything the surface turns into notice rows.
    pub fn screen(self) -> Vec<String> {
        let mut lines = bash_said(&self.text);
        lines.extend(self.flash);
        lines
    }
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
        image: None,
        shown: Some(format!("!{command}")),
    });
}
