use agent::{Agent, Session, Totals};
use tools::Ctx;

use crate::session::Store;

const HELP: &str = "\
/new       start a fresh session, keeping this one on disk
/name      call this session something you will recognise
/compact   summarize everything but what you are working on now
/todo      show the current plan
/cost      what this session has spent so far
/exit      leave (Ctrl-D does the same)";

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
            Cmd::Help => lines(HELP),
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
    use super::{Cmd, parse};

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
