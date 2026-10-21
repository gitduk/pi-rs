//! The words a line can start with: what each one takes, what it answers to,
//! and what it completes to.

use std::borrow::Cow;
use std::sync::Arc;

use skills::Skill;

use super::{Builtin, Intent};
use crate::driver::jobs::Job;
use pi_store::listing::{Listing, Row};
use pi_store::session::ResumeChoice;

#[derive(Clone)]
pub enum Source {
    // A word `read` knows: answered here, or by the surface that added it.
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

impl Command {
    /// A word the surface answers itself, listed with the built-ins.
    pub const fn surface(word: &'static str, args: &'static str, help: &'static str) -> Self {
        Self::builtin(word, args, help, |word, args| Intent::Surface {
            word: word.to_string(),
            args,
        })
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
        "/rewind",
        "<said>",
        "go back to something you said",
        |_, rest| Intent::Builtin(Builtin::Rewind(rest)),
    ),
    Command::builtin(
        "/worktree",
        "[name]",
        "list this repository's worktrees, or work in one — rm removes one",
        |_, rest| Intent::Builtin(Builtin::Worktree(rest)),
    ),
    Command::builtin(
        "/cd",
        "[dir]",
        "carry this conversation on in another directory",
        |_, rest| Intent::Builtin(Builtin::Cd(rest)),
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
        "/job",
        "[stop [id…] | text]",
        "what runs in the background here; stop ends them, bare every running one; \
         anything else has the model start it there",
        |_, rest| job(rest),
    ),
    Command::builtin(
        "/tool",
        "[name [key=value…]]",
        "run one of the model's tools yourself, beside any turn; bare, list them",
        |_, rest| Intent::Builtin(Builtin::Tool(rest)),
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

// The first sentence, where the summary stops being one. How much of it
// fits is the surface's to say: it alone knows the width.
pub(crate) fn gist(description: &str) -> String {
    let end = (description.match_indices('.'))
        .find(|(at, _)| description[at + 1..].starts_with(char::is_whitespace))
        .map_or(description.len(), |(at, _)| at);
    description[..end]
        .trim_end_matches('.')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

/// `table` with `words` after its built-ins, in place of any row of theirs.
pub fn with_surface(table: Arc<Vec<Command>>, words: &[Command]) -> Arc<Vec<Command>> {
    if words.is_empty() {
        return table;
    }
    let mut out: Vec<Command> = (table.iter())
        .filter(|c| !words.iter().any(|w| w.word == c.word))
        .cloned()
        .collect();
    let at = (out.iter())
        .position(|c| !matches!(c.source, Source::Builtin))
        .unwrap_or(out.len());
    out.splice(at..at, words.iter().cloned());
    Arc::new(out)
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
    /// Taken only when picked; the line submitted as it stands runs what was
    /// typed, which this would turn into something else.
    pub picked_only: bool,
}

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
            picked_only: false,
        })
        .collect()
}

// `/job` manages jobs when it is bare or a verb followed only by ids;
// any other text is something to start in the background.
fn job(rest: String) -> Intent {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let id = |w: &&str| w.trim_start_matches('#').parse::<u64>().is_ok();
    match words.as_slice() {
        [] => Intent::Builtin(Builtin::Job(rest)),
        ["stop", ids @ ..] if ids.iter().all(id) => Intent::Builtin(Builtin::Job(rest)),
        ["restart", ids @ ..] if ids.iter().all(id) => Intent::Builtin(Builtin::Job(rest)),
        _ => Intent::Builtin(Builtin::Background(rest)),
    }
}

/// `/tool` text read as a call: a tool's name, then only `key=value` for its
/// keys. The tool and its arguments, or why the line does not read as one.
pub(crate) fn call_of(
    text: &str,
    tools: &tool::Registry,
) -> Result<(String, serde_json::Value), String> {
    let mut words = words(text).ok_or("a quote is left open")?.into_iter();
    let name = words.next().unwrap_or_default();
    let schema = tools
        .get(&name)
        .ok_or_else(|| format!("no tool `{name}` here"))?
        .schema();
    let mut args = serde_json::Map::new();
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            return Err(format!("`{word}` is not key=value"));
        };
        let Some(about) = schema["properties"].get(key) else {
            let keys: Vec<&str> = params(&schema).map(|p| p.name).collect();
            return Err(format!(
                "{name} has no `{key}`; it takes {}",
                keys.join(", ")
            ));
        };
        let value = typed(about["type"].as_str(), value).map_err(|why| format!("{key}: {why}"))?;
        args.insert(key.to_string(), value);
    }
    let missing: Vec<&str> = params(&schema)
        .filter(|p| p.required && !args.contains_key(p.name))
        .map(|p| p.name)
        .collect();
    match missing.as_slice() {
        [] => Ok((name, serde_json::Value::Object(args))),
        _ => Err(format!("{name} needs {}", missing.join(", "))),
    }
}

/// What `/tool` bare answers: each tool by name, and the gist of it.
pub(crate) fn tool_listing(tools: &tool::Registry) -> Listing {
    Listing::of(
        tools
            .listed()
            .into_iter()
            .map(|t| Row::new([t.name().to_string(), gist(t.description())])),
    )
    .brief()
}

// Split at whitespace, a `"…"` run kept whole, `\"` a quote inside one;
// `None` for a quote left open.
fn words(text: &str) -> Option<Vec<String>> {
    let (mut out, mut word, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => (quoted, started) = (!quoted, true),
            '\\' if quoted => word.extend(chars.next()),
            c if c.is_whitespace() && !quoted => {
                if std::mem::take(&mut started) {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        out.push(word);
    }
    Some(out)
}

// A value as the schema's type for it, which is a string when none is given.
fn typed(kind: Option<&str>, value: &str) -> Result<serde_json::Value, String> {
    use serde_json::Value;
    let bad = || format!("`{value}` is not {}", kind.unwrap_or("string"));
    match kind {
        Some("integer") => value.parse::<i64>().map(Value::from).map_err(|_| bad()),
        Some("number") => value
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(bad),
        Some("boolean") => value.parse::<bool>().map(Value::Bool).map_err(|_| bad()),
        Some("array") => match serde_json::from_str(value) {
            Ok(Value::Array(items)) => Ok(Value::Array(items)),
            _ => Err(bad()),
        },
        Some("object") => match serde_json::from_str(value) {
            Ok(Value::Object(map)) => Ok(Value::Object(map)),
            _ => Err(bad()),
        },
        _ => Ok(Value::String(value.to_string())),
    }
}

struct Param<'a> {
    name: &'a str,
    help: &'a str,
    required: bool,
}

// A schema's arguments, the required ones first.
fn params(schema: &serde_json::Value) -> impl Iterator<Item = Param<'_>> {
    let required = |name: &str| {
        schema["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|n| n == name))
    };
    let mut out: Vec<Param<'_>> = (schema["properties"].as_object().into_iter().flatten())
        .map(|(name, about)| Param {
            name,
            help: about["description"].as_str().unwrap_or_default(),
            required: required(name),
        })
        .collect();
    out.sort_by_key(|p| !p.required);
    out.into_iter()
}

// After `/tool <name> `: the keys not given yet, while no `=` is typed.
fn tool_args(tool: &dyn tool::Tool, head: &str, typed: &str) -> Vec<Candidate> {
    let schema = tool.schema();
    let given: Vec<&str> = (head.split_whitespace().skip(1))
        .filter_map(|w| w.split_once('=').map(|(k, _)| k))
        .collect();
    if typed.contains(['=', '"']) {
        return Vec::new();
    }
    params(&schema)
        .filter(|p| p.name.starts_with(typed) && !given.contains(&p.name))
        .map(|p| Candidate {
            show: format!("{}=", p.name),
            line: format!("/tool {head}{}=", p.name),
            help: match p.required {
                true => format!("required · {}", p.help),
                false => format!("optional · {}", p.help),
            },
            more: false,
            picked_only: true,
        })
        .collect()
}

// The ids after `/job <verb>` not named yet, completing the one being typed.
// Once one is named, a bare space offers nothing: submitting runs the line
// as typed, not an offered id tacked on.
fn job_ids(jobs: &[&Job], verb: &str, args: &str) -> Vec<Candidate> {
    let mut named: Vec<&str> = args.split_whitespace().collect();
    let typing = if args.is_empty() || args.ends_with(char::is_whitespace) {
        ""
    } else {
        named.pop().unwrap_or("")
    };
    let typing = typing.trim_start_matches('#');
    let settled = |id: &str| named.iter().any(|n| n.trim_start_matches('#') == id);
    let ids: Vec<(String, &str)> = (jobs.iter())
        .map(|j| (j.id.to_string(), j.description.as_str()))
        .collect();
    if (typing.is_empty() && !named.is_empty()) || ids.iter().any(|(id, _)| id == typing) {
        return Vec::new();
    }
    let head: String = named.iter().map(|n| format!("{n} ")).collect();
    ids.iter()
        .filter(|(id, _)| id.starts_with(typing) && !settled(id))
        .map(|(id, what)| Candidate {
            show: format!("#{id}"),
            line: format!("/job {verb} {head}{id}"),
            help: what.to_string(),
            more: false,
            picked_only: false,
        })
        .collect()
}

/// What the front lane gives completion: its jobs, kept ones too, for `/job`,
/// its tools, for `/tool`, and what was said, each named by its entry id,
/// for `/rewind`.
#[derive(Clone, Copy, Default)]
pub struct Front<'a> {
    pub jobs: &'a [Job],
    pub tools: Option<&'a tool::Registry>,
    pub said: &'a [Choice],
}

/// What the line could still become: a command while its word is typed,
/// then that command's own argument once the word is settled.
///
/// Only args with a known name set are completed (`/model`, `/resume`, `/rewind`,
/// `/worktree`, `/edit`, `/job`, `/tool`); a prompt or focus phrase is prose, not guessed.
///
/// Sessions, worktrees and files are asked for lazily: walking transcripts,
/// running `git worktree list` or reading directories is paid only by the
/// line that needs it.
pub fn complete<'a>(
    line: &str,
    commands: &[Command],
    models: &[Choice],
    front: Front<'_>,
    sessions: impl Fn() -> &'a [ResumeChoice],
    worktrees: impl Fn() -> &'a [Choice],
    editable: impl Fn() -> &'a [Choice],
) -> Vec<Candidate> {
    let Front { jobs, tools, said } = front;
    if !line.starts_with('/') {
        return Vec::new();
    }
    let Some((word, rest)) = line.split_once(char::is_whitespace) else {
        // The exact word stays in the list: dropping it would leave only
        // `/news` when `/new` is typed in full, and accepting would pick wrong.
        return commands
            .iter()
            .filter(|c| c.word.starts_with(line))
            .map(|c| Candidate {
                show: format!("{} {}", c.word, c.args).trim_end().to_string(),
                line: c.word.to_string(),
                help: c.help.to_string(),
                more: !c.args.is_empty(),
                picked_only: false,
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
        "/job" if typed.starts_with("stop ") => {
            let running: Vec<&Job> = jobs.iter().filter(|j| j.state.stoppable()).collect();
            job_ids(&running, "stop", typed["stop ".len()..].trim_start())
        }
        "/job" if typed.starts_with("restart ") => {
            let every: Vec<&Job> = jobs.iter().collect();
            job_ids(&every, "restart", typed["restart ".len()..].trim_start())
        }
        // Past its first word `/job` is prose for the model; nothing to offer.
        "/job" if typed.contains(char::is_whitespace) => Vec::new(),
        // Submitting a bare `/job ` still lists them, not stops them all.
        "/job" => {
            let verbs = [
                ("stop", "end them by id, or every running one"),
                (
                    "restart",
                    "start them over by id, stopping any still running",
                ),
            ]
            .into_iter()
            .filter(|(verb, _)| verb.starts_with(typed) && *verb != typed)
            .map(|(verb, help)| Candidate {
                show: verb.into(),
                line: format!("/job {verb}"),
                help: help.into(),
                more: true,
                // A prefix of a verb may be the first word of something to start.
                picked_only: true,
            });
            verbs.collect()
        }
        "/tool" if typed.contains(char::is_whitespace) => {
            let (head, last) = typed.rsplit_once(char::is_whitespace).unwrap_or_default();
            let named = head.split_whitespace().next().unwrap_or_default();
            match tools.and_then(|t| t.get(named)) {
                Some(tool) => tool_args(tool.as_ref(), &typed[..typed.len() - last.len()], last),
                None => Vec::new(),
            }
        }
        "/tool" => {
            let listed = tools.map(tool::Registry::listed).unwrap_or_default();
            listed
                .into_iter()
                .filter(|t| t.name().starts_with(typed) && t.name() != typed)
                .map(|t| Candidate {
                    show: t.name().to_string(),
                    line: format!("/tool {}", t.name()),
                    help: gist(t.description()),
                    more: params(&t.schema()).next().is_some(),
                    // Enter on a prefix must not run a tool nobody chose.
                    picked_only: true,
                })
                .collect()
        }
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
        // A message is remembered by any word in it; an accepted row's id
        // names that row alone. Newest first, so nearest the line.
        "/rewind" => {
            let named = said.iter().any(|s| s.name == typed);
            (said.iter().rev())
                .filter(|s| match named {
                    true => s.name == typed,
                    false => pi_store::text::matches(&s.note, typed),
                })
                .map(|s| Candidate {
                    show: s.note.clone(),
                    line: format!("/rewind {}", s.name),
                    help: String::new(),
                    more: false,
                    picked_only: false,
                })
                .collect()
        }
        // A session answers to a word of its first question, its name or id.
        "/resume" => {
            let offered: Vec<&ResumeChoice> = sessions()
                .iter()
                .filter(|s| {
                    let holds = |text: &str| pi_store::text::matches(text, typed);
                    (s.name.is_some() || !s.prompt.is_empty())
                        && (s.name.as_deref().is_some_and(holds)
                            || holds(&s.prompt)
                            || holds(&s.id))
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
                    picked_only: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    // The first sentence, whatever breaks it, and never its own period.
    #[test]
    fn a_gist_is_the_first_sentence_without_its_period() {
        assert_eq!(gist("Reads a file."), "Reads a file");
        assert_eq!(gist("Reads a file. And more."), "Reads a file");
        assert_eq!(
            gist("Gets its conclusion.\n  The rest"),
            "Gets its conclusion"
        );
        assert_eq!(gist("Version 2.4 is out"), "Version 2.4 is out");
    }

    // A verb with only ids after it manages jobs; any other text, even one
    // starting with a verb, is something to start.
    #[test]
    fn job_manages_only_on_a_verb_and_ids() {
        for line in ["", "stop", "stop 2 #3", "restart", "restart #1"] {
            assert!(
                matches!(job(line.into()), Intent::Builtin(Builtin::Job(_))),
                "{line}"
            );
        }
        for line in [
            "python3 -m http.server 8080",
            "stop the server",
            "restart the server",
            "output 4",
        ] {
            assert!(
                matches!(job(line.into()), Intent::Builtin(Builtin::Background(_))),
                "{line}"
            );
        }
    }
}
