//! The words a line can start with: what each one takes, what it answers to,
//! and what it completes to.

use std::borrow::Cow;
use std::sync::Arc;

use skills::Skill;

use super::{Builtin, Intent};
use pi_store::listing::{Listing, Row};
use pi_store::session::ResumeChoice;

mod arguments;
pub use arguments::{Args, Input, Parameter, Sources, Subcommand, Values};

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
    /// Parameter behavior, shared by help, candidate lookup and hints.
    pub args: Args,
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
        args: Args,
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
    pub const fn surface(word: &'static str, args: Args, help: &'static str) -> Self {
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
        Args::None,
        "start a fresh session, keeping this one on disk",
        |_, _| Intent::Builtin(Builtin::New),
    ),
    Command::builtin(
        "/resume",
        Args::optional("id", Values::Sessions),
        "list this workspace's sessions, or switch to one",
        |_, rest| Intent::Builtin(Builtin::Resume(rest)),
    ),
    Command::builtin(
        "/rewind",
        Args::required("said", Values::Said),
        "go back to something you said",
        |_, rest| Intent::Builtin(Builtin::Rewind(rest)),
    ),
    Command::builtin(
        "/worktree",
        Args::Subcommands {
            commands: &[Subcommand {
                word: "rm",
                help: "remove one worktree, or all linked worktrees without a name",
                args: Args::optional("name", Values::LinkedWorktrees),
            }],
            fallback: Some(Parameter {
                name: "name",
                required: false,
                values: Values::Worktrees,
            }),
        },
        "list this repository's worktrees, or work in one — rm removes one or all",
        |_, rest| Intent::Builtin(Builtin::Worktree(rest)),
    ),
    Command::builtin(
        "/mv",
        Args::optional("dir", Values::Directory),
        "move this session to another directory",
        |_, rest| Intent::Builtin(Builtin::Mv(rest)),
    ),
    Command::builtin(
        "/name",
        Args::optional("text", Values::Text),
        "call this session something you will recognise",
        |_, rest| Intent::Builtin(Builtin::Name(rest)),
    ),
    Command::builtin(
        "/model",
        Args::optional("name", Values::Models),
        "list discovered and configured models, or move this session to one",
        |_, rest| Intent::Builtin(Builtin::Model(rest)),
    ),
    Command::builtin(
        "/compact",
        Args::optional("focus", Values::Text),
        "summarize everything but what you are working on now",
        |_, rest| Intent::Builtin(Builtin::Compact(rest)),
    ),
    Command::builtin(
        "/loop",
        Args::optional("text", Values::Text),
        "repeat a line while it keeps changing the tree; bare, stop one",
        |_, rest| Intent::Builtin(Builtin::Loop(rest)),
    ),
    Command::builtin(
        "/effort",
        Args::optional("level", Values::Fixed(pi_store::args::EffortArg::names)),
        "how hard the model thinks, from the next request on",
        |_, rest| Intent::Builtin(Builtin::Effort(rest)),
    ),
    Command::builtin(
        "/mcp",
        Args::Subcommands {
            commands: &[Subcommand {
                word: "restart",
                help: "reconnect one MCP server, or all",
                args: Args::optional("name", Values::Servers),
            }],
            fallback: None,
        },
        "the MCP servers and their tools; restart reconnects one, or all",
        |_, rest| Intent::Builtin(Builtin::Mcp(rest)),
    ),
    Command::builtin(
        "/status",
        Args::None,
        "what this session stands on, has spent, and where it writes",
        |_, _| Intent::Builtin(Builtin::Status),
    ),
    Command::builtin(
        "/content",
        Args::None,
        "everything the model is given before your first word, AGENTS.md included",
        |_, _| Intent::Builtin(Builtin::Content),
    ),
    Command::builtin("/help", Args::None, "this list", |_, _| {
        Intent::Builtin(Builtin::Help)
    }),
    Command::builtin(
        "/edit",
        Args::optional("file", Values::Editable),
        "list what the model is given from files — SYSTEM.md, AGENTS.md, memory — or edit one",
        |_, rest| Intent::Builtin(Builtin::Edit(rest)),
    ),
    Command::builtin(
        "/settings",
        Args::None,
        "edit the project's .pi/settings.toml, then reload",
        |_, rest| Intent::Builtin(Builtin::Settings(rest)),
    ),
    Command::builtin("/exit", Args::None, "leave", |_, _| {
        Intent::Builtin(Builtin::Quit)
    }),
];

// The first sentence, where the summary stops being one. How much of it
// fits is the surface's to say: it alone knows the width.
pub(crate) fn gist(description: &str) -> String {
    let end = (description.match_indices('.'))
        .find(|(at, _)| description[at + 1..].starts_with(char::is_whitespace))
        .map_or(description.len(), |(at, _)| at);
    pi_store::text::collapsed(description[..end].trim_end_matches('.'))
}

/// What a slash answers to: the built-ins and the surface's `words`, then one
/// command per skill.
///
/// No prefix: a skill is `/commit`, not `/skill:commit`. On a name clash a
/// built-in wins — the skill stays loadable by name, and the note says so.
pub fn commands(skills: &[Skill], words: &[Command], notes: &mut Vec<String>) -> Vec<Command> {
    let mut out = BUILTIN.to_vec();
    out.extend(words.iter().cloned());
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
            args: Args::optional("text", Values::Text),
            help: Cow::Owned(format!("[skill] {}", gist(&skill.description))),
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
            args: Args::optional("args", Values::Text),
            help: Cow::Owned(gist(&prompt.description)),
            intent: hand_on,
            source: Source::Prompt(prompt),
        });
    }
    Arc::new(out)
}

pub(crate) fn help(commands: &[Command], tools: &tool::Registry) -> Listing {
    let mut out = command_help(commands);
    let rows: Vec<Row> = slash_tools(commands, tools)
        .map(|t| Row::new([format!("/{}", t.name()), tool_help(t.as_ref())]))
        .collect();
    if !rows.is_empty() {
        out.rows.push(Row::new([""]));
        out.rows.extend(rows);
    }
    out
}

fn command_help(commands: &[Command]) -> Listing {
    let row = |c: &Command| {
        Row::new([
            format!("{} {}", c.word, c.args.usage())
                .trim_end()
                .to_string(),
            c.help.to_string(),
        ])
    };
    // Where built-ins end, not where skills begin — a third source would
    // otherwise land in the built-in half. `commands` keeps them contiguous.
    let Some(split) = commands
        .iter()
        .position(|c| !matches!(c.source, Source::Builtin))
    else {
        return Listing::of(commands.iter().map(row));
    };
    let mut out: Vec<Row> = commands[..split].iter().map(row).collect();
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
    pub accept: Acceptance,
    /// Append a space when placing this completion in the input.
    pub more: bool,
    /// Taken only when picked; the line submitted as it stands runs what was
    /// typed, which this would turn into something else.
    pub picked_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Run,
    /// Complete this part of the input, then accept more arguments or a value.
    Continue,
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
            accept: Acceptance::Run,
            more: false,
            picked_only: false,
        })
        .collect()
}

/// A named tool's arguments: only `key=value` for its keys, or why the
/// arguments do not read as a call.
pub(crate) fn call_of(
    name: &str,
    text: &str,
    tools: &tool::Registry,
) -> Result<serde_json::Value, String> {
    let words = words(text).ok_or("a quote is left open")?;
    let schema = tools
        .get(name)
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
        [] => Ok(serde_json::Value::Object(args)),
        _ => Err(format!("{name} needs {}", missing.join(", "))),
    }
}

// Existing commands keep their words; tools fill only the names still free.
fn slash_tools<'a>(
    commands: &'a [Command],
    tools: &tool::Registry,
) -> impl Iterator<Item = Arc<dyn tool::Tool>> + 'a {
    tools.listed().into_iter().filter(|t| {
        !commands
            .iter()
            .any(|c| c.word.strip_prefix('/') == Some(t.name()))
    })
}

fn tool_help(tool: &dyn tool::Tool) -> String {
    format!("[tool] {}", gist(tool.description()))
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

// After a tool's command word: the keys not given yet, while no `=` is typed.
fn tool_args(tool: &dyn tool::Tool, head: &str, typed: &str) -> Vec<Candidate> {
    if typed.contains(['=', '"']) {
        return Vec::new();
    }
    let schema = tool.schema();
    let given: Vec<&str> = (head.split_whitespace().skip(1))
        .filter_map(|w| w.split_once('=').map(|(k, _)| k))
        .collect();
    params(&schema)
        .filter(|p| p.name.starts_with(typed) && !given.contains(&p.name))
        .map(|p| Candidate {
            show: format!("{}=", p.name),
            line: format!("{head}{}=", p.name),
            accept: Acceptance::Continue,
            help: match p.required {
                true => format!("required · {}", p.help),
                false => format!("optional · {}", p.help),
            },
            more: false,
            picked_only: true,
        })
        .collect()
}

/// Each session's age, rounds and size, every column as wide as its widest
/// among `sessions`, so the rows shown together line up.
fn session_facts(sessions: &[&ResumeChoice]) -> Vec<String> {
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

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl tool::Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "Bridge this session. More details."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "description": "What to do"},
                    "text": {"type": "string"},
                    "enabled": {"type": "boolean"}
                },
                "required": ["action"]
            })
        }
        fn tier(&self) -> tool::Tier {
            tool::Tier::Read
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: &tool::Ctx,
        ) -> Result<tool::ToolOutput, tool::ToolError> {
            unreachable!("completion does not execute a tool")
        }
    }

    fn offered(line: &str, commands: &[Command], tools: &tool::Registry) -> Vec<Candidate> {
        Input::new(line, line.len(), commands, Some(tools)).complete(&Sources::default())
    }

    #[test]
    fn tools_share_slash_completion_without_taking_existing_words() {
        let skills = [Skill {
            name: "dotfiles".into(),
            description: "Sync configuration. More details.".into(),
            dir: "/nowhere".into(),
            builtin: None,
            builtin_files: Vec::new(),
        }];
        let table = commands(&skills, &[], &mut Vec::new());
        let tools = tool::Registry::new()
            .with(Named("wechat"))
            .with(Named("edit"))
            .with(Named("dotfiles"));
        let rows = offered("/", &table, &tools);
        let wechat = rows.iter().find(|c| c.line == "/wechat").unwrap();
        assert_eq!(wechat.show, "/wechat");
        assert_eq!(wechat.help, "[tool] Bridge this session");
        assert!(wechat.more && wechat.picked_only);
        for word in ["/edit", "/dotfiles"] {
            assert_eq!(rows.iter().filter(|c| c.line == word).count(), 1);
            assert!(
                !offered(&format!("{word} "), &table, &tools)
                    .iter()
                    .any(|c| c.show == "action=")
            );
        }
        assert_eq!(
            rows.iter().find(|c| c.line == "/dotfiles").unwrap().help,
            "[skill] Sync configuration"
        );
        assert_eq!(
            crate::input::read("/edit", &table, Some(&tools)),
            Intent::Builtin(Builtin::Edit(String::new()))
        );
        assert!(
            matches!(crate::input::read("/dotfiles", &table, Some(&tools)),
            Intent::Other { word, .. } if word == "/dotfiles")
        );
        let help = help(&table, &tools);
        assert!(
            help.rows
                .iter()
                .any(|r| r.cells == ["/wechat", "[tool] Bridge this session"])
        );
        assert!(
            !help
                .rows
                .iter()
                .any(|r| r.cells.first().is_some_and(|c| c.starts_with("/tool")))
        );
        for line in ["/tool", "/tool wechat", "/tool wechat action=bridge"] {
            assert!(offered(line, &table, &tools).is_empty());
            assert!(matches!(crate::input::read(line, &table, Some(&tools)),
                Intent::Other { word, .. } if word == "/tool"));
        }
    }

    #[test]
    fn direct_tool_arguments_complete_and_parse_as_key_value_pairs() {
        let tools = tool::Registry::new().with(Named("wechat"));
        assert_eq!(
            Input::new("/wechat ", 8, BUILTIN, Some(&tools))
                .hint()
                .as_deref(),
            Some("<action=> [enabled=] [text=]")
        );
        let rows = offered("/wechat ", BUILTIN, &tools);
        assert_eq!(rows[0].show, "action=");
        assert_eq!(rows[0].line, "/wechat action=");
        assert!(rows[0].help.starts_with("required"));
        assert!(rows[0].picked_only);
        let rows = offered("/wechat action=bridge te", BUILTIN, &tools);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].line, "/wechat action=bridge text=");
        assert!(rows[0].help.starts_with("optional"));
        assert!(
            !offered("/wechat action=bridge ", BUILTIN, &tools)
                .iter()
                .any(|c| c.show == "action=")
        );
        assert!(offered("/wechat text=", BUILTIN, &tools).is_empty());
        let args = r#"action=bridge text="hello phone" enabled=true"#;
        assert_eq!(
            crate::input::read(&format!("/wechat {args}"), BUILTIN, Some(&tools)),
            Intent::Tool {
                name: "wechat".into(),
                args: args.into()
            }
        );
        assert_eq!(
            call_of("wechat", args, &tools).unwrap(),
            serde_json::json!({"action": "bridge", "text": "hello phone", "enabled": true})
        );
        for args in [
            "",
            "action=bridge enabled=no",
            "action=bridge unknown=x",
            "action=\"bridge",
        ] {
            assert!(call_of("wechat", args, &tools).is_err(), "{args}");
        }
    }

    #[test]
    fn direct_tool_words_follow_the_live_registry() {
        struct Live(std::sync::Mutex<Vec<Arc<dyn tool::Tool>>>);
        impl tool::Source for Live {
            fn tools(&self) -> Vec<Arc<dyn tool::Tool>> {
                self.0.lock().unwrap().clone()
            }
        }
        let source = Arc::new(Live(Default::default()));
        let mut tools = tool::Registry::new();
        tools.read(source.clone());
        assert!(offered("/wech", BUILTIN, &tools).is_empty());
        source.0.lock().unwrap().push(Arc::new(Named("wechat")));
        assert_eq!(offered("/wech", BUILTIN, &tools)[0].line, "/wechat");
        assert!(matches!(
            crate::input::read("/wechat", BUILTIN, Some(&tools)),
            Intent::Tool { .. }
        ));
        source.0.lock().unwrap().clear();
        assert!(offered("/wechat", BUILTIN, &tools).is_empty());
        assert!(matches!(
            crate::input::read("/wechat", BUILTIN, Some(&tools)),
            Intent::Other { .. }
        ));
    }

    #[test]
    fn the_removed_job_command_is_not_parsed_or_completed() {
        assert!(!BUILTIN.iter().any(|command| command.word == "/job"));
        for line in ["/job", "/job stop 1", "/job restart 1", "/job run this"] {
            assert!(
                matches!(crate::input::read(line, BUILTIN, None), Intent::Other { word, .. } if word == "/job")
            );
            assert!(
                Input::new(line, line.len(), BUILTIN, None)
                    .complete(&Sources::default())
                    .is_empty()
            );
        }
        assert!(
            !Input::new("/j", 2, BUILTIN, None)
                .complete(&Sources::default())
                .iter()
                .any(|candidate| candidate.line.starts_with("/job"))
        );
    }

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

    // A second bang names the same command, so one reading must answer both.
    #[test]
    fn a_second_bang_runs_the_command_beside_the_turn() {
        use crate::input::Fate;
        let read = |line| crate::input::read(line, BUILTIN, None);
        assert!(matches!(read("!ls -l"), Intent::Bash(c) if c == "ls -l"));
        assert!(matches!(read("!!cargo build"), Intent::Background(c) if c == "cargo build"));
        assert!(matches!(read("!! cargo build"), Intent::Background(c) if c == "cargo build"));
        // Neither names a command, so both are prose.
        assert!(matches!(read("!"), Intent::Prompt(_)));
        assert!(matches!(read("!!"), Intent::Prompt(_)));
        // The run holds the transcript a `!` files into; a `!!` files nothing
        // until it ends.
        assert!(matches!(read("!ls").fate(), Fate::Queued));
        assert!(matches!(read("!!ls").fate(), Fate::Now));
    }
}
