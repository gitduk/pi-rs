use serde_json::Value;
use std::borrow::Cow;

// The text at `key`, when it is there to say anything.
fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn leading(args: &Value) -> Option<&str> {
    // A grep's pattern identifies the call more clearly than its path.
    [
        "description",
        "pattern",
        "command",
        "path",
        "query",
        "url",
        "prompt",
        "name",
    ]
    .into_iter()
    .find_map(|key| text(args, key))
}

/// The argument a call was made with, as text: the shell command it ran when
/// it ran one, else its leading argument, else its arguments as JSON. A head
/// says a call's label when it has one, so this is what opening the call reads.
pub fn argument(args: &Value) -> Cow<'_, str> {
    text(args, "command")
        .or_else(|| leading(args))
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(args.to_string()))
}

/// A compact call summary, using `key=value` when no leading argument is known.
pub fn summary(args: &Value) -> Cow<'_, str> {
    leading(args)
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(arguments(args)))
}

/// A call's title, on one line: the argument that names the call, with the
/// tool's name in front unless the call was made with a command of its own.
/// Borrows `name` when its arguments name nothing.
pub fn title<'a>(name: &'a str, args: &Value) -> Cow<'a, str> {
    let Some(line) = argument_line(args) else {
        return Cow::Borrowed(name);
    };
    if text(args, "command").is_some_and(|command| one_line(command) == line) {
        return Cow::Owned(line);
    }
    Cow::Owned(label(name, &line))
}

// The argument that names a call, cut to one line — `git status`, `src/main.rs`.
fn argument_line(args: &Value) -> Option<String> {
    if args.as_object().is_none_or(|args| args.is_empty()) {
        return None;
    }
    let line = one_line(&summary(args));
    (!line.is_empty()).then_some(line)
}

// `text` as one line of a call: its first line, every control character a space.
fn one_line(text: &str) -> String {
    let first: String = text
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    first.trim().to_string()
}

/// A call's line: its name, then `text` — the name dropped when `text` already
/// opens with it. `text` is what the call was made with, or what it showed.
pub fn label(name: &str, text: &str) -> String {
    if text.is_empty() {
        return name.to_string();
    }
    match text.strip_prefix(name) {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => text.to_string(),
        _ => format!("{name} {text}"),
    }
}

// All supplied arguments as `key=value` pairs, quoting non-word strings.
fn arguments(args: &Value) -> String {
    let Some(args) = args.as_object().filter(|args| !args.is_empty()) else {
        return args.to_string();
    };
    args.iter()
        .map(|(key, value)| {
            let value = match value {
                Value::String(text) => word(text),
                value => value.to_string(),
            };
            format!("{}={value}", word(key))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn word(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || "_-./:@%+".contains(c))
    {
        text.to_string()
    } else {
        serde_json::to_string(text).expect("a string serializes")
    }
}
