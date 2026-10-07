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

/// The built-in `pi-tool` skill: how to write a script tool, and a skill.
pub const SKILL: &str = include_str!("../../skills/pi-tool/SKILL.md");

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
    skipped: Vec<Skip>,
}

/// A script that could not be registered, and why.
#[derive(Debug)]
pub struct Skip {
    pub path: PathBuf,
    pub why: String,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.why)
    }
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
        self.fresh(|seen| seen.skipped.iter().map(Skip::to_string).collect())
    }

    fn fresh<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
        let stamps = stamps(&self.dir);
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.stamps != stamps {
            let (scripts, skipped) = discover_in(&self.dir);
            let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
            for script in scripts {
                tools.push(Arc::new(script));
            }
            // A broken script stands in the list as what is wrong with it: the
            // model reads the list every turn, so that is where it learns why.
            for skip in &skipped {
                tracing::warn!(target: "pi::tools", why = %skip, "script skipped");
                if let Some(broken) = Broken::of(skip)
                    && tools.iter().all(|t| t.name() != broken.name)
                {
                    tools.push(Arc::new(broken));
                }
            }
            *seen = Seen {
                stamps,
                tools,
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
pub fn discover_in(dir: &Path) -> (Vec<Script>, Vec<Skip>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut tools = Vec::new();
    let mut skipped = Vec::new();
    // Sorted, so the tool list and which of two clashing names wins hold
    // still from run to run; `read_dir` promises no order.
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
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
        // Sent to the provider as is, and one that will not take it fails
        // every request, not only this tool's.
        if !usable(name) {
            let why = format!("`{name}` is not a tool name: a-z, A-Z, 0-9, - and _, up to 64");
            skipped.push(Skip { path, why });
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            let why = "not utf-8".to_string();
            skipped.push(Skip { path, why });
            continue;
        };
        let (description, args) = match interface(&text) {
            Ok(found) => found,
            Err(why) => {
                skipped.push(Skip { path, why });
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

// What every provider accepts as a tool name.
fn usable(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(name_char)
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

// The stand-in for a script that does not read: its name as near as a tool
// name can come, and what is wrong in its description and its every answer.
struct Broken {
    name: String,
    description: String,
}

impl Broken {
    fn of(skip: &Skip) -> Option<Self> {
        let stem = skip.path.file_stem()?.to_str()?;
        let name: String = stem
            .chars()
            .map(|c| if name_char(c) { c } else { '_' })
            .take(64)
            .collect();
        Some(Self {
            name,
            description: format!(
                "Not usable yet. {skip}. Fix the file and it is offered as the tool it \
                 declares from the next turn; until then a call only says this."
            ),
        })
    }
}

#[async_trait::async_trait]
impl Tool for Broken {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn tier(&self) -> tool::Tier {
        tool::Tier::Exec
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(
        &self,
        _: serde_json::Value,
        _: &tool::Ctx,
    ) -> Result<tool::ToolOutput, tool::ToolError> {
        Err(tool::ToolError::Invalid(self.description.clone()))
    }
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

    // The skill is how a model learns to write a script; if its example stops
    // parsing, every script written from it is skipped without a word.
    #[test]
    fn the_pi_tool_example_is_a_script_pi_accepts() {
        let example = SKILL
            .split("```rust\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("a rust example");
        let (description, args) = interface(example).unwrap();
        assert!(!description.is_empty());
        assert_eq!(args.len(), 1, "{args:?}");
        assert!(example.contains(&format!("std::env::var(\"{}\")", args[0].0)));
    }

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
        assert_eq!(scripts.skipped().len(), 1);
    }

    // A script the model wrote wrong would otherwise just not appear, with
    // nothing to say why: its stand-in in the list is where the why goes.
    #[test]
    fn a_broken_script_says_why_where_the_model_looks() {
        use tool::Source as _;
        let dir = tempfile::tempdir().unwrap();
        let scripts = Dir::new(dir.path());
        let path = dir.path().join("count.rs");
        std::fs::write(&path, "---\n[package]\n---\nfn main() {}\n").unwrap();
        let tools = scripts.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "count");
        assert!(
            tools[0].description().contains("no [package] description"),
            "{}",
            tools[0].description()
        );

        std::fs::write(
            &path,
            "---\n[package]\ndescription = \"Count lines\"\n---\nfn main() {}\n",
        )
        .unwrap();
        assert_eq!(scripts.tools()[0].description(), "Count lines", "fixed");
    }

    // A tool name no provider takes would fail every request, not only its own.
    #[test]
    fn a_name_no_provider_takes_never_reaches_one() {
        use tool::Source as _;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("my tool.rs"),
            "---\n[package]\ndescription = \"x\"\n---\nfn main() {}\n",
        )
        .unwrap();
        let tools = Dir::new(dir.path()).tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "my_tool");
        assert!(tools[0].description().contains("not a tool name"));
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
