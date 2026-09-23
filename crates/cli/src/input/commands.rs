//! The words a line can start with: what each one takes, what it answers to,
//! and what it completes to.

use std::borrow::Cow;

use skills::Skill;

use super::{Builtin, Intent};
use crate::store::session::ResumeChoice;

#[derive(Clone)]
pub enum Source {
    // A word `parse` knows and `command` answers itself.
    Builtin,
    // A `SKILL.md` to read and hand to the model as if the user had typed it.
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
    /// What the word means to the door. A skill's row carries [`hand_on`], the
    /// same thing a word the table does not have gets.
    pub intent: fn(&str, String) -> Intent,
    pub source: Source,
}

impl Command {
    const fn builtin(
        word: &'static str,
        args: &'static str,
        help: &'static str,
        intent: fn(&str, String) -> Intent,
    ) -> Self {
        Self {
            word: Cow::Borrowed(word),
            args,
            help: Cow::Borrowed(help),
            intent,
            source: Source::Builtin,
        }
    }
}

/// A word the table lists and nothing more: a skill, whose line `step_for`
/// expands once the door has handed it on. A word the table does not have at
/// all lands in the same place.
fn hand_on(word: &str, args: String) -> Intent {
    Intent::Other {
        word: word.to_string(),
        args,
    }
}

// Every built-in command, once: the word, what it takes, what it does, and what
// it means. Help, completion and reading a line all come from here, so a word
// that reached only one of them is not a bug this table can have.
//
// Nothing reads this directly except `commands`, which appends the skills to
// it. What a run answers to is settled when the workspace is known, not when
// the binary is built.
pub(crate) const BUILTIN: &[Command] = &[
    Command::builtin(
        "/new",
        "",
        "start a fresh session, keeping this one on disk",
        |_, _| Intent::Builtin(Builtin::New),
    ),
    Command::builtin(
        "/resume",
        "[id]",
        "list this workspace's sessions, or switch to one",
        |_, rest| Intent::Builtin(Builtin::Resume(rest)),
    ),
    Command::builtin(
        "/worktree",
        "[name]",
        "list this repository's worktrees, or work in one — rm removes one",
        |_, rest| Intent::Builtin(Builtin::Worktree(rest)),
    ),
    Command::builtin(
        "/name",
        "[text]",
        "call this session something you will recognise",
        |_, rest| Intent::Builtin(Builtin::Name(rest)),
    ),
    Command::builtin(
        "/model",
        "[name]",
        "list the models in ~/.pi/settings.toml, or move this session to one",
        |_, rest| Intent::Builtin(Builtin::Model(rest)),
    ),
    Command::builtin(
        "/compact",
        "[focus]",
        "summarize everything but what you are working on now",
        |_, rest| Intent::Builtin(Builtin::Compact(rest)),
    ),
    Command::builtin(
        "/loop",
        "[text]",
        "repeat a line while it keeps changing the tree; bare, stop one",
        |_, rest| Intent::Builtin(Builtin::Loop(rest)),
    ),
    Command::builtin(
        "/reload",
        "",
        "re-read ~/.pi/settings.toml, the instructions and the skills",
        |_, _| Intent::Builtin(Builtin::Reload),
    ),
    Command::builtin(
        "/status",
        "",
        "what this session stands on, has spent, and where it writes",
        |_, _| Intent::Builtin(Builtin::Status),
    ),
    Command::builtin(
        "/keys",
        "",
        "what every key does, and the id to rebind it under",
        |_, _| Intent::Builtin(Builtin::Keys),
    ),
    Command::builtin("/help", "", "this list", |_, _| {
        Intent::Builtin(Builtin::Help)
    }),
    Command::builtin("/settings", "", "open the settings panel", |_, rest| {
        Intent::Builtin(Builtin::Settings(rest))
    }),
    Command::builtin(
        "/wechat",
        "[on|off]",
        "bridge this session to WeChat (scan a QR on first connect)",
        |_, rest| Intent::Builtin(Builtin::Wechat(rest)),
    ),
    Command::builtin("/exit", "", "leave (Ctrl-D does the same)", |_, _| {
        Intent::Builtin(Builtin::Quit)
    }),
];

// How wide a one-line description may be before it is cut.
const GIST: usize = 60;

// A description written for the model, cut down to a line for a list.
//
// Two cuts, because they answer different questions: the first sentence is
// where the description stops being a summary, and the column is where the
// terminal stops having room.
fn gist(description: &str) -> String {
    let first = description
        .split_once(". ")
        .map_or(description, |(head, _)| head);
    crate::store::text::clip(first.trim().trim_end_matches('.'), GIST)
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
            intent: hand_on,
            source: Source::Skill(skill.clone()),
        });
    }
    out
}

pub(crate) fn help(commands: &[Command]) -> Vec<String> {
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

/// Something the prompt can complete to — a model, a worktree — and what tells
/// it apart from the others.
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

// A worktree name completed against `prefix`; the whole line an accept makes
// is `head` plus the name, which is what tells entering from removing.
fn worktree_candidates(trees: &[Choice], head: &str, prefix: &str) -> Vec<Candidate> {
    trees
        .iter()
        .filter(|w| w.name.starts_with(prefix) && w.name != prefix)
        .map(|w| Candidate {
            show: w.name.clone(),
            line: format!("{head}{}", w.name),
            help: w.note.clone(),
            more: false,
        })
        .collect()
}

/// What the line could still become: a command while its word is being typed,
/// then that command's own argument once the word is settled.
///
/// The commands with arguments worth completing are the ones whose argument is
/// a name out of a known set: `/model` against the config's models, `/resume`
/// against the workspace's saved sessions, `/worktree` against the
/// repository's checkouts. A prompt is prose and a focus phrase is prose;
/// guessing at either is worse than leaving it alone.
///
/// The last two are asked for through a call rather than handed over, because
/// every line but theirs is completed without them: reading the workspace's
/// sessions is a walk of every transcript in it and reading the checkouts is a
/// `git worktree list`, and a frame that offers neither should pay neither.
pub fn complete<'a>(
    line: &str,
    commands: &[Command],
    models: &[Choice],
    sessions: impl Fn() -> &'a [ResumeChoice],
    worktrees: impl Fn() -> &'a [Choice],
) -> Vec<Candidate> {
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
    let typed = rest.trim_start();
    match word {
        // A second word means the model name is settled and something else is
        // being typed. There is no third thing to offer.
        "/model" if typed.contains(char::is_whitespace) => Vec::new(),
        "/model" => models
            .iter()
            .filter(|m| m.name.starts_with(typed) && m.name != typed)
            .map(|m| Candidate {
                show: m.name.clone(),
                line: format!("/model {}", m.name),
                help: m.note.clone(),
                more: false,
            })
            .collect(),
        // `rm` marks what follows it for removal; a bare `rm` still means the
        // name of a worktree, so nothing is offered until the space says.
        "/worktree" if typed == "rm" => Vec::new(),
        "/worktree" if typed.starts_with("rm ") => {
            let arg = typed["rm ".len()..].trim_start();
            worktree_candidates(worktrees(), "/worktree rm ", arg)
        }
        // A name may hold a slash (`feat/one`), so unlike a model it is not
        // settled by the first word — only whitespace after it settles it.
        "/worktree" if typed.contains(char::is_whitespace) => Vec::new(),
        "/worktree" => worktree_candidates(worktrees(), "/worktree ", typed),
        // A first question is a whole sentence, and a session answers to the
        // name it was given as well, and to its id.
        "/resume" => sessions()
            .iter()
            .filter(|s| {
                let named = s.name.as_deref().is_some_and(|n| n.starts_with(typed));
                (s.name.is_some() || !s.prompt.is_empty())
                    && (named || s.prompt.starts_with(typed) || s.id.starts_with(typed))
            })
            .map(|s| Candidate {
                show: s.label(),
                line: format!("/resume {}", s.id),
                help: ago(s.created),
                more: false,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn ago(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(secs);
    if ago < 60 {
        "just now".into()
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else {
        format!("{}d ago", ago / 86400)
    }
}
