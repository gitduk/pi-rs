//! The `.env` a pi directory keeps: the directory holding `tools/` owns it,
//! so `~/.pi/tools` reads `~/.pi/.env` and a checkout's `.pi/tools` reads
//! `.pi/.env`. One file serves the scripts, the shell calls and a key the
//! settings name as `$NAME`.

use std::path::{Path, PathBuf};

const FILE: &str = ".env";

/// The env file for a tools directory: the directory holding it owns it.
pub fn path(tools: &Path) -> PathBuf {
    tools.parent().unwrap_or(tools).join(FILE)
}

/// pi's own env file, `$PI_HOME/.env`.
pub fn home_file() -> Option<PathBuf> {
    crate::dir().map(|root| path(&root.join("tools")))
}

/// The `KEY=value` lines of the env file a tools directory keeps; none when
/// there is none.
pub fn read(tools: &Path) -> Vec<(String, String)> {
    values(&path(tools))
}

// The whole file, every time: it is small, and an edit has to reach the next
// call without a restart.
fn values(path: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .map(|text| parse(&text))
        .unwrap_or_default()
}

/// What `name` is set to in the env file at `path`, the last say winning;
/// none when it is not set.
pub(crate) fn value(path: &Path, name: &str) -> Option<String> {
    values(path)
        .into_iter()
        .rev()
        .find_map(|(key, value)| (key == name).then_some(value))
}

// Literal values: no expansion, no escapes, nothing run. A line that is not
// `NAME=value` is passed over rather than guessed at.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            let value = b"\"'"
                .iter()
                .find_map(|q| {
                    let q = *q as char;
                    value.strip_prefix(q)?.strip_suffix(q)
                })
                .unwrap_or(value);
            is_identifier(key).then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// A name a shell and this file both accept: a letter or `_`, then those or a
/// digit.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What to say when the env file for `tools` is readable by others; it holds
/// keys.
pub fn loose(tools: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = path(tools);
        let mode = std::fs::metadata(&path).ok()?.permissions().mode();
        (mode & 0o077 != 0).then(|| {
            format!(
                "{} is readable by others (mode {:o}); chmod 600 it",
                path.display(),
                mode & 0o777
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = tools;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_a_literal_name_and_value_or_nothing() {
        let text = "# keys\n\nA=1\n  B = two words \nC=\"quoted\"\nD='single'\n\
                    E=$HOME\nF=a=b\nbad-name=x\nno equals\n";
        let said = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert_eq!(
            parse(text),
            [
                said("A", "1"),
                said("B", "two words"),
                said("C", "quoted"),
                said("D", "single"),
                said("E", "$HOME"),
                said("F", "a=b"),
            ]
        );
    }

    // The directory that owns `tools/` owns their values, not `tools/` itself.
    #[test]
    fn the_file_sits_beside_the_tools_directory() {
        let root = tempfile::tempdir().unwrap();
        let tools = root.path().join(".pi/tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(root.path().join(".pi/.env"), "TOKEN=1234567890abcdef\n").unwrap();
        assert_eq!(path(&tools), root.path().join(".pi/.env"));
        assert_eq!(
            read(&tools),
            [("TOKEN".to_string(), "1234567890abcdef".to_string())]
        );
        assert_eq!(
            value(&path(&tools), "TOKEN").as_deref(),
            Some("1234567890abcdef")
        );
    }
}
