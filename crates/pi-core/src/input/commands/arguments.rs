//! Parameter declarations and the argument currently being edited. Help,
//! candidates and placeholders all read the same declaration.

use super::{
    Acceptance, Candidate, Choice, Command, offer, params, session_facts, slash_tools, tool_args,
    tool_help,
};
use pi_store::session::ResumeChoice;
use std::sync::Arc;

/// Where a parameter's values come from. Text deliberately has no candidates.
#[derive(Clone, Copy, Debug)]
pub enum Values {
    Text,
    Directory,
    Fixed(fn() -> Vec<String>),
    Models,
    Sessions,
    Worktrees,
    LinkedWorktrees,
    Editable,
    Said,
    Servers,
}

#[derive(Clone, Copy, Debug)]
pub struct Parameter {
    pub name: &'static str,
    pub required: bool,
    pub values: Values,
}

impl Parameter {
    fn label(self) -> String {
        match self.values {
            Values::Fixed(names) => names().join("|"),
            Values::Text
            | Values::Directory
            | Values::Models
            | Values::Sessions
            | Values::Worktrees
            | Values::LinkedWorktrees
            | Values::Editable
            | Values::Said
            | Values::Servers => self.name.into(),
        }
    }

    fn usage(self) -> String {
        let name = self.label();
        if self.required {
            format!("<{name}>")
        } else {
            format!("[{name}]")
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Subcommand {
    pub word: &'static str,
    pub help: &'static str,
    pub args: Args,
}

/// Every command declares its parameter behavior, including no parameters.
#[derive(Clone, Copy, Debug)]
pub enum Args {
    None,
    Value(Parameter),
    Subcommands {
        commands: &'static [Subcommand],
        /// A value accepted without a subcommand, as in a worktree name.
        fallback: Option<Parameter>,
    },
}

impl Args {
    pub const fn optional(name: &'static str, values: Values) -> Self {
        Self::Value(Parameter {
            name,
            required: false,
            values,
        })
    }

    pub const fn required(name: &'static str, values: Values) -> Self {
        Self::Value(Parameter {
            name,
            required: true,
            values,
        })
    }

    pub fn is_empty(self) -> bool {
        matches!(self, Self::None)
    }

    /// Required values use angle brackets; optional values use square brackets.
    pub fn usage(self) -> String {
        match self {
            Self::None => String::new(),
            Self::Value(p) => p.usage(),
            Self::Subcommands { commands, fallback } => {
                let mut choices: Vec<String> = fallback.into_iter().map(Parameter::label).collect();
                choices.extend(commands.iter().map(|c| {
                    format!("{} {}", c.word, c.args.usage())
                        .trim_end()
                        .to_string()
                }));
                format!("[{}]", choices.join("|"))
            }
        }
    }

    fn at(self, line: &str, mut start: usize, cursor: usize) -> Option<(Self, usize)> {
        let rest = &line[start..];
        start += rest.len() - rest.trim_start_matches([' ', '\t']).len();
        if cursor < start {
            return None;
        }
        if let Self::Subcommands { commands, .. } = self
            && let Some((word, rest)) = line[start..].split_once(char::is_whitespace)
            && let Some(command) = commands.iter().find(|c| c.word == word)
        {
            return command.args.at(line, line.len() - rest.len(), cursor);
        }
        Some((self, start))
    }
}

/// Dynamic candidates are requested only for the active parameter. In
/// particular, command-name completion does not scan archives or run git.
#[derive(Default)]
pub struct Sources<'a> {
    pub models: &'a [Choice],
    pub said: &'a [Choice],
    pub sessions: Option<&'a dyn Fn() -> &'a [ResumeChoice]>,
    /// All checkouts, with the main checkout first.
    pub worktrees: Option<&'a dyn Fn() -> &'a [Choice]>,
    pub editable: Option<&'a dyn Fn() -> &'a [Choice]>,
    pub servers: Option<&'a dyn Fn() -> Vec<Choice>>,
}

enum Target {
    None,
    Words,
    Argument {
        args: Args,
        start: usize,
    },
    Tool {
        tool: Arc<dyn tool::Tool>,
        start: usize,
    },
}

/// One interpretation of the input and caret, shared by candidate lookup,
/// directory lookup and the placeholder. It never changes submitted text.
pub struct Input<'a> {
    line: &'a str,
    cursor: usize,
    commands: &'a [Command],
    tools: Option<&'a tool::Registry>,
    target: Target,
}

impl<'a> Input<'a> {
    pub fn new(
        line: &'a str,
        cursor: usize,
        commands: &'a [Command],
        tools: Option<&'a tool::Registry>,
    ) -> Self {
        let mut input = Self {
            line,
            cursor,
            commands,
            tools,
            target: Target::None,
        };
        if !line.starts_with('/') || cursor == 0 || !line.is_char_boundary(cursor) {
            return input;
        }
        let Some((word, rest)) = line.split_once(char::is_whitespace) else {
            input.target = Target::Words;
            return input;
        };
        let start = line.len() - rest.len();
        if cursor < start {
            return input;
        }
        if let Some(command) = commands.iter().find(|c| c.word == word) {
            if let Some((args, start)) = command.args.at(line, start, cursor) {
                input.target = Target::Argument { args, start };
            }
        } else if let Some(tool) = tools.and_then(|t| t.get(&word[1..])) {
            input.target = Target::Tool { tool, start };
        }
        input
    }

    pub fn hint(&self) -> Option<String> {
        if self.cursor != self.line.len() || self.line.contains('\n') {
            return None;
        }
        let hint = match &self.target {
            Target::Argument { args, start } if self.line[*start..].is_empty() => args.usage(),
            Target::Tool { tool, start } if self.line[*start..].trim().is_empty() => {
                params(&tool.schema())
                    .map(|p| {
                        if p.required {
                            format!("<{}=>", p.name)
                        } else {
                            format!("[{}=]", p.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            Target::None | Target::Words | Target::Argument { .. } | Target::Tool { .. } => {
                return None;
            }
        };
        (!hint.is_empty()).then_some(hint)
    }

    /// The complete directory argument, spaces included, up to the caret.
    pub fn directory(&self) -> Option<(usize, usize, &str)> {
        match self.target {
            Target::Argument {
                args:
                    Args::Value(Parameter {
                        values: Values::Directory,
                        ..
                    }),
                start,
            } => Some((start, self.cursor, &self.line[start..self.cursor])),
            _ => None,
        }
    }

    pub fn complete(&self, sources: &Sources<'_>) -> Vec<Candidate> {
        match &self.target {
            Target::None => Vec::new(),
            Target::Words => {
                let prefix = self.line;
                let mut offered: Vec<Candidate> = self
                    .commands
                    .iter()
                    .filter(|c| c.word.starts_with(prefix))
                    .map(|c| Candidate {
                        show: c.word.to_string(),
                        line: c.word.to_string(),
                        help: c.help.to_string(),
                        accept: Acceptance::Run,
                        more: !c.args.is_empty(),
                        picked_only: false,
                    })
                    .collect();
                if let Some(tools) = self.tools {
                    offered.extend(
                        slash_tools(self.commands, tools)
                            .filter(|t| t.name().starts_with(&prefix[1..]))
                            .map(|t| Candidate {
                                show: format!("/{}", t.name()),
                                line: format!("/{}", t.name()),
                                help: tool_help(t.as_ref()),
                                accept: Acceptance::Run,
                                more: params(&t.schema()).next().is_some(),
                                picked_only: true,
                            }),
                    );
                }
                offered
            }
            Target::Tool { tool, start } => {
                let rest = &self.line[*start..];
                let last = rest
                    .rsplit_once(char::is_whitespace)
                    .map_or(rest, |(_, last)| last);
                tool_args(
                    tool.as_ref(),
                    &self.line[..self.line.len() - last.len()],
                    last,
                )
            }
            Target::Argument { args, start } => {
                let (head, typed) = (&self.line[..*start], &self.line[*start..]);
                match args {
                    Args::None => Vec::new(),
                    Args::Value(p) => values(p.values, head, typed, sources),
                    Args::Subcommands { commands, fallback } => {
                        // Keep ordinary values first to preserve their default rank.
                        // Subcommands require an explicit choice.
                        let mut offered = fallback
                            .map_or_else(Vec::new, |p| values(p.values, head, typed, sources));
                        offered.extend(commands.iter().filter(|c| c.word.starts_with(typed)).map(
                            |c| Candidate {
                                show: c.word.into(),
                                line: format!("{head}{}", c.word),
                                help: c.help.into(),
                                accept: if c.args.is_empty() {
                                    Acceptance::Run
                                } else {
                                    Acceptance::Continue
                                },
                                more: !c.args.is_empty(),
                                picked_only: true,
                            },
                        ));
                        offered
                    }
                }
            }
        }
    }
}

fn values(source: Values, head: &str, typed: &str, sources: &Sources<'_>) -> Vec<Candidate> {
    match source {
        Values::Text | Values::Directory => Vec::new(),
        Values::Fixed(names) => {
            let choices: Vec<_> = names()
                .into_iter()
                .map(|name| Choice {
                    name,
                    note: String::new(),
                })
                .collect();
            offer(&choices, head, typed)
        }
        Values::Models
        | Values::Worktrees
        | Values::LinkedWorktrees
        | Values::Editable
        | Values::Servers
            if typed.contains(char::is_whitespace) =>
        {
            Vec::new()
        }
        Values::Models => offer(sources.models, head, typed),
        Values::Worktrees => offer(sources.worktrees.map_or(&[], |f| f()), head, typed),
        Values::LinkedWorktrees => {
            let trees: &[Choice] = sources.worktrees.map_or(&[], |f| f());
            let mut offered = offer(trees.get(1..).unwrap_or_default(), head, typed);
            if typed.is_empty() {
                for candidate in &mut offered {
                    candidate.picked_only = true;
                }
            }
            offered
        }
        Values::Editable => offer(sources.editable.map_or(&[], |f| f()), head, typed),
        Values::Servers => {
            let mut offered = offer(&sources.servers.map_or_else(Vec::new, |f| f()), head, typed);
            // Without a name, the operation applies to the whole set. An
            // unchosen suggestion must not silently narrow that operation.
            if typed.is_empty() {
                for candidate in &mut offered {
                    candidate.picked_only = true;
                }
            }
            offered
        }
        Values::Said => {
            let named = sources.said.iter().any(|s| s.name == typed);
            sources
                .said
                .iter()
                .rev()
                .filter(|s| {
                    if named {
                        s.name == typed
                    } else {
                        pi_store::text::matches(&s.note, typed)
                    }
                })
                .map(|s| Candidate {
                    show: s.note.clone(),
                    line: format!("{head}{}", s.name),
                    help: String::new(),
                    accept: Acceptance::Run,
                    more: false,
                    picked_only: false,
                })
                .collect()
        }
        Values::Sessions => {
            let sessions: &[ResumeChoice] = sources.sessions.map_or(&[], |f| f());
            let offered: Vec<_> = sessions
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
                    line: format!("{head}{}", s.id),
                    accept: Acceptance::Run,
                    help,
                    more: false,
                    picked_only: false,
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BUILTIN, command_help};
    use super::*;

    #[test]
    fn declarations_provide_help_hints_and_candidates_under_any_command_name() {
        fn modes() -> Vec<String> {
            vec!["fast".into(), "slow".into()]
        }
        let commands = [
            Command::surface(
                "/speed",
                Args::optional("mode", Values::Fixed(modes)),
                "choose a mode",
            ),
            Command::surface(
                "/choose",
                Args::optional("name", Values::Models),
                "choose a model",
            ),
            Command::surface(
                "/visit",
                Args::optional("dir", Values::Directory),
                "choose a directory",
            ),
            Command::surface(
                "/say",
                Args::optional("text", Values::Text),
                "say something",
            ),
        ];
        let models = [Choice {
            name: "a-model".into(),
            note: String::new(),
        }];
        let sources = Sources {
            models: &models,
            ..Default::default()
        };
        let resolve = |line: &str| Input::new(line, line.len(), &commands, None).complete(&sources);
        let help = command_help(&commands);
        assert_eq!(help.rows[0].cells[0], "/speed [fast|slow]");
        assert_eq!(
            Input::new("/speed ", 7, &commands, None).hint().as_deref(),
            Some("[fast|slow]")
        );
        assert_eq!(resolve("/speed s")[0].line, "/speed slow");
        assert_eq!(resolve("/choose a")[0].line, "/choose a-model");
        assert_eq!(
            Input::new("/choose a", 8, &commands, None).complete(&sources)[0].line,
            "/choose a-model",
            "a whole-line candidate must retain text after the caret"
        );
        assert!(resolve("/say ").is_empty());
        assert_eq!(
            Input::new("/say ", 5, &commands, None).hint().as_deref(),
            Some("[text]")
        );

        let line = "/visit a long directory/中文";
        let input = Input::new(line, line.len(), &commands, None);
        assert_eq!(
            input.directory(),
            Some((7, line.len(), "a long directory/中文"))
        );
        assert_eq!(
            Input::new(line, 13, &commands, None).directory(),
            Some((7, 13, "a long"))
        );
        assert!(input.hint().is_none());
    }

    #[test]
    fn subcommands_select_the_next_parameter_and_only_query_its_source() {
        let calls = std::cell::Cell::new(0);
        let servers = || {
            calls.set(calls.get() + 1);
            vec![Choice {
                name: "alpha".into(),
                note: String::new(),
            }]
        };
        let sources = Sources {
            servers: Some(&servers),
            ..Default::default()
        };
        let resolve = |line: &str| Input::new(line, line.len(), BUILTIN, None).complete(&sources);
        let rows = resolve("/mcp ");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].line, "/mcp restart");
        assert!(rows[0].more && rows[0].picked_only);
        assert_eq!(rows[0].accept, Acceptance::Continue);
        assert_eq!(calls.get(), 0);
        assert!(
            resolve("/mcp restart ")[0].picked_only,
            "an empty name still means all servers"
        );
        assert_eq!(resolve("/mcp restart a")[0].line, "/mcp restart alpha");
        assert_eq!(calls.get(), 2);
        assert!(resolve("/mcp restart alpha extra").is_empty());
        assert_eq!(calls.get(), 2, "a third word must not query servers");
        for (line, hint) in [
            ("/mcp ", "[restart [name]]"),
            ("/mcp restart ", "[name]"),
            ("/worktree rm ", "[name]"),
        ] {
            assert_eq!(
                Input::new(line, line.len(), BUILTIN, None)
                    .hint()
                    .as_deref(),
                Some(hint)
            );
        }
        for line in ["/mcp invalid ", "/mcp restart alpha", "/mcp restart \n"] {
            assert!(Input::new(line, line.len(), BUILTIN, None).hint().is_none());
        }
    }
}
