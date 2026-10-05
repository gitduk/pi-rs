//! User-defined tools: a Rust script dropped into `$PI_HOME/tools/` (by
//! default `~/.pi/tools/`) is a tool.
//! Its cargo frontmatter is its interface: `[package] description` says what
//! it does, and each `name = "what it is"` under `[package.metadata.pi.args]`
//! declares one argument, however many. Arguments also arrive as environment
//! variables `name`: absent when not passed, never shadowing an inherited
//! name, and capped in size — while the call's full JSON stays on stdin and
//! stdout is the result; stderr, compile errors included, only surfaces when
//! the exit is non-zero. Output runs through the same bounded capture as
//! bash's, so a runaway script floods neither memory nor transcript.

mod script;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use tool::Tool;

pub use script::{Script, cargo_script, run_script};

/// The tools directory as a `tool::Source`: looked at whenever the tool set
/// is asked, parsed again only when a file in it changed.
pub struct Dir {
    dir: PathBuf,
    seen: Mutex<Seen>,
}

#[derive(Default)]
struct Seen {
    stamps: Vec<(PathBuf, Option<SystemTime>, u64)>,
    tools: Vec<Arc<dyn Tool>>,
    skipped: Vec<String>,
}

impl Dir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            seen: Mutex::default(),
        }
    }

    /// The scripts that could not be registered, and why.
    pub fn skipped(&self) -> Vec<String> {
        self.fresh(|seen| seen.skipped.clone())
    }

    fn fresh<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
        let stamps = stamps(&self.dir);
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.stamps != stamps {
            let (tools, skipped) = discover_in(&self.dir);
            for why in &skipped {
                tracing::warn!(target: "pi::tools", %why, "script skipped");
            }
            *seen = Seen {
                stamps,
                tools: tools
                    .into_iter()
                    .map(|t| Arc::new(t) as Arc<dyn Tool>)
                    .collect(),
                skipped,
            };
        }
        read(&seen)
    }
}

impl tool::Source for Dir {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.fresh(|seen| seen.tools.clone())
    }
}

// What says a script changed without reading it: its name, time and size.
fn stamps(dir: &Path) -> Vec<(PathBuf, Option<SystemTime>, u64)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.path(), meta.modified().ok(), meta.len()))
        })
        .collect();
    out.sort();
    out
}

/// Scan the given directory for `.rs` files; anything else is not a script.
/// One whose frontmatter carries no description is not registered — a tool
/// the model cannot see described is a trap — and is named in the skipped
/// list instead.
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
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            skipped.push(format!("{}: not utf-8", path.display()));
            continue;
        };
        let (description, args) = match interface(&text) {
            Ok(found) => found,
            Err(why) => {
                skipped.push(format!("{}: {why}", path.display()));
                continue;
            }
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

type Interface = (String, Vec<(String, String)>);

/// The frontmatter's manifest is the interface declaration: the same block
/// cargo reads the script's dependencies from.
fn interface(text: &str) -> Result<Interface, String> {
    let manifest = frontmatter(text).ok_or("no --- frontmatter")?;
    let manifest: toml::Table = manifest.parse().map_err(|e| format!("frontmatter: {e}"))?;
    let package = manifest.get("package");
    let description = package
        .and_then(|p| p.get("description"))
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or("no [package] description")?;
    let mut args = Vec::new();
    let declared = package
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("pi"))
        .and_then(|p| p.get("args"));
    if let Some(declared) = declared {
        let declared = declared
            .as_table()
            .ok_or("[package.metadata.pi.args] is not a table")?;
        for (name, what) in declared {
            let what = what
                .as_str()
                .ok_or_else(|| format!("argument {name}: describe it as a string"))?;
            args.push((name.clone(), what.to_string()));
        }
    }
    Ok((description.to_string(), args))
}

/// The manifest between the `---` fences, after an optional shebang.
fn frontmatter(text: &str) -> Option<&str> {
    let body = match text.strip_prefix("#!") {
        Some(rest) => rest.split_once('\n')?.1,
        None => text,
    };
    let (open, body) = body.split_once('\n')?;
    if !matches!(open.trim_end(), "---" | "---cargo") {
        return None;
    }
    let mut end = 0;
    for line in body.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some(&body[..end]);
        }
        end += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frontmatter_declares_description_and_args() {
        let text = "#!/usr/bin/env cargo\n---\n[package]\ndescription = \"Count lines\"\n\n\
                    [package.metadata.pi.args]\nfile = \"file to count\"\n\n\
                    [dependencies]\nserde_json = \"1\"\n---\nfn main() {}\n";
        let (description, args) = interface(text).unwrap();
        assert_eq!(description, "Count lines");
        assert_eq!(args, vec![("file".into(), "file to count".into())]);
    }

    #[test]
    fn a_script_without_a_description_is_refused_by_name() {
        let bare = "---\n[dependencies]\n---\nfn main() {}\n";
        assert_eq!(interface(bare).unwrap_err(), "no [package] description");
        let unfenced = "fn main() {}\n";
        assert_eq!(interface(unfenced).unwrap_err(), "no --- frontmatter");
        let unclosed = "---\n[package]\ndescription = \"x\"\nfn main() {}\n";
        assert_eq!(interface(unclosed).unwrap_err(), "no --- frontmatter");
    }

    #[test]
    fn a_script_written_later_is_a_tool_on_the_next_look() {
        use tool::Source as _;
        let dir = tempfile::tempdir().unwrap();
        let scripts = Dir::new(dir.path());
        assert!(scripts.tools().is_empty());
        std::fs::write(
            dir.path().join("count.rs"),
            "---\n[package]\ndescription = \"Count\"\n---\nfn main() {}\n",
        )
        .unwrap();
        assert_eq!(scripts.tools()[0].name(), "count");
        std::fs::write(dir.path().join("count.rs"), "fn main() {}\n").unwrap();
        assert!(scripts.tools().is_empty());
        assert_eq!(scripts.skipped().len(), 1);
    }

    #[test]
    fn only_rust_scripts_are_registered_and_the_rest_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("count.rs"),
            "---\n[package]\ndescription = \"Count\"\n---\nfn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("old.sh"),
            "#!/usr/bin/env bash\n# description: x\n",
        )
        .unwrap();
        let (found, skipped) = discover_in(dir.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "count");
        assert!(skipped.is_empty(), "{skipped:?}");
    }
}
