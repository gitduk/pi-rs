//! User-defined tools: a Rust script dropped into `$PI_HOME/tools/` (by
//! default `~/.pi/tools/`) is a tool.
//! Its cargo frontmatter is its interface: `[package] description` says what
//! it does, and each `name = "what it is"` under `[package.metadata.pi.required]`
//! or `[package.metadata.pi.optional]` declares one string argument.
//! Either table may be omitted. Arguments also arrive as environment
//! variables `name`: absent when not passed, never shadowing an inherited
//! name, and capped in size — while the call's full JSON stays on stdin and
//! stdout is the result; stderr, compile errors included, only surfaces when
//! the exit is non-zero. Output runs through the same bounded capture as
//! bash's, so a runaway script floods neither memory nor transcript.

pub mod env;
#[cfg(unix)]
pub use crate::channel::process::foreground as run_asking;
#[cfg(not(unix))]
pub async fn run_asking(
    _name: &str,
    cmd: tokio::process::Command,
    input: Vec<u8>,
    timeout: std::time::Duration,
    ctx: &tool::Ctx,
    _asks: &dyn crate::channel::Ask,
) -> Result<crate::process::Exited, tool::ToolError> {
    crate::process::run(cmd, Some(input), timeout, ctx).await
}
mod script;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tool::Tool;
use tool::stamp::{Stamp, Stamped};

use crate::bash::Bash;
use crate::channel::{Ask, NoOne};

use script::{Arg, Runner};
pub use script::{Script, cargo_script, run_script};

/// The tools directory as a `tool::Source`: looked at whenever the tool set
/// is asked, parsed again only when a file in it changed.
pub struct Dir {
    dir: PathBuf,
    seen: Stamped<Seen>,
    shell: Option<Arc<Bash>>,
    asks: Arc<dyn Ask>,
}

struct Seen {
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
            seen: Stamped::default(),
            shell: None,
            asks: Arc::new(NoOne),
        }
    }

    /// Arm script wrappers with the same shell and question service as their caller.
    pub fn with_shell(&self, shell: Arc<Bash>, asks: Arc<dyn Ask>) -> Self {
        Self {
            dir: self.dir.clone(),
            seen: Stamped::default(),
            shell: Some(shell),
            asks,
        }
    }

    /// The scripts that could not be registered, and why.
    pub fn skipped(&self) -> Vec<String> {
        self.fresh(|seen| seen.skipped.iter().map(Skip::to_string).collect())
    }

    fn fresh<T>(&self, read: impl FnOnce(&Seen) -> T) -> T {
        self.seen.read(stamps(&self.dir), || self.build(), read)
    }

    fn build(&self) -> Seen {
        let (scripts, skipped) = discover_in(&self.dir);
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for script in scripts {
            if script.background && self.shell.is_none() {
                continue;
            }
            tools.push(Arc::new(Script {
                shell: self.shell.clone(),
                asks: self.asks.clone(),
                ..script
            }));
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
        Seen { tools, skipped }
    }
}

impl tool::Source for Dir {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.fresh(|seen| seen.tools.clone())
    }
}

// What says a script changed without reading it: its name, time and size.
fn stamps(dir: &Path) -> Vec<Stamp> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some(tool::stamp::from_meta(e.path(), &meta))
        })
        .collect();
    out.sort();
    out
}

/// Scan the given directory for scripts: a `.rs` cargo script, or any file
/// whose first line is a `#!`. Anything else is not a script. One whose
/// interface does not read is not registered — a tool the model cannot see
/// described is a trap — and is named in the skipped list instead.
pub fn discover_in(dir: &Path) -> (Vec<Script>, Vec<Skip>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut tools: Vec<Script> = Vec::new();
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
        let rust = path.extension().is_some_and(|e| e == "rs");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) if rust => {
                let why = "not utf-8".to_string();
                skipped.push(Skip { path, why });
                continue;
            }
            Err(_) => continue,
        };
        let runner = match (rust, script::shebang(&text)) {
            (true, _) => Runner::Cargo,
            (false, Some((program, arg))) => Runner::Shebang(program, arg),
            (false, None) => continue,
        };
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
        if let Some(first) = tools.iter().find(|t| t.name == name) {
            let why = format!("`{name}` is taken by {}", first.path.display());
            skipped.push(Skip { path, why });
            continue;
        }
        let read = match runner {
            Runner::Cargo => interface(&text),
            Runner::Shebang(..) => header(&text),
        };
        let (description, args, background) = match read {
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
            background,
            path,
            runner,
            shell: None,
            asks: Arc::new(NoOne),
        });
    }
    (tools, skipped)
}

// What every provider accepts as a tool name.
fn usable(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.chars().all(tool::name_char)
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
            .map(|c| if tool::name_char(c) { c } else { '_' })
            .take(64)
            .collect();
        Some(Self {
            name,
            description: format!(
                "Not usable yet. {skip}. Fix the file to make its declared tool available on the next turn. \
                 Until then, a call only reports this problem."
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

type Interface = (String, Vec<Arg>, bool);

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
    let declared = package
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("pi"));
    const PREFIX: &str = "package.metadata.pi.";
    if let Some(declared) = declared {
        known(declared, &["required", "optional", "background"], PREFIX)?;
    }
    let args = declared_args(declared, PREFIX)?;
    Ok((description.to_string(), args, background_mode(declared)?))
}

fn background_mode(table: Option<&toml::Value>) -> Result<bool, String> {
    match table.and_then(|table| table.get("background")) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "background must be true or false".to_string()),
    }
}

// A key nothing reads would drop what it declares without a word.
fn known(table: &toml::Value, keys: &[&str], prefix: &str) -> Result<(), String> {
    let Some(table) = table.as_table() else {
        return Ok(());
    };
    match table.keys().find(|key| !keys.contains(&key.as_str())) {
        None => Ok(()),
        Some(key) => Err(format!(
            "unknown key `{prefix}{key}`: expected one of {}",
            keys.join(", ")
        )),
    }
}

// Required and optional declarations share the same string descriptions.
fn declared_args(table: Option<&toml::Value>, prefix: &str) -> Result<Vec<Arg>, String> {
    let Some(table) = table else {
        return Ok(Vec::new());
    };
    let table = table
        .as_table()
        .ok_or("argument declaration is not a table")?;
    let mut args: Vec<Arg> = Vec::new();
    for (section, required) in [("required", true), ("optional", false)] {
        let Some(declared) = table.get(section) else {
            continue;
        };
        let declared = declared
            .as_table()
            .ok_or_else(|| format!("[{prefix}{section}] is not a table"))?;
        for (name, description) in declared {
            if args.iter().any(|arg| arg.name == *name) {
                return Err(format!(
                    "argument {name} appears in both [{prefix}required] and [{prefix}optional]"
                ));
            }
            let description = description
                .as_str()
                .ok_or_else(|| format!("argument {name}: describe it as a string"))?;
            args.push(Arg {
                name: name.clone(),
                description: description.to_string(),
                required,
            });
        }
    }
    Ok(args)
}

/// Any other script's interface: TOML in a comment block right after the
/// `#!` line, between `---` fences that carry the comment's own prefix —
/// `# ---`, `// ---` — with `description`, `[required]` and `[optional]`.
fn header(text: &str) -> Result<Interface, String> {
    const NONE: &str = "no `# ---` header after the #! line";
    let mut lines = text.lines().skip(1).skip_while(|l| l.trim().is_empty());
    let fence = lines.next().ok_or(NONE)?.trim_end();
    let prefix = fence
        .strip_suffix("---")
        .filter(|p| !p.trim().is_empty())
        .ok_or(NONE)?;
    let bare = prefix.trim_end();
    let mut toml = String::new();
    let mut closed = false;
    for line in lines {
        let line = line.trim_end();
        if line == fence {
            closed = true;
            break;
        }
        let body = line
            .strip_prefix(prefix)
            .or_else(|| line.strip_prefix(bare))
            .ok_or("a header line is not a comment. Close it with the same `---` line.")?;
        toml.push_str(body);
        toml.push('\n');
    }
    if !closed {
        return Err("the header is not closed by a second `---` line".into());
    }
    let table: toml::Value = toml::from_str(&toml).map_err(|e| format!("header: {e}"))?;
    let description = table
        .get("description")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .ok_or("no description in the header")?;
    known(
        &table,
        &["description", "required", "optional", "background"],
        "",
    )?;
    let args = declared_args(Some(&table), "")?;
    Ok((
        description.to_string(),
        args,
        background_mode(Some(&table))?,
    ))
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
    use serde_json::json;

    // Exercise both declarations through discovery and the schema sent to callers.
    fn discover_declarations(declaration: &str) -> (Vec<Script>, Vec<Skip>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cargo.rs"),
            format!(
                "---\n[package]\ndescription = \"Test\"\n[package.metadata.pi]\n{declaration}\n---\nfn main() {{}}\n"
            ),
        )
        .unwrap();
        let commented = declaration
            .lines()
            .map(|line| format!("# {line}\n"))
            .collect::<String>();
        std::fs::write(
            dir.path().join("shell"),
            format!("#!/bin/sh\n# ---\n# description = \"Test\"\n{commented}# ---\n"),
        )
        .unwrap();
        discover_in(dir.path())
    }

    #[test]
    fn background_mode_is_declared_in_both_script_formats() {
        for (text, expected) in [
            ("true", Ok(true)),
            ("false", Ok(false)),
            ("42", Err("background must be true or false")),
        ] {
            let rust = format!(
                "---\n[package]\ndescription = \"x\"\n[package.metadata.pi]\nbackground = {text}\n---\n"
            );
            let shell =
                format!("#!/bin/sh\n# ---\n# description = \"x\"\n# background = {text}\n# ---\n");
            for parsed in [interface(&rust), header(&shell)] {
                assert_eq!(
                    parsed
                        .map(|(_, _, mode)| mode)
                        .as_ref()
                        .map_err(String::as_str),
                    expected.as_ref().map_err(|error| *error)
                );
            }
        }
    }

    #[test]
    fn required_and_optional_tables_define_the_schema_without_changing_descriptions() {
        let (found, skipped) = discover_declarations(
            "required = { prompt = 'optional: literal description' }\noptional = { after = 'A delay' }",
        );
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(found.len(), 2);
        for script in found {
            assert_eq!(
                script.schema(),
                json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "optional: literal description" },
                        "after": { "type": "string", "description": "A delay" },
                    },
                    "required": ["prompt"],
                })
            );
        }
    }

    #[test]
    fn either_argument_table_may_be_omitted() {
        for (declaration, expected, count) in [
            ("", json!([]), 0),
            ("required = {}\noptional = {}", json!([]), 0),
            ("required = { prompt = 'A request' }", json!(["prompt"]), 1),
            ("optional = { after = 'A delay' }", json!([]), 1),
        ] {
            let (found, skipped) = discover_declarations(declaration);
            assert!(skipped.is_empty(), "{skipped:?}");
            assert_eq!(found.len(), 2);
            for script in found {
                assert_eq!(script.schema()["required"], expected);
                assert_eq!(script.args.len(), count);
            }
        }
    }

    #[test]
    fn invalid_argument_declarations_are_refused_in_both_formats() {
        for (declaration, message) in [
            (
                "required = { prompt = 'A request' }\noptional = { prompt = 'Another request' }",
                "argument prompt appears in both",
            ),
            ("required = 1", "required] is not a table"),
            ("optional = 'delay'", "optional] is not a table"),
            (
                "required = { prompt = 1 }",
                "argument prompt: describe it as a string",
            ),
            (
                "optional = { after = false }",
                "argument after: describe it as a string",
            ),
        ] {
            let (found, skipped) = discover_declarations(declaration);
            assert!(found.is_empty());
            assert_eq!(skipped.len(), 2);
            for skip in skipped {
                assert!(skip.why.contains(message), "{skip}");
            }
        }
    }

    #[test]
    fn the_frontmatter_declares_description_and_args() {
        let text = "#!/usr/bin/env cargo\n---\n[package]\ndescription = \"Count lines\"\n\n\
                    [package.metadata.pi.required]\nfile = \"file to count\"\n\n\
                    [dependencies]\nserde_json = \"1\"\n---\nfn main() {}\n";
        let (description, args, _) = interface(text).unwrap();
        assert_eq!(description, "Count lines");
        assert_eq!(
            args,
            vec![Arg {
                name: "file".into(),
                description: "file to count".into(),
                required: true,
            }]
        );
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
    fn a_script_is_rust_or_names_its_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
        write(
            "count.rs",
            "---\n[package]\ndescription = \"Count\"\n---\nfn main() {}\n",
        );
        write(
            "greet.sh",
            "#!/usr/bin/env bash\n# ---\n# description = \"Greet\"\n# [required]\n# who = \"whom\"\n# ---\necho hi\n",
        );
        write("notes.md", "# not a script\n");
        write("old.sh", "#!/usr/bin/env bash\n# description: x\n");
        let (found, skipped) = discover_in(dir.path());
        let names: Vec<_> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["count", "greet"]);
        assert_eq!(found[1].schema()["required"], serde_json::json!(["who"]));
        assert!(
            matches!(&found[1].runner, Runner::Shebang(p, Some(a)) if p == "/usr/bin/env" && a == "bash")
        );
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].why.contains("no `# ---` header"), "{skipped:?}");
    }

    // The shipped examples are what people copy; one that stopped parsing
    // would teach a script pi skips.
    #[test]
    fn the_shipped_example_tools_are_scripts_pi_accepts() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/tools");
        let (found, skipped) = discover_in(&dir);
        assert!(skipped.is_empty(), "{skipped:?}");
        let names: Vec<_> = found.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["choose", "judge", "search", "telegram", "wechat"]);
        let example = std::fs::read_to_string(dir.join(".env.example")).unwrap();
        let named: Vec<_> = env::parse(&example).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            named,
            ["TYPESAFE_API_KEY", "TELEGRAM_BOT_TOKEN"],
            "what judge and telegram read"
        );
    }

    // A script sees what `.env` sets, and an argument of the same name cannot
    // replace it: the model picks arguments, never the keys they go out with.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_script_starts_with_the_env_file_and_the_model_cannot_swap_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("show.sh"),
            "#!/bin/sh\n# ---\n# description = \"Show\"\n# [required]\n\
             # PI_TEST_KEY = \"a name the file sets\"\n# ---\necho \"$PI_TEST_KEY\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(env::FILE), "PI_TEST_KEY=from-file\n").unwrap();
        let (found, _) = discover_in(dir.path());
        let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        let out = found[0]
            .execute(serde_json::json!({ "PI_TEST_KEY": "from-model" }), &ctx)
            .await
            .unwrap()
            .flatten();
        assert_eq!(out.trim(), "from-file");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_omitted_optional_argument_stays_absent_on_stdin_and_in_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("show"),
            "#!/bin/sh\n# ---\n# description = \"Show\"\n# [optional]\n\
             # pi_test_optional_delay = \"A delay\"\n# ---\n\
             cat\nprintf '\\n%s' \"${pi_test_optional_delay-unset}\"\n",
        )
        .unwrap();
        let (found, skipped) = discover_in(dir.path());
        assert!(skipped.is_empty(), "{skipped:?}");
        let ctx = tool::Ctx::new(tool::Workspace::new(dir.path()).unwrap());
        for (args, value) in [
            (json!({}), "unset"),
            (json!({ "pi_test_optional_delay": "" }), ""),
            (json!({ "pi_test_optional_delay": "10m" }), "10m"),
        ] {
            let expected = format!("{args}\n{value}");
            let out = found[0].execute(args, &ctx).await.unwrap().flatten();
            assert_eq!(out, expected);
        }
    }

    #[test]
    fn a_header_reads_in_its_own_comment_style() {
        let js = "#!/usr/bin/env node\n\n// ---\n// description = \"Hi\"\n//\n// ---\n";
        assert_eq!(header(js).unwrap().0, "Hi");
        let open = "#!/bin/sh\n# ---\n# description = \"Hi\"\n";
        assert!(header(open).unwrap_err().contains("not closed"));
        let stray = "#!/bin/sh\n# ---\ndescription = \"Hi\"\n# ---\n";
        assert!(header(stray).unwrap_err().contains("not a comment"));
        let typo =
            "#!/bin/sh\n# ---\n# description = \"Hi\"\n# [requried]\n# who = \"whom\"\n# ---\n";
        assert!(header(typo).unwrap_err().contains("unknown key `requried`"));
    }
}
