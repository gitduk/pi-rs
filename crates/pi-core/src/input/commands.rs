//! The words a line can start with: what each one takes, what it answers to,
//! and what it completes to.

use std::borrow::Cow;
use std::sync::Arc;

use skills::Skill;

use super::{Builtin, Intent};
use pi_store::listing::{Listing, Row};
use pi_store::session::ResumeChoice;

#[derive(Clone)]
pub enum Source {
    // A word `read` knows and `command` answers itself.
    Builtin,
    // A `SKILL.md` to read and hand to the model as if the user had typed it.
    Skill(Skill),
    // An MCP server's prompt, fetched from it when run and sent as typed.
    Prompt(mcp::Prompt),
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

/// A word the table lists and nothing more — a skill `step_for` expands
/// later. A word the table lacks entirely lands in the same place.
fn hand_on(word: &str, args: String) -> Intent {
    Intent::Other {
        word: word.to_string(),
        args,
    }
}

// Every built-in, once: help, completion and reading a line all come from
// here, so a word landing in only one of them is a bug this table prevents.
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
        "/jobs",
        "[stop id]",
        "what runs in the background here; stop ends one",
        |_, rest| Intent::Builtin(Builtin::Jobs(rest)),
    ),
    Command::builtin(
        "/loop",
        "[text]",
        "repeat a line while it keeps changing the tree; bare, stop one",
        |_, rest| Intent::Builtin(Builtin::Loop(rest)),
    ),
    Command::builtin(
        "/effort",
        "[off|low|medium|high]",
        "how hard the model thinks, from the next request on",
        |_, rest| Intent::Builtin(Builtin::Effort(rest)),
    ),
    Command::builtin(
        "/mcp",
        "[restart [name]]",
        "the MCP servers and their tools; restart reconnects one, or all",
        |_, rest| Intent::Builtin(Builtin::Mcp(rest)),
    ),
    Command::builtin(
        "/status",
        "",
        "what this session stands on, has spent, and where it writes",
        |_, _| Intent::Builtin(Builtin::Status),
    ),
    Command::builtin(
        "/content",
        "",
        "everything the model is given before your first word, AGENTS.md included",
        |_, _| Intent::Builtin(Builtin::Content),
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
    Command::builtin(
        "/edit",
        "[file]",
        "list what the model is given from files — SYSTEM.md, AGENTS.md, memory — or edit one",
        |_, rest| Intent::Builtin(Builtin::Edit(rest)),
    ),
    Command::builtin(
        "/settings",
        "",
        "edit the project's .pi.toml, then reload",
        |_, rest| Intent::Builtin(Builtin::Settings(rest)),
    ),
    Command::builtin("/exit", "", "leave (Ctrl-D does the same)", |_, _| {
        Intent::Builtin(Builtin::Quit)
    }),
];

// How wide a one-line description may be before it is cut.
const GIST: usize = 60;

// Two cuts: the first sentence is where the summary stops being one, the
// column is where the terminal stops having room.
fn gist(description: &str) -> String {
    let first = description
        .split_once(". ")
        .map_or(description, |(head, _)| head);
    pi_store::text::clip(first.trim().trim_end_matches('.'), GIST)
}

/// What a slash answers to: the built-ins, then one command per skill.
///
/// No prefix: a skill is `/commit`, not `/skill:commit`. On a name clash a
/// built-in wins — the skill stays loadable by name, and the note says so.
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

/// `table` with the running servers' prompts after it. A prompt whose word
/// another command holds gives way, as a skill does to a built-in.
pub fn with_prompts(table: Arc<Vec<Command>>, prompts: Vec<mcp::Prompt>) -> Arc<Vec<Command>> {
    if prompts.is_empty() {
        return table;
    }
    let mut out = (*table).clone();
    for prompt in prompts {
        let word = prompt.word();
        if out.iter().any(|c| c.word.as_ref() == word) {
            continue;
        }
        out.push(Command {
            word: Cow::Owned(word),
            args: "[args]",
            help: Cow::Owned(gist(&prompt.description)),
            intent: hand_on,
            source: Source::Prompt(prompt),
        });
    }
    Arc::new(out)
}

/// The command that turns `channel` on and off: `/wechat` for `wechat`.
pub fn channel_command(channel: &dyn ::channel::Channel) -> Command {
    Command {
        word: Cow::Owned(format!("/{}", channel.name())),
        args: "[on|off]",
        help: Cow::Borrowed(channel.help()),
        intent: |word, rest| {
            Intent::Builtin(Builtin::Channel(
                word.trim_start_matches('/').to_string(),
                rest,
            ))
        },
        source: Source::Builtin,
    }
}

/// `table` with `channels` among its built-ins, where `help` lists them. A
/// channel's word is a built-in's, so a skill of the same name gives way.
pub fn with_channels(table: &Arc<Vec<Command>>, channels: &[Command]) -> Arc<Vec<Command>> {
    if channels.is_empty() {
        return table.clone();
    }
    let mut out: Vec<Command> = table
        .iter()
        .filter(|c| channels.iter().all(|ch| ch.word != c.word))
        .cloned()
        .collect();
    let at = out
        .iter()
        .position(|c| !matches!(c.source, Source::Builtin))
        .unwrap_or(out.len());
    out.splice(at..at, channels.iter().cloned());
    Arc::new(out)
}

pub(crate) fn help(commands: &[Command]) -> Listing {
    let row = |c: &Command| Row::new([format!("{} {}", c.word, c.args), c.help.to_string()]);
    // Where built-ins end, not where skills begin — a third source would
    // otherwise land in the built-in half. `commands` keeps them contiguous.
    let Some(split) = commands
        .iter()
        .position(|c| !matches!(c.source, Source::Builtin))
    else {
        return Listing::of(commands.iter().map(row));
    };
    let mut out: Vec<Row> = commands[..split].iter().map(row).collect();
    // Without a prefix there is nothing in the word itself to say which group
    // it came from, so the list says it once per group.
    let groups = [
        (
            "skills — the instructions load when you run one:",
            (|s: &Source| matches!(s, Source::Skill(_))) as fn(&Source) -> bool,
        ),
        (
            "MCP prompts — asked of their server when you run one:",
            |s: &Source| matches!(s, Source::Prompt(_)),
        ),
    ];
    for (head, member) in groups {
        let rows: Vec<Row> = commands[split..]
            .iter()
            .filter(|c| member(&c.source))
            .map(row)
            .collect();
        if !rows.is_empty() {
            out.push(Row::new([""]));
            out.push(Row::new([head]));
            out.extend(rows);
        }
    }
    Listing::of(out)
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
// The names among `choices` that `typed` begins and does not already finish,
// each as the whole line `head` plus the name.
fn offer(choices: &[Choice], head: &str, typed: &str) -> Vec<Candidate> {
    choices
        .iter()
        .filter(|c| c.name.starts_with(typed) && c.name != typed)
        .map(|c| Candidate {
            show: c.name.clone(),
            line: format!("{head}{}", c.name),
            help: c.note.clone(),
            more: false,
        })
        .collect()
}

/// What the line could still become: a command while its word is typed,
/// then that command's own argument once the word is settled.
///
/// Only args with a known name set are completed (`/model`, `/resume`,
/// `/worktree`, `/edit`); a prompt or focus phrase is prose, not guessed.
///
/// Sessions, worktrees and files are asked for lazily: walking transcripts,
/// running `git worktree list` or reading directories is paid only by the
/// line that needs it.
pub fn complete<'a>(
    line: &str,
    commands: &[Command],
    models: &[Choice],
    sessions: impl Fn() -> &'a [ResumeChoice],
    worktrees: impl Fn() -> &'a [Choice],
    editable: impl Fn() -> &'a [Choice],
) -> Vec<Candidate> {
    if !line.starts_with('/') {
        return Vec::new();
    }
    let Some((word, rest)) = line.split_once(char::is_whitespace) else {
        // The exact word stays in the list: dropping it would leave only
        // `/news` when `/new` is typed in full, and Tab would pick wrong.
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
        "/effort" => {
            let levels: Vec<Choice> = pi_store::args::EffortArg::names()
                .into_iter()
                .map(|e| Choice {
                    name: e.to_string(),
                    note: String::new(),
                })
                .collect();
            offer(&levels, "/effort ", typed)
        }
        // A second word means the model name is settled and something else is
        // being typed. There is no third thing to offer.
        "/model" if typed.contains(char::is_whitespace) => Vec::new(),
        "/model" => offer(models, "/model ", typed),
        "/edit" => offer(editable(), "/edit ", typed),
        // `rm` marks what follows it for removal; a bare `rm` still means the
        // name of a worktree, so nothing is offered until the space says.
        "/worktree" if typed == "rm" => Vec::new(),
        "/worktree" if typed.starts_with("rm ") => {
            let arg = typed["rm ".len()..].trim_start();
            offer(worktrees(), "/worktree rm ", arg)
        }
        // A name may hold a slash (`feat/one`), so unlike a model it is not
        // settled by the first word — only whitespace after it settles it.
        "/worktree" if typed.contains(char::is_whitespace) => Vec::new(),
        "/worktree" => offer(worktrees(), "/worktree ", typed),
        // A first question is a whole sentence, and a session answers to the
        // name it was given as well, and to its id.
        "/resume" => {
            let offered: Vec<&ResumeChoice> = sessions()
                .iter()
                .filter(|s| {
                    let named = s.name.as_deref().is_some_and(|n| n.starts_with(typed));
                    (s.name.is_some() || !s.prompt.is_empty())
                        && (named || s.prompt.starts_with(typed) || s.id.starts_with(typed))
                })
                .collect();
            let facts = session_facts(&offered);
            offered
                .into_iter()
                .zip(facts)
                .map(|(s, help)| Candidate {
                    show: s.label(),
                    line: format!("/resume {}", s.id),
                    help,
                    more: false,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Each session's age, rounds and size, every column as wide as its widest
/// among `sessions`, so the rows shown together line up.
pub(crate) fn session_facts(sessions: &[&ResumeChoice]) -> Vec<String> {
    let rows: Vec<[String; 3]> = sessions
        .iter()
        .map(|s| {
            [
                ago(s.touched),
                format!("{} rounds", s.rounds),
                pi_store::text::size(s.bytes),
            ]
        })
        .collect();
    let w: [usize; 3] = std::array::from_fn(|i| rows.iter().map(|r| r[i].len()).max().unwrap_or(0));
    rows.iter()
        .map(|[age, rounds, size]| {
            format!(
                "{age:>a$}  {rounds:>b$}  {size:>c$}",
                a = w[0],
                b = w[1],
                c = w[2]
            )
        })
        .collect()
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
