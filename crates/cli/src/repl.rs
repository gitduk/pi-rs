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
        word: "/model",
        args: "[name]",
        help: "list the models in ~/.pi.toml, or move this session to one",
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
        word: "/reload",
        args: "",
        help: "re-read ~/.pi.toml, the instructions and the skills",
    },
    Command {
        word: "/log",
        args: "",
        help: "where this run is writing its journal",
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

/// A model the prompt can complete to, and what tells it apart from the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub note: String,
}

/// One thing the line could still become.
///
/// Owned rather than borrowed from the table, because half the candidates come
/// from the config and none of those are `'static`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Shown in the list.
    pub show: String,
    /// What the whole line becomes when this is accepted.
    pub line: String,
    pub help: String,
    /// Something is still expected after it, so accepting leaves a trailing
    /// space and the caret past it.
    pub more: bool,
}

/// What the line could still become: a command while its word is being typed,
/// then that command's own argument once the word is settled.
///
/// Only `/model` has an argument worth completing. A prompt is prose and a
/// focus phrase is prose; guessing at either is worse than leaving it alone.
pub fn complete(line: &str, models: &[Choice]) -> Vec<Candidate> {
    if !line.starts_with('/') {
        return Vec::new();
    }
    let Some((word, rest)) = line.split_once(char::is_whitespace) else {
        return COMMANDS
            .iter()
            .filter(|c| c.word.starts_with(line))
            // An exact and only match is already typed; offering it is noise.
            .filter(|c| c.word != line)
            .map(|c| Candidate {
                show: format!("{} {}", c.word, c.args).trim_end().to_string(),
                line: c.word.to_string(),
                help: c.help.to_string(),
                more: !c.args.is_empty(),
            })
            .collect();
    };
    if word != "/model" {
        return Vec::new();
    }
    let typed = rest.trim_start();
    // A second word means the name is settled and something else is being
    // typed. There is no third thing to offer.
    if typed.contains(char::is_whitespace) {
        return Vec::new();
    }
    models
        .iter()
        .filter(|m| m.name.starts_with(typed) && m.name != typed)
        .map(|m| Candidate {
            show: m.name.clone(),
            line: format!("/model {}", m.name),
            help: m.note.clone(),
            more: false,
        })
        .collect()
}

/// A session and everything that outlives any one turn of it.
///
/// Both surfaces hold one of these and differ only in how they read a line and
/// where they put what comes back.
pub struct Repl {
    pub agent: Agent,
    pub store: Store,
    pub session: Session,
    pub id: String,
    /// What the user calls this session, if anything.
    pub name: Option<String>,
    /// Held so `/keys` can show what is actually in force, overrides included.
    pub keys: std::sync::Arc<crate::keys::Keys>,
    /// The config in force, as opposed to the one on disk. `/model` picks from
    /// this, so a switch cannot quietly apply an edit `/reload` has not.
    pub config: std::sync::Arc<crate::config::Config>,
    /// The command line, kept because it outranks the config and so has to be
    /// re-applied over every reload.
    pub args: std::sync::Arc<crate::Args>,
    /// Carried across turns: the plan and the file locks outlive any one run.
    pub ctx: Ctx,
}

impl Repl {
    /// Re-read the config and everything it decides.
    ///
    /// Whole or not at all: on any failure nothing changes, which is why the
    /// new state is computed in full before a field is touched. Pi separates
    /// global from project so a broken one of each does not take the other
    /// down; here the whole reload is refused instead, and what was running
    /// keeps running — the case that separation protects against cannot arise
    /// when nothing is applied.
    ///
    /// What the session owns is untouched by construction: the transcript, the
    /// plan, the name, the history, the model. Only what the config decides is
    /// replaced.
    pub fn reload(&mut self) -> Vec<String> {
        let config = match crate::config::load(self.args.config.as_deref()) {
            Ok(c) => c,
            Err(e) => return vec![format!("nothing reloaded — {}", refused("reload", e))],
        };
        let project = match crate::config::load_project(self.ctx.workspace.root()) {
            Ok(p) => p,
            Err(e) => return vec![format!("nothing reloaded — {}", refused("reload", e))],
        };
        let resolved =
            match crate::resolve(&self.args, self.ctx.workspace.root(), &config, &project) {
                Ok(r) => r,
                Err(e) => return vec![format!("nothing reloaded — {}", refused("reload", e))],
            };

        let changed = resolved.system != self.agent.system;
        self.agent.registry = resolved.registry;
        self.agent.approver = std::sync::Arc::new(agent::Ceiling(resolved.tier));
        self.agent.system = resolved.system;
        self.agent.effort = resolved.effort;
        self.agent.max_turns = resolved.max_turns;
        self.keys = std::sync::Arc::new(resolved.keys);
        // The running model is deliberately not re-dialled: a reload re-reads
        // preferences, and which model this session is on was a decision, not a
        // preference. `/model` is how that one changes.
        self.config = std::sync::Arc::new(config);

        tracing::info!(
            target: "pi::session",
            models = self.config.models.len(),
            rebound_keys = self.config.keys.len(),
            max_turns = self.agent.max_turns,
            effort = ?self.agent.effort,
            system_bytes = self.agent.system.len(),
            prompt_changed = changed,
            "reloaded"
        );

        let mut said = resolved.notes;
        said.push(if changed {
            // Worth saying: the prompt is what a provider caches, so a changed
            // one starts the cache over. An unchanged one costs nothing, which
            // is why there is no narrower `/reload keys`.
            "reloaded — the instructions changed, so the prompt cache starts over".into()
        } else {
            "reloaded".into()
        });
        said
    }

    /// The models `/model` can reach, with what tells them apart.
    ///
    /// Empty under `--wire`: the command line named one endpoint directly and
    /// the config is not consulted, so there is no list to pick from — only a
    /// wire id to type.
    pub fn choices(&self) -> Vec<Choice> {
        if self.args.wire.is_some() {
            return Vec::new();
        }
        self.config
            .models
            .iter()
            .map(|(name, entry)| Choice {
                name: name.clone(),
                note: summary(
                    entry.wire.transport_name(),
                    entry.context_window,
                    &entry.pricing,
                ),
            })
            .collect()
    }

    /// Move this session to another model.
    ///
    /// The transcript comes with it. Reasoning blocks carry the model that
    /// produced them and every transport demotes one it did not write —
    /// signature dropped, replayed as text or as `<think>` per the new model's
    /// `thinking_replay` — so the history stays sendable instead of becoming a
    /// 400 on the next turn. Nothing is rewritten on the way: switch back and
    /// the original blocks are native again.
    ///
    /// What has been spent stays spent. Each turn was priced by the spec in
    /// force when it ran, and the total is the sum of those, so a switch to a
    /// dearer model does not reprice the cheap turns behind it.
    pub fn switch(&mut self, name: &str) -> Vec<String> {
        let dialled = match crate::dial(
            &self.args,
            &self.config,
            name,
            crate::config::Origin::Command,
        ) {
            Ok(d) => d,
            Err(e) => {
                let held = self.agent.spec.id.clone();
                return vec![format!("still on {held} — {}", refused("switch", e))];
            }
        };
        // Compared after resolving, not before: `find` accepts a model's
        // `wire_id` as well as its table name, so the name typed and the id it
        // lands on need not be the same string. Comparing the typed one would
        // re-dial the model already running and then announce a reasoning
        // demotion that never happened.
        if dialled.spec.id == self.agent.spec.id {
            return vec![format!("already on {}", self.agent.spec.id)];
        }
        let mut said: Vec<String> = dialled.warning.into_iter().chain(dialled.notes).collect();
        let spec = &dialled.spec;
        said.push(format!(
            "now on {} · {}",
            spec.id,
            summary(spec.transport_name(), spec.context_window, &spec.pricing)
        ));
        if carries_reasoning(&self.session.log) {
            said.push(demotion(spec.thinking_replay).into());
        }
        tracing::info!(
            target: "pi::session",
            from = %self.agent.spec.id,
            to = %spec.id,
            wire = spec.transport_name(),
            context_window = spec.context_window,
            "model switched"
        );
        self.agent.spec = dialled.spec;
        self.agent.transport = dialled.transport;
        said
    }

    /// What `/model` on its own shows.
    fn listing(&self) -> Vec<String> {
        let here = &self.agent.spec.id;
        // Two different reasons the list can be empty, and reporting the wrong
        // one sends the reader to the wrong file.
        if self.args.wire.is_some() {
            return vec![
                format!("on {here}, at the endpoint --base-url named"),
                "--wire bypasses ~/.pi.toml, so `/model <id>` asks that same endpoint \
                 for another of its models"
                    .into(),
            ];
        }
        let choices = self.choices();
        if choices.is_empty() {
            return vec![
                format!("on {here}, and ~/.pi.toml now defines no model to switch to"),
                "see examples/pi.toml for what a [models.<name>] entry looks like".into(),
            ];
        }
        let width = choices.iter().map(|c| c.name.len()).max().unwrap_or(0);
        choices
            .iter()
            .map(|c| {
                let mark = if &c.name == here { "●" } else { " " };
                format!("{mark} {:width$}  {}", c.name, c.note)
            })
            .collect()
    }

    /// Save the transcript. Called after every turn: an interrupted one is
    /// exactly the one worth keeping.
    pub fn save(&self) -> anyhow::Result<()> {
        self.store.save(
            &self.id,
            self.ctx.workspace.root(),
            &self.agent.spec.id,
            self.name.as_deref(),
            &self.session.log,
        )?;
        Ok(())
    }
}

/// Enough about a model to choose between them: who serves it, how much it
/// holds, and what it costs where that is known.
///
/// Takes the three pieces rather than a config entry, because the running model
/// may never have been one — an ad-hoc `--wire` spec has no entry to read.
fn summary(wire: &str, window: u32, p: &brain::catalog::Pricing) -> String {
    let mut parts = vec![wire.to_string(), format!("{}k", window / 1000)];
    if p.input_per_mtok > 0.0 || p.output_per_mtok > 0.0 {
        parts.push(format!(
            "${:.2}/${:.2} per Mtok",
            p.input_per_mtok, p.output_per_mtok
        ));
    }
    parts.join(" · ")
}

/// What becomes of the transcript's reasoning once another model is reading it.
///
/// Only ever asked about a model that did not write it — the origin recorded on
/// each block cannot match after a switch — so the signed path is out and one of
/// these three is what the transport will do with it.
fn demotion(replay: brain::catalog::ThinkingReplay) -> &'static str {
    use brain::catalog::ThinkingReplay as R;
    match replay {
        R::Signed | R::BareProse => "reasoning from the earlier turns replays as plain text",
        R::Tagged => "reasoning from the earlier turns replays wrapped in <think> tags",
        R::Drop => "reasoning from the earlier turns is dropped rather than replayed",
    }
}

/// Whether the transcript holds any prior-turn reasoning at all.
///
/// Worth saying at a switch: it is the one part of the history that does not
/// survive intact, and a model that suddenly reads its own earlier thinking as
/// quoted prose is otherwise an unexplained change in tone.
fn carries_reasoning(log: &agent::log::Log) -> bool {
    // `live`, not `messages`: what compaction has already dropped is not going
    // to reach the new model in any form, demoted or otherwise.
    log.live().iter().any(|(_, m)| {
        matches!(m, brain::message::Message::Assistant { content, .. }
            if content.iter().any(|b| matches!(b, brain::message::AssistantContent::Reasoning(_))))
    })
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
    Reload,
    Log,
    /// Everything after the word, or empty to clear.
    Name(String),
    /// Everything after the word focuses the summary.
    Compact(String),
    /// The name to move to, or empty to list what there is.
    Model(String),
    Unknown(String),
}

/// Slash commands are recognized before anything reaches the model, so a line
/// that merely starts with a slash never becomes a prompt by accident.
/// A command that changed nothing, said once to the user and once to the
/// journal. Nothing-happened is the hardest kind of bug to read back: the
/// terminal has scrolled and the config on disk is whatever it is now.
fn refused(what: &str, e: anyhow::Error) -> String {
    let detail = format!("{e:#}");
    tracing::warn!(target: "pi::session", command = what, error = %detail, "refused");
    detail
}

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
        "/reload" => Cmd::Reload,
        "/log" => Cmd::Log,
        "/name" => Cmd::Name(rest(line)),
        "/compact" => Cmd::Compact(rest(line)),
        "/model" => Cmd::Model(rest(line)),
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
            Cmd::Reload => Step::Handled(self.reload()),
            Cmd::Log => lines(match crate::journal::path() {
                Some(p) => format!("{}", p.display()),
                None => "not recording — --log is off, or the file would not open".into(),
            }),
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
                crate::journal::now_recording(&self.id);
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
            Cmd::Model(name) => Step::Handled(if name.is_empty() {
                self.listing()
            } else {
                self.switch(&name)
            }),
            Cmd::Unknown(other) => lines(format!("unknown command {other} — /help lists them")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, Candidate, Choice, Cmd, complete, parse};

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

    fn choices() -> Vec<Choice> {
        ["flash", "flint", "sonnet"]
            .iter()
            .map(|n| Choice {
                name: (*n).to_string(),
                note: "openai · 128k".into(),
            })
            .collect()
    }

    fn offered(line: &str) -> Vec<String> {
        complete(line, &choices())
            .into_iter()
            .map(|c| c.show)
            .collect()
    }

    #[test]
    fn completion_narrows_as_the_word_is_typed() {
        assert_eq!(offered("/n"), ["/new", "/name [text]"]);
        assert_eq!(offered("/na"), ["/name [text]"]);
        // Already whole: there is nothing left to offer.
        assert!(offered("/name").is_empty());
        // Past a word with nothing to complete, the rest is prose.
        assert!(offered("/name the flaky test").is_empty());
        // Not a command at all.
        assert!(offered("what does /help do").is_empty());
        assert!(offered("").is_empty());
    }

    #[test]
    fn accepting_a_command_that_wants_an_argument_leaves_room_for_one() {
        let of = |line: &str| -> Candidate { complete(line, &choices()).swap_remove(0) };
        let name = of("/nam");
        assert_eq!((name.line.as_str(), name.more), ("/name", true));
        // Nothing follows /todo, so the caret should not be pushed past a space
        // the user then has to delete.
        let todo = of("/tod");
        assert_eq!((todo.line.as_str(), todo.more), ("/todo", false));
    }

    #[test]
    fn the_models_complete_once_the_command_word_is_settled() {
        // The whole point: the name is the tedious part to type, and it is the
        // one thing the config already knows.
        assert_eq!(offered("/model fl"), ["flash", "flint"]);
        assert_eq!(offered("/model fla"), ["flash"]);
        // A bare space offers all of them rather than nothing.
        assert_eq!(offered("/model "), ["flash", "flint", "sonnet"]);
        // Whole already, and there is no second argument behind it.
        assert!(offered("/model flash").is_empty());
        assert!(offered("/model flash and").is_empty());
        // Accepting one replaces the line, not just the word.
        assert_eq!(complete("/model fla", &choices())[0].line, "/model flash");
    }

    #[test]
    fn a_config_with_no_models_offers_nothing_rather_than_every_command() {
        // The `--wire` case: choices() is empty there, and the argument branch
        // must not fall back to completing command words again.
        assert!(complete("/model fl", &[]).is_empty());
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
