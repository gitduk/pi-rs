//! User-defined tools: a script dropped into `~/.pi/tools/` is a tool.
//! The comment header — from the shebang to the first non-comment line —
//! is its interface: `# description:` what it does, and every other
//! `# name: what it is` line declares one argument, however many. String
//! arguments also arrive as environment variables `$name`: absent when not
//! passed, never shadowing an inherited name, and capped in size — while
//! the call's full JSON stays on stdin and stdout is the result; stderr
//! only surfaces when the exit is non-zero. Output runs through the same
//! bounded capture as bash's, so a runaway script floods neither memory
//! nor transcript.

mod script;

use std::path::Path;

pub use script::Script;

/// Scan the given directory. A script whose header carries no description
/// is not registered — a tool the model cannot see described is a trap,
/// and it is named in the skipped list instead.
pub fn discover_in(dir: &Path) -> (Vec<Script>, Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut tools = Vec::new();
    let mut skipped = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            skipped.push(format!("{}: not utf-8", path.display()));
            continue;
        };
        let Some((description, args)) = header(&text) else {
            skipped.push(format!("{}: no # description: line", path.display()));
            continue;
        };
        tools.push(Script {
            name: name.to_string(),
            description,
            args,
            path,
        });
    }
    (tools, skipped)
}

/// The `#` comment block a script opens with is its interface declaration.
fn header(text: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut description = None;
    let mut args = Vec::new();
    for line in text.lines() {
        let Some(line) = line.strip_prefix('#') else {
            break;
        };
        let Some((key, rest)) = line.trim().split_once(':') else {
            continue;
        };
        let value = rest.trim();
        match key.trim() {
            "description" if !value.is_empty() => description = Some(value.to_string()),
            // Everything else in the header names an argument; the value is
            // what the schema says about it.
            key if !key.is_empty() => args.push((key.to_string(), value.to_string())),
            _ => {}
        }
    }
    description.map(|d| (d, args))
}
