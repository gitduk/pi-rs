use serde_json::Value;
use std::borrow::Cow;

fn leading(args: &Value) -> Option<&str> {
    // A grep's pattern identifies the call more clearly than its path.
    [
        "description",
        "pattern",
        "command",
        "path",
        "query",
        "prompt",
        "name",
    ]
    .into_iter()
    .find_map(|key| args.get(key).and_then(Value::as_str))
    .filter(|text| !text.is_empty())
}

/// The call's leading argument, or its full JSON when no leading argument is known.
pub fn asked(args: &Value) -> Cow<'_, str> {
    leading(args)
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(args.to_string()))
}

/// A compact call summary, using `key=value` when no leading argument is known.
pub fn summary(args: &Value) -> Cow<'_, str> {
    leading(args)
        .map(Cow::Borrowed)
        .unwrap_or_else(|| Cow::Owned(arguments(args)))
}

/// All supplied arguments as `key=value` pairs, quoting non-word strings.
pub fn arguments(args: &Value) -> String {
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
