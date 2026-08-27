use std::borrow::Cow;

use agent::session::Session;
use agent::{Agent, Totals};
use tokio_util::sync::CancellationToken;
use tools::{Ctx, Tool, ToolError};
use tools::skills::Skill;

use crate::session::Store;

/// Where a command came from, and so what running it means.
#[derive(Clone)]
pub enum Source {
    /// A word `parse` knows and `command` answers itself.
    Builtin,
    /// A `SKILL.md` to read and hand to the model as if the user had typed it.
    Skill(Skill),
}

/// One command: the word, what it takes, what it does, and what it is.
#[derive(Clone)]
pub struct Command {
    /// With the leading slash.
    pub word: Cow<'static, str>,
    /// Shown in completion, in the shape prompt templates use elsewhere:
    /// angle brackets required, square brackets optional.
    pub args: &'static str,
    /// One line of it. A skill's description is written for the model and is
    /// routinely longer than a line, so it arrives here already cut down.
    pub help: Cow<'static, str>,
    pub source: Source,
}

impl Command {
    const fn builtin(word: &'static str, args: &'static str, help: &'static str) -> Self {
        Self {
            word: Cow::Borrowed(word),
            args,
            help: Cow::Borrowed(help),
            source: Source::Builtin,
        }
    }
}

/// Every built-in command, once. `parse` still maps words to typed variants,
/// but the help text and the completion list are generated from here — a new
/// command that reached only one of the three was the bug this table prevents.
///
/// Nothing reads this directly except `commands`, which appends the skills to
/// it. What a run answers to is settled when the workspace is known, not when
/// the binary is built.
const BUILTIN: &[Command] = &[
    Command::builtin(
        "/new",
        "",
        "start a fresh session, keeping this one on disk",
    ),
    Command::builtin(
        "/name",
        "[text]",
        "call this session something you will recognise",
    ),
    Command::builtin(
        "/model",
        "[name]",
        "list the models in ~/.pi.toml, or move this session to one",
    ),
    Command::builtin(
        "/compact",
        "[focus]",
        "summarize everything but what you are working on now",
    ),
    Command::builtin("/todo", "", "show the current plan"),
    Command::builtin("/cost", "", "what this session has spent so far"),
    Command::builtin(
        "/reload",
        "",
        "re-read ~/.pi.toml, the instructions and the skills",
    ),
    Command::builtin("/log", "", "where this run is writing its journal"),
    Command::builtin(
        "/keys",
        "",
        "what every key does, and the id to rebind it under",
    ),
    Command::builtin("/help", "", "this list"),
    Command::builtin("/exit", "", "leave (Ctrl-D does the same)"),
];

/// How wide a one-line description may be before it is cut.
const GIST: usize = 60;

/// A description written for the model, cut down to a line for a list.
///
/// Two cuts, because they answer different questions: the first sentence is
/// where the description stops being a summary, and the column is where the
/// terminal stops having room.
fn gist(description: &str) -> String {
    let first = description
        .split_once(". ")
        .map_or(description, |(head, _)| head);
    crate::render::clip(first.trim().trim_end_matches('.'), GIST)
}

/// What a slash answers to: the built-ins, then one command per skill.
///
/// No prefix. A skill is `/commit`, not `/skill:commit`, because the name is
/// what it is known by and a namespace only earns its keep when something else
/// is competing for the word. What does compete is a built-in, and the built-in
/// wins: a repository contributes skills, and one that could take `/new` away
/// from the session it would otherwise start is a checkout redefining the
/// terminal. The skill itself is untouched — the model can still load it by
/// name — and the note says which of the two happened, because a command that
/// silently is not there is one the user goes looking for in the wrong place.
pub fn commands(skills: &[Skill], notes: &mut Vec<String>) -> Vec<Command> {
    let mut out = BUILTIN.to_vec();
    for skill in skills {
        let word = format!("/{}", skill.name);
        if out.iter().any(|c| c.word.as_ref() == word) {
            notes.push(format!(
                "skill `{}` has no {word} — that word is a built-in command; \
                 the model can still load the skill by name",
                skill.name
            ));
            continue;
        }
        out.push(Command {
            word: Cow::Owned(word),
            args: "[text]",
            help: Cow::Owned(gist(&skill.description)),
            source: Source::Skill(skill.clone()),
        });
    }
    out
}

fn help(commands: &[Command]) -> Vec<String> {
    let width = commands
        .iter()
        .map(|c| c.word.len() + c.args.len() + 1)
        .max()
        .unwrap_or(0);
    let row = |c: &Command| {
        let head = format!("{} {}", c.word, c.args);
        format!("{head:width$}  {}", c.help)
    };
    // The break is where the built-ins end, not where the skills begin: a third
    // source would otherwise land silently in the half that looks built in.
    // `commands` keeps the built-ins first and contiguous, so one position
    // settles it.
    let Some(split) = commands
        .iter()
        .position(|c| !matches!(c.source, Source::Builtin))
    else {
        return commands.iter().map(row).collect();
    };
    let mut out: Vec<String> = commands[..split].iter().map(row).collect();
    // Without a prefix there is nothing in the word itself to say which half it
    // came from, so the list says it once.
    out.push(String::new());
    out.push("skills — the instructions load when you run one:".into());
    out.extend(commands[split..].iter().map(row));
    out
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
pub fn complete(line: &str, commands: &[Command], models: &[Choice]) -> Vec<Candidate> {
    if !line.starts_with('/') {
        return Vec::new();
    }
    let Some((word, rest)) = line.split_once(char::is_whitespace) else {
        // The exact word stays in the list. Dropping it would leave only the
        // longer `/news` when `/new` is typed in full, and Tab would hand the
        // line to the wrong command.
        return commands
            .iter()
            .filter(|c| c.word.starts_with(line))
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
    /// What a slash answers to, built-ins and skills together. Rebuilt by
    /// `/reload`, because a skill can appear between one turn and the next.
    ///
    /// Shared rather than copied, like the key map beside it: the terminal
    /// holds the same table to complete against and re-reads it whenever this
    /// one is replaced.
    pub commands: std::sync::Arc<Vec<Command>>,
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
        // A skill can appear between one turn and the next, so the table of
        // what a slash answers to is recomputed like everything else here.
        self.commands = std::sync::Arc::new(resolved.commands);
        // The running model is deliberately not re-dialled: a reload re-reads
        // preferences, and which model this session is on was a decision, not a
        // preference. `/model` is how that one changes.
        self.config = std::sync::Arc::new(config);

        tracing::info!(
            target: "pi::session",
            models = self.config.models.len(),
            rebound_keys = self.config.keys.len(),
            commands = self.commands.len(),
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
        if carries_reasoning(&self.session) {
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
        self.agent.retarget(dialled.transport, dialled.spec);
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
            &self.session,
        )?;
        Ok(())
    }

    /// Rewind the conversation to a user message and write the shorter
    /// transcript back. How many entries were dropped; an unknown id drops
    /// nothing.
    pub fn rewind_to(&mut self, user: agent::session::EntryId) -> anyhow::Result<usize> {
        let dropped = self.session.rollback_to(user);
        if dropped == 0 {
            return Ok(0);
        }
        self.save()?;
        Ok(dropped)
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
fn carries_reasoning(session: &agent::session::Session) -> bool {
    // `live`, not `messages`: what compaction has already dropped is not going
    // to reach the new model in any form, demoted or otherwise.
    session.live().iter().any(|(_, m)| {
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
    /// Not a built-in word. It may name a skill and it may name nothing; the
    /// command table settles that, and `parse` does not have it.
    Other {
        word: String,
        args: String,
    },
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

/// A skill command as a message the user could have typed, or why it could not
/// be read.
///
/// The body goes in whole rather than as an instruction to go and fetch it:
/// `/commit` says the user has already chosen those instructions, and a model
/// that must call the `skill` tool to learn what it just agreed to has spent a
/// turn on a decision that was made before it was asked.
fn expanded(skill: &Skill, args: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(skill.dir.join("SKILL.md")).map_err(|e| {
        let why = refused(&skill.name, anyhow::anyhow!("{}: {e}", skill.dir.display()));
        format!("cannot run {} — {why}", skill.name)
    })?;
    let mut out = format!(
        "Run the `{}` skill. Its instructions follow.\n\n{}",
        skill.name,
        tools::skill::instructions(skill, &text)
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

/// The skill a word names, if it names one. A built-in never reaches here —
/// `parse` has already turned those into their own variants.
fn skill_for<'a>(commands: &'a [Command], word: &str) -> Option<&'a Skill> {
    match &commands.iter().find(|c| c.word.as_ref() == word)?.source {
        Source::Skill(skill) => Some(skill),
        Source::Builtin => None,
    }
}

/// A word `parse` did not know: a skill to run, or a typo to name.
fn dispatch(commands: &[Command], word: &str, args: &str) -> Step {
    let Some(skill) = skill_for(commands, word) else {
        return lines(format!("unknown command {word} — /help lists them"));
    };
    match expanded(skill, args) {
        Ok(text) => Step::Prompt(text),
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
    let Cmd::Other { word, args } = parse(line)? else {
        return None;
    };
    Some(expanded(skill_for(commands, &word)?, &args))
}

/// What a line starting with `!` asks to run, when it names a command.
///
/// `!` alone is prose (a prompt, like any other line); `!cmd` runs `cmd`.
/// `!!cmd` keeps its second bang: in shell grammar `! cmd` negates the exit
/// code, which is what a non-interactive shell will do with it.
fn bash_command(line: &str) -> Option<&str> {
    let cmd = line.strip_prefix('!')?.trim();
    (!cmd.is_empty()).then_some(cmd)
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
        other => Cmd::Other {
            word: other.to_string(),
            args: rest(line),
        },
    })
}

/// Whatever followed the command word.
fn rest(line: &str) -> String {
    line.trim_start()
        .split_once(char::is_whitespace)
        .map_or(String::new(), |(_, r)| r.trim().to_string())
}

pub enum Step {
    /// A `!` command to run. The surface executes it and records the result,
    /// because only it can await; `Repl::bash` does the actual work.
    Bash(String),
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
        if let Some(command) = bash_command(line) {
            return Step::Bash(command.to_string());
        }
        let Some(cmd) = parse(line) else {
            return Step::Prompt(line.to_string());
        };
        match cmd {
            Cmd::Exit => Step::Quit,
            Cmd::Help => Step::Handled(help(&self.commands)),
            Cmd::Keys => Step::Handled(self.keys.listing()),
            Cmd::Reload => Step::Handled(self.reload()),
            Cmd::Log => lines(match crate::journal::path() {
                Some(p) => format!("{}", p.display()),
                None => "not recording — --log is off, or the file would not open".into(),
            }),
            Cmd::Todo => lines(tools::todo::render(self.session.todos())),
            Cmd::Cost => lines(crate::render::spent(
                &totals.usage,
                totals.cost,
                totals.estimated,
            )),
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
            Cmd::Other { word, args } => dispatch(&self.commands, &word, &args),
        }
    }

    /// Run what `!` named: show the output, and record the command and its
    /// result in the transcript so the model answers with it in view. Same
    /// runner, workspace and timeout as the model's own `bash` tool. The
    /// surface hands in a token so Ctrl-C (line mode) or Esc (terminal) can
    /// stop the command instead of leaving the caller stuck for the timeout.
    pub async fn bash(&mut self, command: &str, cancel: CancellationToken) -> Vec<String> {
        let ctx = self.ctx.clone().with_cancel(cancel);
        let out = tools::bash::Bash
            .execute(serde_json::json!({ "command": command }), &ctx)
            .await;
        let out = match out {
            Ok(out) => out,
            Err(ToolError::Cancelled) => return vec!["cancelled".into()],
            Err(e) => return vec![format!("failed to run `{command}`: {e}")],
        };
        let body = out.flatten();
        let text = format!(
            "Ran `{command}`\n{}",
            if body.is_empty() { "(no output)" } else { &body }
        );
        self.session.append_user(text);
        let mut said: Vec<String> = body
            .lines()
            // The tags wrap the model's copy; the terminal shows the output
            // itself.
            .filter(|l| !matches!(*l, "<stdout>" | "</stdout>" | "<stderr>" | "</stderr>"))
            .map(str::to_string)
            .collect();
        if let Err(e) = self.save() {
            said.push(format!("warning: the transcript was not saved: {e}"));
        }
        said
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BUILTIN, Candidate, Choice, Cmd, Command, Source, Step, bash_command, commands,
        complete, dispatch, expand, gist, help, parse,
    };
    use tools::skills::Skill;

    #[test]
    fn every_listed_command_actually_parses() {
        // The table drives help and completion; a word that reached the list
        // without reaching `parse` would offer to complete into nothing.
        for c in BUILTIN {
            assert!(
                !matches!(parse(&c.word), Some(Cmd::Other { .. }) | None),
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

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            dir: std::path::PathBuf::from("/nowhere").join(name),
        }
    }

    fn table() -> Vec<Command> {
        commands(&[], &mut Vec::new())
    }

    fn offered_from(line: &str, table: &[Command]) -> Vec<String> {
        complete(line, table, &choices())
            .into_iter()
            .map(|c| c.show)
            .collect()
    }

    fn offered(line: &str) -> Vec<String> {
        offered_from(line, &table())
    }

    #[test]
    fn completion_narrows_as_the_word_is_typed() {
        assert_eq!(offered("/n"), ["/new", "/name [text]"]);
        assert_eq!(offered("/na"), ["/name [text]"]);
        // A word typed in full stays first: the menu must not drop it and
        // leave only a longer one that starts the same way.
        assert_eq!(offered("/name"), ["/name [text]"]);
        // Past a word with nothing to complete, the rest is prose.
        assert!(offered("/name the flaky test").is_empty());
        // Not a command at all.
        assert!(offered("what does /help do").is_empty());
        assert!(offered("").is_empty());
    }

    #[test]
    fn an_exact_word_stays_first_when_it_prefixes_another() {
        // `/news` is a skill here; `/new` typed in full keeps first place in
        // the menu, so Tab accepts the built-in rather than sliding onto news.
        let found = [skill("news", "headlines from the fixed feeds")];
        let table = commands(&found, &mut Vec::new());
        assert_eq!(offered_from("/new", &table), ["/new", "/news [text]"]);
    }
    #[test]
    fn accepting_a_command_that_wants_an_argument_leaves_room_for_one() {
        let of = |line: &str| -> Candidate { complete(line, &table(), &choices()).swap_remove(0) };
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
        assert_eq!(
            complete("/model fla", &table(), &choices())[0].line,
            "/model flash"
        );
    }

    #[test]
    fn a_config_with_no_models_offers_nothing_rather_than_every_command() {
        // The `--wire` case: choices() is empty there, and the argument branch
        // must not fall back to completing command words again.
        assert!(complete("/model fl", &table(), &[]).is_empty());
    }

    #[test]
    fn a_skill_is_a_command_under_its_own_name() {
        // No prefix: the name is what the user knows the skill by.
        let found = [skill("commit", "Use when ready to commit changes")];
        let table = commands(&found, &mut Vec::new());
        assert_eq!(
            offered_from("/com", &table),
            ["/compact [focus]", "/commit [text]"]
        );
        assert!(matches!(
            table
                .iter()
                .find(|c| c.word == "/commit")
                .map(|c| &c.source),
            Some(Source::Skill(_))
        ));
    }

    #[test]
    fn a_skill_cannot_take_a_built_in_word() {
        // A repository contributes skills, and one that could redefine /new
        // would be a checkout taking the session over.
        let found = [skill("new", "not this one"), skill("archify", "diagrams")];
        let mut notes = Vec::new();
        let table = commands(&found, &mut notes);
        let new = table.iter().find(|c| c.word == "/new").unwrap();
        assert!(matches!(new.source, Source::Builtin));
        assert_eq!(table.iter().filter(|c| c.word == "/new").count(), 1);
        assert!(table.iter().any(|c| c.word == "/archify"));
        // Silently absent is how a user goes looking in the wrong place.
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("/new"), "{}", notes[0]);
    }

    #[test]
    fn a_skill_command_keeps_what_follows_it() {
        // `parse` has no table, so a skill and a typo are the same shape here
        // and only the caller can tell them apart.
        assert_eq!(
            parse("/commit fix the flaky test"),
            Some(Cmd::Other {
                word: "/commit".into(),
                args: "fix the flaky test".into()
            })
        );
    }

    #[test]
    fn a_typo_is_named_rather_than_sent_to_the_model() {
        // Otherwise `/tood` becomes a prompt and the model has to guess.
        assert_eq!(
            parse("/tood"),
            Some(Cmd::Other {
                word: "/tood".into(),
                args: String::new()
            })
        );
    }

    #[test]
    fn a_description_written_for_the_model_is_cut_to_a_line() {
        // Skill descriptions run to paragraphs; the list has one column.
        assert_eq!(gist("Write a commit."), "Write a commit");
        assert_eq!(
            gist("Draw diagrams. Also validates them, and much more besides."),
            "Draw diagrams"
        );
        let long = "a".repeat(200);
        assert_eq!(
            gist(&long).chars().count(),
            61,
            "60 columns and the ellipsis"
        );
        // Width, not characters: these cost two columns each.
        let wide = "从固定信源拉取上次运行之后的增量信息，生成每日精选摘要，并按重要性排序";
        assert!(gist(wide).ends_with('…'));
        assert_eq!(gist(wide).chars().count(), 31);
    }

    #[test]
    fn help_says_which_half_of_the_list_a_word_came_from() {
        // Without a prefix there is nothing in `/commit` itself to say it is
        // not built in.
        let plain = help(&table());
        assert!(plain.iter().all(|l| !l.is_empty()));
        let with = help(&commands(
            &[skill("commit", "Write a commit")],
            &mut Vec::new(),
        ));
        let split = with.iter().position(String::is_empty).expect("a break");
        assert!(with[split + 1].starts_with("skills"));
        assert!(with[split + 2].starts_with("/commit"));
    }

    #[test]
    fn a_skill_arrives_as_a_message_the_user_could_have_typed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: commit\ndescription: Write a commit.\n---\nStage, then write it.\n",
        )
        .unwrap();
        let mut s = skill("commit", "Write a commit.");
        s.dir = dir;

        let table = commands(std::slice::from_ref(&s), &mut Vec::new());
        let Step::Prompt(text) = dispatch(&table, "/commit", "the parser work") else {
            panic!("a skill runs a turn; it is not handled here");
        };
        assert!(text.starts_with("Run the `commit` skill."), "{text}");
        assert!(text.contains("Stage, then write it."));
        // The frontmatter is metadata, not instructions.
        assert!(!text.contains("description:"), "{text}");
        // Arguments below the body, so the skill reads as the standing order.
        let body = text.find("Stage, then").unwrap();
        assert!(text.find("the parser work").unwrap() > body, "{text}");
    }

    #[test]
    fn a_skill_whose_file_has_gone_says_so_rather_than_prompting() {
        // Discovery ran at startup; the directory can be gone by now, and an
        // empty prompt reaching the model is the worst of the answers.
        let ghost = skill("ghost", "x");
        let table = commands(std::slice::from_ref(&ghost), &mut Vec::new());
        let Step::Handled(said) = dispatch(&table, "/ghost", "") else {
            panic!("nothing should reach the model");
        };
        assert!(said[0].starts_with("cannot run ghost"), "{said:?}");
        // And a one-shot run refuses rather than sending the word itself.
        assert!(expand(&table, "/ghost").unwrap().is_err());
    }

    #[test]
    fn a_one_shot_prompt_expands_a_skill_and_leaves_everything_else_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\ndescription: x\n---\nStage it.\n",
        )
        .unwrap();
        let mut s = skill("commit", "x");
        s.dir = dir;
        let table = commands(std::slice::from_ref(&s), &mut Vec::new());

        let got = expand(&table, "/commit the parser work").expect("a skill");
        assert!(got.unwrap().contains("Stage it."));

        // A built-in is a session operation and there is no session here.
        assert!(expand(&table, "/help").is_none());
        // And a word that names nothing is prose, not a typo to refuse: the
        // argument is a whole prompt, and prompts start with slashes.
        assert!(expand(&table, "/comit the parser work").is_none());
        assert!(expand(&table, "/usr/bin is missing").is_none());
        assert!(expand(&table, "fix the flaky test").is_none());
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

    #[test]
    fn a_bang_runs_a_shell_command_and_a_bare_one_stays_prose() {
        assert_eq!(bash_command("!ls"), Some("ls"));
        assert_eq!(bash_command("! ls -la"), Some("ls -la"));
        assert_eq!(bash_command("!"), None);
        assert_eq!(bash_command("hello!"), None);
        // `!!cmd` is shell `! cmd` (negate the exit code), not pi's
        // excluded-from-context marker — that half of the feature is not here.
        assert_eq!(bash_command("!!git status"), Some("!git status"));
    }
}
