use agent::{Agent, Session, Totals};
use tools::Ctx;

use crate::session::Store;

const HELP: &str = "\
/new    start a fresh session, keeping this one on disk
/todo   show the current plan
/cost   what this session has spent so far
/exit   leave (Ctrl-D does the same)";

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
    /// Carried across turns: the plan and the file locks outlive any one run.
    pub ctx: Ctx,
}

/// What a line at the prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Exit,
    Help,
    Todo,
    Cost,
    New,
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
        other => Cmd::Unknown(other.to_string()),
    })
}

pub enum Step {
    Prompt(String),
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
}
