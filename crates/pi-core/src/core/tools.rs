//! What a surface knows of a tool by its name, so it never names one itself.

use std::sync::LazyLock;

use tool::Tier;

/// Whether the tool `name` changes files. What it changed is the result, so
/// a surface keeps it in view rather than folding it away.
pub fn modifies(name: &str) -> bool {
    // Built-ins only: no other source offers a tool at the write tier.
    static WRITERS: LazyLock<Vec<String>> = LazyLock::new(|| {
        let tools = toolbox::builtin();
        tools
            .names()
            .into_iter()
            .filter(|n| tools.get(n).is_some_and(|t| t.tier() == Tier::Write))
            .map(String::from)
            .collect()
    });
    WRITERS.iter().any(|w| w == name)
}

/// The lines of what the tool `name` printed that a screen shows.
pub fn shown<'a>(name: &str, output: &'a str) -> impl Iterator<Item = &'a str> {
    let bash = name == toolbox::bash::Bash::NAME;
    output
        .lines()
        .filter(move |l| !(bash && super::bash::is_stream_tag(l)))
}
