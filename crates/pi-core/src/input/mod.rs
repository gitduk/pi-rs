//! One line of text, turned into a call on the core, and what such a call
//! asks the surface to do.

pub mod commands;
pub mod complete;

use skills::Skill;

use crate::input::commands::{Command, Source};
use pi_store::listing::Listing;

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
    Content,
    Name(String),
    // The session to switch to, or empty to list what there is.
    Resume(String),
    // Everything after the word focuses the summary.
    Compact(String),
    // The name to move to, or empty to list what there is.
    Model(String),
    // The thinking effort to run at, or empty to say the one in force.
    Effort(String),
    // The name to work in, or empty to list what there is.
    Worktree(String),
    // Bare `/settings`: the project's `.pi.toml` in `$EDITOR`. An argument
    // after the word is refused.
    Settings(String),
    // A file the model is given, by the name `/edit` lists it under, or
    // empty to list them.
    Edit(String),
    // A channel's command: its name, then "" = status, "on" or "off".
    Channel(String, String),
    // What to run over and over, or empty to stop the loop in force.
    Loop(String),
    // Bare, what is left for later here; `rm <id>` cancels one.
    Later(String),
    // Bare, the MCP servers and their tools; `restart [name]` reconnects.
    Mcp(String),
    // `/new` and `ctrl+l` twice are one variant: a fresh session, old one
    // kept on disk, screen rebuilt empty — one intent, however expressed.
    New,
    // Leave now — `/exit`, `/quit`, `ctrl+d`, double `ctrl+c` — one intent,
    // so the four cannot answer differently.
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
    // Would move what the run stands on. Says this rather than doing it.
    Refused(&'static str),
}

impl Intent {
    /// Whether the surface shows the line this was read from, above the answer.
    ///
    /// Shown when the answer lands under it (a turn, a `!`'s output); not for
    /// a command's reply, which is dismissed rather than kept.
    pub fn echoed(&self) -> bool {
        !matches!(self, Intent::Builtin(_))
    }

    /// Exhaustive on purpose, with no catch-all arm: an intent added without an
    /// answer here should fail to compile rather than default to one.
    ///
    /// What it cannot check is an arm's body: `Model` is `Now` because it
    /// writes through `Arc::make_mut`, not because a rule says so.
    pub fn fate(&self) -> Fate {
        match self {
            // Answered from the config, the key map or the lane's own tally
            // — none of which the run is holding.
            Intent::Builtin(
                Builtin::Help
                | Builtin::Keys
                | Builtin::Status
                | Builtin::Content
                | Builtin::Name(_)
                | Builtin::Model(_)
                | Builtin::Effort(_)
                | Builtin::Later(_),
            ) => Fate::Now,
            // A restart drops only the servers' connections, never the run's.
            Intent::Builtin(Builtin::Mcp(_)) => Fate::Now,
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
            // An argument is a refusal, and a refusal answers now; bare hands
            // the terminal to an editor and reloads, which waits for the run.
            Intent::Builtin(Builtin::Settings(rest)) if !rest.trim().is_empty() => Fate::Now,
            Intent::Builtin(Builtin::Settings(_)) => Fate::Queued,
            // Bare it lists; naming a file hands the terminal to an editor.
            Intent::Builtin(Builtin::Edit(name)) if name.trim().is_empty() => Fate::Now,
            Intent::Builtin(Builtin::Edit(_)) => Fate::Queued,
            Intent::Builtin(Builtin::Channel(..)) => Fate::Now,
            Intent::Other { .. } | Intent::Prompt(_) => Fate::Queued,
            // Its first round is due at once, and a round wants the lane free.
            Intent::Builtin(Builtin::Loop(_)) => Fate::Queued,
            // A `!` files its result in the transcript, which the run has.
            Intent::Bash(_) => Fate::Queued,
            // Both reach for the transcript, and the run is holding it.
            // Leaving is never refused: a hung run must not trap the user.
            Intent::Builtin(Builtin::Quit) => Fate::Now,
        }
    }
}

// Said once to the user and once to the journal: nothing-happened is hard
// to read back later, once the terminal has scrolled and disk has moved on.
pub(crate) fn refused(what: &str, e: anyhow::Error) -> String {
    let detail = format!("{e:#}");
    tracing::warn!(target: "pi::session", command = what, error = %detail, "refused");
    detail
}

// Body goes in whole, not fetched later: the model shouldn't spend a turn
// re-learning instructions `/commit` says the user already chose.
fn expanded(skill: &Skill, args: &str) -> Result<String, String> {
    let text = skill.text().map_err(|e| {
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
// `read` has already turned those into their own variants.
pub(crate) fn skill_for<'a>(commands: &'a [Command], word: &str) -> Option<&'a Skill> {
    match &commands.iter().find(|c| c.word.as_ref() == word)?.source {
        Source::Skill(skill) => Some(skill),
        Source::Builtin | Source::Prompt(_) => None,
    }
}

/// Whether `word` names a command that becomes a turn: a skill or a prompt.
pub(crate) fn starts_turn(commands: &[Command], word: &str) -> bool {
    commands
        .iter()
        .find(|c| c.word.as_ref() == word)
        .is_some_and(|c| !matches!(c.source, Source::Builtin))
}

/// Whether `line` comes back with Up: what the user said, and a skill — a
/// skill command is a prompt wearing a slash. Built-ins are operations
/// rather than words to re-say, and a word pi does not know is nothing.
pub fn recallable(line: &str, commands: &[Command]) -> bool {
    match line.split_whitespace().next() {
        Some(word) if word.starts_with('/') => starts_turn(commands, word),
        _ => true,
    }
}

// A word `read` did not know: a skill to run, or a typo to name.
pub(crate) fn step_for(commands: &[Command], word: &str, args: &str) -> Step {
    let typed = || format!("{word} {args}").trim_end().to_string();
    let skill = match commands
        .iter()
        .find(|c| c.word.as_ref() == word)
        .map(|c| &c.source)
    {
        Some(Source::Skill(skill)) => skill,
        Some(Source::Prompt(prompt)) => {
            return Step::McpPrompt {
                prompt: prompt.clone(),
                args: args.to_string(),
                typed: typed(),
            };
        }
        _ => return Step::Flash(format!("unknown command {word} — /help lists them")),
    };
    match expanded(skill, args) {
        Ok(send) => Step::Prompt {
            typed: Some(typed()),
            send,
        },
        Err(why) => lines(why),
    }
}

/// What a one-shot prompt turns into when it names a skill.
///
/// `pi "/commit ..."` means what it means at the terminal; everything else
/// starting with a slash is left alone — deliberately narrower.
///
/// An unknown word reads as prose, not a refused typo: refusing trades a
/// recoverable "what did you mean" for an unrecoverable one.
pub fn expand(commands: &[Command], line: &str) -> Option<Result<String, String>> {
    let Intent::Other { word, args } = read(line, commands) else {
        return None;
    };
    Some(expanded(skill_for(commands, &word)?, &args))
}

// `!` alone is prose; `!cmd` runs `cmd`. `!!cmd` keeps its second bang:
// shell grammar reads `! cmd` as negating the exit code.
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
    // The table is the only list of words: a built-in row says what it
    // means, a skill's hands the line on; no match is nothing till `step_for`.
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
    // "Nothing happened" shown once over the editor, filed nowhere — no
    // state moved, so there's nothing for a rebuild to draw later.
    Flash(String),
    // A `!` command to run. The surface runs and records it, because only it
    // can await; `run_bash` does the work and `record_bash` files it.
    Bash(String),
    // What to send, and the typed line when a skill expanded into it —
    // `rewind_nodes()` reads the latter so a menu doesn't offer raw SKILL.md.
    Prompt {
        send: String,
        typed: Option<String>,
    },
    // Needs the network, so the surface runs it and reports.
    Compact(Option<String>),
    // An MCP prompt to ask its server for, then send as a turn showing
    // `typed`. Asked off the loop: a slow server must not hold the screen.
    McpPrompt {
        prompt: mcp::Prompt,
        args: String,
        typed: String,
    },
    // A command for what drives a lane from outside. The surface hands it
    // to the drivers and places what they answer.
    Drive(Drive),
    // A file to open in `$EDITOR`, everything it feeds rebuilt after: the
    // editor takes the terminal, which only the surface can give away.
    Edit(std::path::PathBuf),
    // Dealt with here; this is what there is to show for it. Returned rather
    // than laid out: the surface decides how wide the columns are.
    Handled(Listing),
    // The session was replaced — a `/new` or a `/resume` — so the surface
    // has to rebuild its view from the new one, not just show the rows.
    Swap(Listing),
    // The set of worktrees changed (a removal): show the rows and drop the
    // cached list, which would keep naming the checkout that just went.
    Worktrees(Listing),
    Quit,
}

/// A command for a driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drive {
    /// `/<channel>`, which starts, stops or reports the named channel.
    Channel(String, ChannelCmd),
    /// `/loop` over a goal already known to start a turn; `None` stops it.
    Loop(Option<String>),
    /// `/later`, with what followed the word.
    Later(String),
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
