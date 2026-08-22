use agent::{Agent, Session, Totals};
use tools::Ctx;

use crate::session::Store;

/// One command: the word, what it takes, and what it does.
pub struct Command {
    pub word: &'static str,
    /// Shown in completion, in the shape prompt templates use elsewhere:
    /// angle brackets required, square brackets optional.
    pub args: &'static str,
    pub help: &'static str,
}

/// Every command, once. `parse` still maps words to typed variants, but the
/// help text and the completion list are generated from here — a new command
/// that reached only one of the three was the bug this table prevents.
pub const COMMANDS: &[Command] = &[
    Command {
        word: "/new",
        args: "",
        help: "start a fresh session, keeping this one on disk",
    },
    Command {
        word: "/name",
        args: "[text]",
        help: "call this session something you will recognise",
    },
    Command {
        word: "/compact",
        args: "[focus]",
        help: "summarize everything but what you are working on now",
    },
    Command {
        word: "/todo",
        args: "",
        help: "show the current plan",
    },
    Command {
        word: "/cost",
        args: "",
        help: "what this session has spent so far",
    },
    Command {
        word: "/keys",
        args: "",
        help: "what every key does, and the id to rebind it under",
    },
    Command {
        word: "/help",
        args: "",
        help: "this list",
    },
    Command {
        word: "/exit",
        args: "",
        help: "leave (Ctrl-D does the same)",
    },
];

fn help() -> Vec<String> {
    let width = COMMANDS
        .iter()
        .map(|c| c.word.len() + c.args.len() + 1)
        .max()
        .unwrap_or(0);
    COMMANDS
        .iter()
        .map(|c| {
            let head = format!("{} {}", c.word, c.args);
            format!("{head:width$}  {}", c.help)
        })
        .collect()
}

/// Commands the line could still become, while the command word is still being
/// typed. Empty once there is whitespace: the word is settled by then and what
/// follows is its argument.
pub fn complete(line: &str) -> Vec<&'static Command> {
    if !line.starts_with('/') || line.contains(char::is_whitespace) {
        return Vec::new();
    }
    COMMANDS
        .iter()
        .filter(|c| c.word.starts_with(line))
        // An exact and only match is already typed; offering it is noise.
        .filter(|c| c.word != line)
        .collect()
}

/// A session and everything that outlives any one turn of it.
///
/// Both surfaces hold one of these and differ only in how they read a line and
/// where they put what comes back.
pub struct Repl {
    pub agent: Agent,
    pub store: Store,
    pub model: String,
    pub session: Session,
    pub id: String,
    /// What the user calls this session, if anything.
    pub name: Option<String>,
    /// Held so `/keys` can show what is actually in force, overrides included.
    pub keys: std::sync::Arc<crate::keys::Keys>,
    /// Carried across turns: the plan and the file locks outlive any one run.
    pub ctx: Ctx,
}

impl Repl {
    /// Save the transcript. Called after every turn: an interrupted one is
    /// exactly the one worth keeping.
    pub fn save(&self) -> anyhow::Result<()> {
        self.store.save(
            &self.id,
            self.ctx.workspace.root(),
            &self.model,
            self.name.as_deref(),
            &self.session.log,
        )?;
        Ok(())
    }
}

/// What a line at the prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Exit,
    Help,
    Todo,
    Cost,
    New,
    Keys,
    /// Everything after the word, or empty to clear.
    Name(String),
    /// Everything after the word focuses the summary.
    Compact(String),
    Unknown(String),
}

/// Slash commands are recognized before anything reaches the model, so a line
/// that merely starts with a slash never becomes a prompt by accident.
pub fn parse(line: &str) -> Option<Cmd> {
    let word = line.split_whitespace().next()?;
    if !word.starts_with('/') {
        return None;
    }
    Some(match word {
        "/exit" | "/quit" => Cmd::Exit,
        "/help" => Cmd::Help,
        "/todo" => Cmd::Todo,
        "/cost" => Cmd::Cost,
        "/new" => Cmd::New,
        "/keys" => Cmd::Keys,
        "/name" => Cmd::Name(rest(line)),
        "/compact" => Cmd::Compact(rest(line)),
        other => Cmd::Unknown(other.to_string()),
    })
}

/// Whatever followed the command word.
fn rest(line: &str) -> String {
    line.trim_start()
        .split_once(char::is_whitespace)
        .map_or(String::new(), |(_, r)| r.trim().to_string())
}

pub enum Step {
    Prompt(String),
    /// Needs the network, so the surface runs it and reports.
    Compact(Option<String>),
    /// Dealt with here; these lines are what there is to show for it. Returned
    /// rather than printed because one surface prints and the other paints.
    Handled(Vec<String>),
    Quit,
}

fn lines(text: impl Into<String>) -> Step {
    Step::Handled(text.into().lines().map(str::to_string).collect())
}

impl Repl {
    pub fn command(&mut self, line: &str, totals: &Totals) -> Step {
        let Some(cmd) = parse(line) else {
            return Step::Prompt(line.to_string());
        };
        match cmd {
            Cmd::Exit => Step::Quit,
            Cmd::Help => Step::Handled(help()),
            Cmd::Keys => Step::Handled(self.keys.listing()),
            Cmd::Todo => lines(tools::todo::render(self.session.log.todos())),
            Cmd::Cost => {
                let u = &totals.usage;
                let cost = if totals.cost > 0.0 {
                    format!(" · ${:.4}", totals.cost)
                } else {
                    String::new()
                };
                lines(format!(
                    "{} in / {} out · {} cached{cost}",
                    u.input, u.output, u.cache_read
                ))
            }
            Cmd::New => {
                // The old one stays on disk; only the thread of conversation ends.
                self.session = Session::default();
                self.id = crate::session::new_id();
                if let Ok(mut held) = self.ctx.todos.lock() {
                    held.clear();
                }
                lines(format!("started {}", self.id))
            }
            Cmd::Name(name) => {
                if name.is_empty() {
                    self.name = None;
                    lines(format!("{} is unnamed again", self.id))
                } else {
                    let said = format!("{} is now “{name}”", self.id);
                    self.name = Some(name);
                    lines(said)
                }
            }
            Cmd::Compact(focus) => Step::Compact(Some(focus).filter(|f| !f.is_empty())),
            Cmd::Unknown(other) => lines(format!("unknown command {other} — /help lists them")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, Cmd, complete, parse};

    #[test]
    fn every_listed_command_actually_parses() {
        // The table drives help and completion; a word that reached the list
        // without reaching `parse` would offer to complete into nothing.
        for c in COMMANDS {
            assert!(
                !matches!(parse(c.word), Some(Cmd::Unknown(_)) | None),
                "{} is listed but does not parse",
                c.word
            );
        }
    }

    #[test]
    fn completion_narrows_as_the_word_is_typed() {
        let words = |s: &str| -> Vec<&str> { complete(s).iter().map(|c| c.word).collect() };
        assert_eq!(words("/n"), vec!["/new", "/name"]);
        assert_eq!(words("/na"), vec!["/name"]);
        // Already whole: there is nothing left to offer.
        assert!(words("/name").is_empty());
        // Past the word, the rest is an argument.
        assert!(words("/name the flaky test").is_empty());
        // Not a command at all.
        assert!(words("what does /help do").is_empty());
        assert!(words("").is_empty());
    }

    #[test]
    fn slash_words_are_commands_and_prose_is_not() {
        assert_eq!(parse("/exit"), Some(Cmd::Exit));
        assert_eq!(parse("/quit"), Some(Cmd::Exit));
        assert_eq!(parse("/todo"), Some(Cmd::Todo));
        assert_eq!(parse("fix the bug"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn a_typo_is_named_rather_than_sent_to_the_model() {
        // Otherwise `/tood` becomes a prompt and the model has to guess.
        assert_eq!(parse("/tood"), Some(Cmd::Unknown("/tood".into())));
    }

    #[test]
    fn a_prompt_that_merely_mentions_a_slash_stays_a_prompt() {
        assert_eq!(parse("what does /help do?"), None);
        assert_eq!(parse("read src/main.rs"), None);
    }

    #[test]
    fn trailing_words_do_not_break_a_command() {
        assert_eq!(parse("/todo please"), Some(Cmd::Todo));
    }

    #[test]
    fn a_command_that_takes_words_keeps_all_of_them() {
        assert_eq!(
            parse("/name the flaky test"),
            Some(Cmd::Name("the flaky test".into()))
        );
        assert_eq!(
            parse("/compact  keep the parser work "),
            Some(Cmd::Compact("keep the parser work".into()))
        );
        // Bare, they mean clear and unfocused respectively.
        assert_eq!(parse("/name"), Some(Cmd::Name(String::new())));
        assert_eq!(parse("/compact"), Some(Cmd::Compact(String::new())));
    }
}
