//! `.env` beside the scripts: values every script is started with, such as
//! the keys their services want. Only scripts get them — never `bash`, whose
//! output the model reads, and never pi's own environment.

use std::path::Path;

use super::script::is_identifier;

/// The file's name inside the tools directory. A dotfile, so the scan that
/// finds scripts passes over it.
pub const FILE: &str = ".env";

/// The `KEY=value` lines of the env file in `dir`; none when there is none.
pub fn read(dir: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(dir.join(FILE))
        .map(|text| parse(&text))
        .unwrap_or_default()
}

// Literal values: no expansion, no escapes, nothing run. A line that is not
// `NAME=value` is passed over rather than guessed at.
fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            let value = [b'"', b'\'']
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

/// What to say when the env file in `dir` is readable by others; it holds keys.
pub fn loose(dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(FILE);
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
        let _ = dir;
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
}
