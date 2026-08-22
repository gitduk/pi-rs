use std::path::PathBuf;
use std::sync::Arc;

use agent::{Agent, AgentError, Session, Totals};
use anyhow::Result;
use rustyline::error::ReadlineError;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;
use tools::{Ctx, Workspace};

use crate::session::Store;

const BANNER: &str = "\x1b[2m/help for commands · Ctrl-C stops a run · Ctrl-D exits\x1b[0m";
const HELP: &str = "\
/new    start a fresh session, keeping this one on disk
/todo   show the current plan
/cost   what this session has spent so far
/exit   leave (Ctrl-D does the same)";

/// A prompt and everything the session accumulates around it.
pub struct Repl {
    pub agent: Agent,
    pub workspace: Workspace,
    pub store: Store,
    pub model: String,
    pub session: Session,
    pub id: String,
    pub todos: Arc<std::sync::Mutex<Vec<tools::todo::Todo>>>,
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

enum Step {
    Prompt(String),
    Handled,
    Quit,
}

impl Repl {
    fn history_path() -> Option<PathBuf> {
        crate::session::state_dir().map(|d| d.join("history"))
    }

    fn command(&mut self, line: &str, totals: &Totals) -> Step {
        let Some(cmd) = parse(line) else {
            return Step::Prompt(line.to_string());
        };
        match cmd {
            Cmd::Exit => return Step::Quit,
            Cmd::Help => println!("{HELP}"),
            Cmd::Todo => println!("{}", tools::todo::render(self.session.log.todos())),
            Cmd::Cost => {
                let u = &totals.usage;
                let cost = if totals.cost > 0.0 {
                    format!(" · ${:.4}", totals.cost)
                } else {
                    String::new()
                };
                println!(
                    "{} in / {} out · {} cached{cost}",
                    u.input, u.output, u.cache_read
                );
            }
            Cmd::New => {
                // The old one stays on disk; only the thread of conversation ends.
                self.session = Session::default();
                self.id = crate::session::new_id();
                if let Ok(mut held) = self.todos.lock() {
                    held.clear();
                }
                println!("started {}", self.id);
            }
            Cmd::Unknown(other) => println!("unknown command {other} — /help lists them"),
        }
        Step::Handled
    }

    /// One run, cancellable on its own without taking the session with it.
    async fn turn(&mut self, prompt: String, tx: &UnboundedSender<agent::Event>) -> Totals {
        self.session.log.resume(prompt);

        let cancel = CancellationToken::new();
        // Ctrl-C belongs to the run, not the process: the session survives it.
        let watcher = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                }
            }
        });

        let ctx = Ctx {
            workspace: self.workspace.clone(),
            cancel,
            todos: self.todos.clone(),
            yielded: Default::default(),
        };
        let out = self.agent.run(&mut self.session, &ctx, tx).await;
        watcher.abort();

        // Saved either way: an interrupted turn is exactly the one worth keeping.
        if let Err(e) = self.store.save(
            &self.id,
            self.workspace.root(),
            &self.model,
            &self.session.log,
        ) {
            eprintln!("warning: the transcript was not saved: {e}");
        }

        match out {
            Ok(o) => o.totals,
            Err(AgentError::Cancelled) => {
                eprintln!("\x1b[2mstopped\x1b[0m");
                Totals::default()
            }
            Err(e) => {
                eprintln!("\x1b[31merror\x1b[0m {e}");
                Totals::default()
            }
        }
    }

    pub async fn run(mut self, tx: UnboundedSender<agent::Event>) -> Result<()> {
        let mut editor = rustyline::DefaultEditor::new()?;
        let history = Self::history_path();
        if let Some(h) = &history {
            let _ = editor.load_history(h);
        }
        eprintln!("{BANNER}");

        let mut totals = Totals::default();
        loop {
            let line = match editor.readline("\x1b[36m›\x1b[0m ") {
                Ok(line) => line,
                // Ctrl-C at an empty prompt clears the line, not the program.
                Err(ReadlineError::Interrupted) => continue,
                Err(ReadlineError::Eof) => break,
                Err(e) => return Err(e.into()),
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let _ = editor.add_history_entry(line);

            match self.command(line, &totals) {
                Step::Quit => break,
                Step::Handled => continue,
                Step::Prompt(prompt) => {
                    let spent = self.turn(prompt, &tx).await;
                    totals.add(&spent.usage, spent.cost);
                }
            }
        }

        if let Some(h) = &history {
            let _ = editor.save_history(h);
        }
        Ok(())
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
