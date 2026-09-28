//! One line of text, turned into a call on the core, and what such a call
//! asks the surface to do.

pub mod commands;
pub mod complete;

use skills::Skill;

use crate::input::commands::{Command, Source};
use crate::store::listing::Listing;

#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    // What `read` makes of a submitted line.
    // Prose for the model.
    Prompt(String),
    // What `!` named.
    Bash(String),
    // A word from the table.
    Builtin(Builtin),
    // Not a built-in word: the table's skill rows land here too, because what a
    // skill *is* — a line to expand — is `step_for`'s to say, not the door's.
    Other { word: String, args: String },
}

/// The words `read` answers by name. Grouped rather than spread among the kinds
/// of input, so that the door reads four shapes and no more: prose, a shell
/// command, one of these, or a word it does not know.
#[derive(Debug, PartialEq, Eq)]
pub enum Builtin {
    Help,
    Keys,
    Status,
    Reload,
    Name(String),
    // The session to switch to, or empty to list what there is.
    Resume(String),
    // Everything after the word focuses the summary.
    Compact(String),
    // The name to move to, or empty to list what there is.
    Model(String),
    // The name to work in, or empty to list what there is.
    Worktree(String),
    // Bare `/settings`. The panel is the whole surface; the line verbs are
    // gone, and an argument after the word is refused.
    Settings(String),
    // A channel's command: its name, then "" = status, "on" or "off".
    Channel(&'static str, String),
    // What to run over and over, or empty to stop the loop in force.
    Loop(String),
    // `/new`, and `ctrl+l` twice: a fresh session, the old one kept on disk,
    // and the screen rebuilt from the empty one. One variant, because they
    // are one intent however it was expressed.
    New,
    // Leave now — `/exit`, `/quit`, `ctrl+d`, a double `ctrl+c`. One intent,
    // so the four of them cannot answer differently. What a key can ask for
    // and a line cannot is not here: those are the screen's own deeds, in
    // `ui/tui`; a key that means a command, like `ctrl+l` twice for `/new`,
    // arrives here already read.
    Quit,
}

/// What a line may do while a turn is in flight.
///
/// Read off the parsed command rather than off the `Step` it produces:
/// `command` has already had its effect by the time it hands one back, so a
/// `Step` can say what happened but never whether it should have.
#[derive(Debug)]
pub enum Fate {
    // Touches nothing the run is standing on.
    Now,
    // Goes to the model, or needs the surface free, so it waits.
    Queued,
    // Prose, while a run is working: it goes to the run rather than waiting
    // for it, and is read at the run's next turn boundary.
    //
    // Carries the text because answering it took a read of the line, and
    // reading the line a second time is how two readings drift apart.
    Steered(String),
    // Would move what the run stands on. Says this rather than doing it.
    Refused(&'static str),
}

impl Intent {
    /// Whether the surface shows the line this was read from, above the answer.
    ///
    /// It does when the answer lands under it: a turn streams rows below the
    /// line, and a `!` command's output is filed as one. It does not when the
    /// answer is a command's, which goes to the reply over the menu — the reply
    /// is dismissed rather than kept, so a row left behind would be a question
    /// standing with no answer under it. The line is still in the recall list
    /// either way, and still in the history file.
    pub fn echoed(&self) -> bool {
        !matches!(self, Intent::Builtin(_))
    }

    /// Exhaustive on purpose, with no catch-all arm: an intent added without an
    /// answer here should fail to compile rather than default to one.
    ///
    /// What it cannot check is an arm's body: `Reload` and `Model` are `Now`
    /// because they write through `Arc::make_mut`, not because a rule says so.
    pub fn fate(&self) -> Fate {
        match self {
            // Answered from the config, the key map or the lane's own tally
            // — none of which the run is holding.
            Intent::Builtin(Builtin::Help | Builtin::Keys | Builtin::Status | Builtin::Name(_)) => {
                Fate::Now
            }
            Intent::Builtin(Builtin::Reload | Builtin::Model(_)) => Fate::Now,
            // Bare, these only list what there is.
            Intent::Builtin(Builtin::Resume(name) | Builtin::Worktree(name))
                if name.trim().is_empty() =>
            {
                Fate::Now
            }
            Intent::Builtin(Builtin::Resume(_)) => Fate::Refused(
                "/resume would replace the transcript this run is writing — esc first",
            ),
            // A lane of its own to move to, and the one being left keeps
            // working in the tree it was already in.
            Intent::Builtin(Builtin::Worktree(_)) => Fate::Now,
            Intent::Builtin(Builtin::New) => {
                Fate::Refused("/new would replace the transcript this run is writing — esc first")
            }
            Intent::Builtin(Builtin::Compact(_)) => {
                Fate::Refused("/compact rewrites the transcript this run is writing — esc first")
            }
            // An argument is a refusal, and a refusal answers now; bare opens
            // a panel, which wants the surface to itself.
            Intent::Builtin(Builtin::Settings(rest)) if !rest.trim().is_empty() => Fate::Now,
            Intent::Builtin(Builtin::Settings(_)) => Fate::Queued,
            Intent::Builtin(Builtin::Channel(..)) => Fate::Now,
            Intent::Other { .. } => Fate::Queued,
            // Arms the lane and submits its first round like a typed line;
            // both want the lane free.
            Intent::Builtin(Builtin::Loop(_)) => Fate::Queued,
            // Prose reaches the run that is already talking to the model:
            // waiting for it is what makes a correction arrive too late to be
            // one.
            Intent::Prompt(text) => Fate::Steered(text.clone()),
            // A `!` files its result in the transcript, which the run has.
            Intent::Bash(_) => Fate::Queued,
            // Both reach for the transcript, and the run is holding it.
            // Leaving is never refused: a hung run must not trap the user.
            Intent::Builtin(Builtin::Quit) => Fate::Now,
        }
    }
}

// Slash commands are recognized before anything reaches the model, so a line
// that merely starts with a slash never becomes a prompt by accident.
// A command that changed nothing, said once to the user and once to the
// journal. Nothing-happened is the hardest kind of bug to read back: the
// terminal has scrolled and the config on disk is whatever it is now.
pub(crate) fn refused(what: &str, e: anyhow::Error) -> String {
    let detail = format!("{e:#}");
    tracing::warn!(target: "pi::session", command = what, error = %detail, "refused");
    detail
}

// A skill command as a message the user could have typed, or why it could not
// be read.
//
// The body goes in whole rather than as an instruction to go and fetch it:
// `/commit` says the user has already chosen those instructions, and a model
// that must call the `skill` tool to learn what it just agreed to has spent a
// turn on a decision that was made before it was asked.
fn expanded(skill: &Skill, args: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(skill.dir.join("SKILL.md")).map_err(|e| {
        let why = refused(&skill.name, anyhow::anyhow!("{}: {e}", skill.dir.display()));
        format!("cannot run {} — {why}", skill.name)
    })?;
    let mut out = format!(
        "Run the `{}` skill. Its instructions follow.\n\n{}",
        skill.name,
        skills::instructions(skill, &text)
    );
    if !args.is_empty() {
        // Below the instructions, so the skill is read as the standing order
        // and this as what it is being applied to.
        out.push_str(&format!("\n---\n{args}\n"));
    }
    tracing::info!(
        target: "pi::session",
        skill = %skill.name,
        bytes = out.len(),
        args = !args.is_empty(),
        "skill invoked"
    );
    Ok(out)
}

// The skill a word names, if it names one. A built-in never reaches here —
// `parse` has already turned those into their own variants.
fn skill_for<'a>(commands: &'a [Command], word: &str) -> Option<&'a Skill> {
    match &commands.iter().find(|c| c.word.as_ref() == word)?.source {
        Source::Skill(skill) => Some(skill),
        Source::Builtin => None,
    }
}

/// Whether `line` comes back with Up: what the user said, and a skill — a
/// skill command is a prompt wearing a slash. Built-ins are operations
/// rather than words to re-say, and a word pi does not know is nothing.
pub(crate) fn recallable(line: &str, commands: &[Command]) -> bool {
    match line.split_whitespace().next() {
        Some(word) if word.starts_with('/') => skill_for(commands, word).is_some(),
        _ => true,
    }
}

// A word `parse` did not know: a skill to run, or a typo to name.
pub(crate) fn step_for(commands: &[Command], word: &str, args: &str) -> Step {
    let Some(skill) = skill_for(commands, word) else {
        return Step::Flash(format!("unknown command {word} — /help lists them"));
    };
    match expanded(skill, args) {
        Ok(send) => Step::Prompt {
            typed: Some(format!("/{} {args}", skill.name).trim_end().to_string()),
            send,
        },
        Err(why) => lines(why),
    }
}

/// What a one-shot prompt turns into when it names a skill.
///
/// `pi "/commit fix the tests"` means at the command line what it means at the
/// terminal. That is the whole guarantee, and it is deliberately narrower than
/// the terminal's: everything else that starts with a slash is left alone.
///
/// The built-ins are operations on a session, and a run that answers once has
/// no session for them to operate on. A word that names nothing is not a typo
/// to be refused either, because here the argument is a prompt rather than a
/// line at a prompt — `pi "/usr/bin is missing"` and `pi "/2 of the tests
/// fail"` are prose, and refusing them to catch `/comit` trades a recoverable
/// mistake for an unrecoverable one. The model can ask what `/comit` meant; a
/// user whose sentence was rejected has to reword it.
pub fn expand(commands: &[Command], line: &str) -> Option<Result<String, String>> {
    let Intent::Other { word, args } = read(line, commands) else {
        return None;
    };
    Some(expanded(skill_for(commands, &word)?, &args))
}

// What a line starting with `!` asks to run, when it names a command.
//
// `!` alone is prose (a prompt, like any other line); `!cmd` runs `cmd`.
// `!!cmd` keeps its second bang: in shell grammar `! cmd` negates the exit
// code, which is what a non-interactive shell will do with it.
fn bash_command(line: &str) -> Option<&str> {
    let cmd = line.strip_prefix('!')?.trim();
    (!cmd.is_empty()).then_some(cmd)
}

/// What a submitted line is asking for. Total on purpose: every line means
/// something, and a line naming no command is prose for the model.
///
/// `!` is read before the slash words, because a shell command is not one.
pub fn read(line: &str, commands: &[Command]) -> Intent {
    if let Some(command) = bash_command(line) {
        return Intent::Bash(command.to_string());
    }
    let Some(word) = line.split_whitespace().next() else {
        return Intent::Prompt(line.to_string());
    };
    if !word.starts_with('/') {
        return Intent::Prompt(line.to_string());
    }
    let args = rest(line);
    // The table is the only list of words there is: a built-in row says what it
    // means, a skill's row hands the line on, and a word no row matches is the
    // same nothing as a skill is, until `step_for` says otherwise.
    match commands.iter().find(|c| c.word == word) {
        Some(found) => (found.intent)(word, args),
        None => Intent::Other {
            word: word.to_string(),
            args,
        },
    }
}

// Whatever followed the command word.
fn rest(line: &str) -> String {
    line.trim_start()
        .split_once(char::is_whitespace)
        .map_or(String::new(), |(_, r)| r.trim().to_string())
}

/// What a rewind did. Which one it is says whether the entry was unsent or
/// kept, rather than leaving that to be read off whether a string was there.
pub enum Rewound {
    // The id named nothing the transcript still holds.
    Nothing,
    // The entry stayed, and what followed it went.
    Kept,
    // The entry went too, and its text belongs back in the editor.
    Unsent(String),
}

#[derive(Debug)]
pub enum Step {
    // One line whose whole content is "nothing happened": a verb that is not
    // one, a command refused, a checkout you are already in. No state moved
    // and there is no detail to come back to, so a minute later the line says
    // nothing the screen does not already show.
    //
    // Said where a one-line answer goes — the reply over the editor — and
    // filed nowhere: there is nothing here a rebuild should draw. An error
    // carrying detail is the other side of that line and stays in the
    // transcript.
    Flash(String),
    // A `!` command to run. The surface runs and records it, because only it
    // can await; `run_bash` does the work and `record_bash` files it.
    Bash(String),
    // What to send, and — when a skill expanded into it — the line that was
    // typed. `rewind_nodes()` reads the second: a rewind menu offering four
    // thousand characters of `SKILL.md` is offering the wrong thing, and so
    // is a session named after one.
    Prompt { send: String, typed: Option<String> },
    // Needs the network, so the surface runs it and reports.
    Compact(Option<String>),
    // Starts or stops the named channel. Needs the network, so the surface
    // runs it and reports — the same rule as `Compact`.
    Channel(&'static str, ChannelCmd),
    // The settings panel wants the surface to itself, and only a surface with
    // one can answer: bare `/settings` asks for it, an argument is a `Flash`.
    Panel,
    // Dealt with here; this is what there is to show for it. Returned rather
    // than laid out: the surface decides how wide the columns are.
    Handled(Listing),
    // The session was replaced — a `/new` or a `/resume` — so the surface
    // has to rebuild its view from the new one, not just show the rows.
    Swap(Listing),
    // The set of worktrees changed under the surface — a removal — so it
    // shows the rows and forgets the cached list, which would keep naming
    // the checkout that just went.
    Worktrees(Listing),
    Quit,
}

/// What `/<channel>` asks: bare for status, `on` or `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelCmd {
    Status,
    On,
    Off,
}

pub(crate) fn lines(text: impl Into<String>) -> Step {
    Step::Handled(Listing::say(text.into().lines()))
}
