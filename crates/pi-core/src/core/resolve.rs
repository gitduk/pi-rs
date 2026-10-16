//! What the config and the workspace decide for one checkout: the tools, the
//! prompt, the ceiling, the commands. Startup, a reload and every new lane
//! come through here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use llm::request::Effort;

use crate::args::Pinned;
use crate::core::instructions;
use crate::input::commands::{Command, commands};
use pi_store::args::EffortArg;
use pi_store::settings::Settings;
use pi_store::{config, journal};

/// Everything the config and the workspace decide, as opposed to what the
/// command line fixed for the whole run. A reload recomputes exactly this.
#[derive(Clone)]
pub struct Resolved {
    /// What the agent runs on: the tools, the prompt, the ceiling, the effort.
    /// A subagent derives its own from this one.
    pub brief: std::sync::Arc<agent::Briefing>,
    /// The tail of the prompt that belongs to the run rather than to the
    /// assistant: the workspace anchor, what the run is, and the
    /// instructions files. Kept apart because the subagent has its own prompt but the same
    /// tree, the same machine and the same tier.
    pub standing: std::sync::Arc<str>,
    /// How long a subagent may run silent before it is read as wedged; `None`
    /// leaves the subagent's own default.
    pub subagent_deadline: Option<std::time::Duration>,
    /// How far this run may reach. The lane needs it to decide whether the
    /// subagent, offered after this set is cut, belongs in it.
    pub ceiling: tool::Tier,
    /// The key table this tree asked for, defaults included.
    pub keys: std::sync::Arc<pi_store::keys::Keys>,
    /// The built-ins plus one command per skill. Here rather than in the Core
    /// because a skill discovered at reload has to reach the prompt the same
    /// way everything else the config decides does.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// The config files this was built from, as they stood: when one moves,
    /// the lane is resolved again. Skills and scripts are read live, and the
    /// prompt's files are `prompt_files`, so neither is here.
    pub watched: Vec<Stamp>,
    /// What the prompt was told from files, held through every reload.
    pub prompt_files: Arc<PromptFiles>,
    /// Where skills are read from, live; `None` under `--no-skills`.
    pub shelf: Option<std::sync::Arc<skills::Shelf>>,
    /// Which state of the shelf `commands` was built from.
    pub shelf_seen: u64,
    /// Worth saying once, at startup and at each reload.
    pub notes: Vec<String>,
    /// The instructions files folded into the system prompt, named as a person
    /// would. Shown with what the run stands on rather than said as a note:
    /// it is not news.
    pub instructions: Vec<String>,
    /// The memory files folded into the prompt, by path.
    pub memory: Vec<String>,
    /// The file that replaced the built-in system prompt, if one did.
    pub system: Option<PathBuf>,
    /// Where requests go and which source said so, for `/status`: two files
    /// can each name one now.
    pub endpoint: Option<String>,
    /// The MCP servers the config names, shown with what the run stands on.
    pub mcp: Vec<String>,
    /// What the project file sets that reaches past its checkout, as key and
    /// value, said at startup: nothing refuses it.
    pub project: Vec<(String, String)>,
}

/// What the prompt carries from files — the system prompt, the instructions,
/// memory — and the day. Read for a new session and after a compaction, held
/// through reloads between: a prompt that moves mid-session contradicts what
/// the model already read, and throws away the cached prefix.
#[derive(Clone, Default, PartialEq)]
pub struct PromptFiles {
    system_file: Option<PathBuf>,
    system: String,
    instructions: Vec<(PathBuf, String)>,
    memory: Vec<(String, String)>,
    day: String,
}

impl PromptFiles {
    /// The files as they stand now in `root`.
    pub fn read(pinned: &Pinned, root: &Path) -> Result<Self> {
        // The flag's file has to be there; `SYSTEM.md` only when it is.
        let system_file =
            system_file(pinned).filter(|path| pinned.system.is_some() || path.is_file());
        let system = match &system_file {
            Some(path) => std::fs::read_to_string(path)
                .with_context(|| format!("cannot read system prompt {}", path.display()))?,
            None => crate::core::prompt::DEFAULT_SYSTEM.to_string(),
        };
        let (instructions, memory) = if pinned.no_context_files {
            (Vec::new(), Vec::new())
        } else {
            (
                instructions::load(root, pi_store::dir().as_deref()).files,
                crate::core::memory::kept(&pi_store::memory::Memory::default(), root),
            )
        };
        let stamp = journal::rfc3339(std::time::SystemTime::now());
        let day = stamp.split_once('T').map_or(&*stamp, |(day, _)| day);
        Ok(Self {
            system_file,
            system,
            instructions,
            memory,
            day: day.to_string(),
        })
    }

    /// The files that read differently in `now`, named as a person would.
    pub fn moved(&self, now: &Self, root: &Path) -> Vec<String> {
        let mut moved = Vec::new();
        if self.system != now.system {
            let path = now.system_file.as_ref().or(self.system_file.as_ref());
            moved.push(path.map_or("system prompt".into(), |p| instructions::short(p, root)));
        }
        moved.extend(
            changed(&self.instructions, &now.instructions).map(|p| instructions::short(p, root)),
        );
        let memory_dir = pi_store::memory::Memory::default();
        let main = crate::core::worktree::main_root(root);
        moved.extend(
            changed(&self.memory, &now.memory)
                .map(|name| instructions::short(&memory_dir.path_of(name, &main), root)),
        );
        moved
    }
}

// The files that differ between `was` and `now`, added and removed ones included.
fn changed<'a, K: PartialEq>(
    was: &'a [(K, String)],
    now: &'a [(K, String)],
) -> impl Iterator<Item = &'a K> {
    let gone = was.iter().filter(|w| !now.iter().any(|n| n.0 == w.0));
    now.iter()
        .filter(|n| !was.contains(n))
        .chain(gone)
        .map(|f| &f.0)
}

/// Fails whole or not at all. A half-applied config is worse than a stale one,
/// which is why a reload computes all of this before touching anything.
///
/// `prompt_files` is the prompt's files as last read; `None` reads them now.
pub fn resolve(
    pinned: &Pinned,
    workspace: &tool::Workspace,
    config: &config::Config,
    settings: &Settings,
    prompt_files: Option<Arc<PromptFiles>>,
) -> Result<Resolved> {
    let root = workspace.root();
    let mut notes = Vec::new();
    // Before any of it is read, so a write landing meanwhile reads as a change.
    let read_from = watched(pinned, root);

    // Sources offer their tools in order, built-ins first.
    let mut registry = toolbox::builtin();
    // Read live, like the scripts: a skill written while pi runs can be loaded
    // on the next turn, and typed once `Core::refresh_skills` has seen it.
    let shelf = pi_store::dir().filter(|_| !pinned.no_skills).map(|pi| {
        let shelf = skills::Shelf::new(pi.join("skills"));
        let builtins = [toolbox::scripts::SKILL, PI_SKILL];
        Arc::new(
            builtins
                .into_iter()
                .filter_map(skills::Skill::builtin)
                .fold(shelf, skills::Shelf::with_builtin),
        )
    });
    let (found, shelf_seen) = shelf.as_ref().map(|s| s.now()).unwrap_or_default();
    // A skill that silently fails to appear is one the user goes looking
    // for in the wrong place.
    notes.extend(
        found
            .problems
            .iter()
            .map(|p| format!("skill skipped — {p}")),
    );
    // A skill is two things at once, a command the user can type and a body
    // the model can load, and both read the same list.
    let commands = commands(&found.skills, &mut notes);
    if let Some(shelf) = &shelf {
        if registry.get(skills::Load::NAME).is_some() {
            notes.push("tool skipped — skill: the name is taken".to_string());
        }
        registry.read(shelf.clone());
    }

    // Read live: a script written while pi runs is a tool from the next turn.
    let scripts =
        pi_store::dir().map(|root| Arc::new(toolbox::scripts::Dir::new(root.join("tools"))));
    if let Some(scripts) = &scripts {
        notes.extend(
            pi_store::dir().and_then(|root| toolbox::scripts::env::loose(&root.join("tools"))),
        );
        notes.extend(
            scripts
                .skipped()
                .iter()
                .map(|p| format!("tool skipped — {p}")),
        );
        // Offered tools win a name, the subagent among them though it is
        // offered later, per lane.
        for script in tool::Source::tools(&**scripts) {
            if registry.get(script.name()).is_some() || script.name() == subagent::Subagent::NAME {
                notes.push(format!(
                    "tool skipped — user script {}: the name is taken",
                    script.name()
                ));
            }
        }
    }
    // Offered per lane after all of the above, so a holder of its name is
    // known here: say so once, not on every re-arm.
    if registry.get(subagent::Subagent::NAME).is_some() {
        notes.push("tool skipped — subagent: the name is taken".to_string());
    }
    if let Some(scripts) = scripts {
        registry.read(scripts);
    }
    // Read live like the scripts: a server's tools join once it has listed them.
    if !config.mcp.is_empty() {
        registry.read(Arc::new(crate::core::mcp::Live));
    }

    let settled = config.settle(config::Flags {
        effort: pinned.effort,
        tier: pinned.tier,
    });
    let tier = tool::Tier::from(settled.tier);
    let effort = match settled.effort {
        // `unsupported` is refused anywhere but a model's entry.
        EffortArg::Unsupported | EffortArg::Off => Effort::Off,
        EffortArg::Low => Effort::Low,
        EffortArg::Medium => Effort::Medium,
        EffortArg::High => Effort::High,
    };

    let prompt_files = match prompt_files {
        Some(files) => files,
        None => Arc::new(PromptFiles::read(pinned, root)?),
    };
    let mut system = prompt_files.system.clone();
    // Everything the run needs to know about pi rides here rather than in the
    // system prompt, so that a replaced or empty one loses none of it.
    // Said only where `pi-extend` is offered and the home may be written; the
    // write tool's own check, so a symlinked home is judged as it is.
    let pi_home = pi_store::dir().filter(|home| {
        found.skills.iter().any(|s| s.name == "pi-extend")
            && tool::Tier::Write.under(tier)
            && home
                .to_str()
                .is_some_and(|h| workspace.resolve(h, tool::Tier::Write).is_ok())
    });
    let instruction_names = prompt_files
        .instructions
        .iter()
        .map(|(p, _)| instructions::short(p, root))
        .collect();
    let memory_dir = pi_store::memory::Memory::default();
    let main = crate::core::worktree::main_root(root);
    let memory_files = prompt_files
        .memory
        .iter()
        .map(|(name, _)| instructions::short(&memory_dir.path_of(name, &main), root))
        .collect();
    // Appended rather than sent as a message: standing instructions don't
    // change within a run, and the system prompt is what a provider caches.
    let standing = crate::core::prompt::Standing {
        workspace: root.to_path_buf(),
        write_paths: workspace.write_roots().to_vec(),
        pi_home,
        instructions: prompt_files.instructions.clone(),
        memory: prompt_files.memory.clone(),
        day: prompt_files.day.clone(),
        tier,
    }
    .render();
    system.push_str(&standing);

    Ok(Resolved {
        brief: std::sync::Arc::new(agent::Briefing {
            registry: registry.within(tier),
            system,
            effort,
            approver: std::sync::Arc::new(agent::Ceiling(tier)),
            hooks: crate::core::hooks::of(&config.hooks),
        }),
        subagent_deadline: config
            .subagent_deadline
            .map(|s| std::time::Duration::from_secs(s.max(1))),
        standing: standing.into(),
        ceiling: tier,
        keys: std::sync::Arc::new(config.key_map()?),
        commands: std::sync::Arc::new(commands),
        watched: read_from,
        shelf,
        shelf_seen,
        notes,
        instructions: instruction_names,
        memory: memory_files,
        system: prompt_files.system_file.clone(),
        prompt_files,
        endpoint: endpoint(pinned, config, settings, root),
        mcp: mcp_names(config, settings),
        project: settings
            .project_reaching()
            .map(|(_, keys)| keys)
            .unwrap_or_default(),
    })
}

// The file that replaces the built-in system prompt: the flag's, else
// `SYSTEM.md` in pi's home, whether or not that one exists yet.
fn system_file(pinned: &Pinned) -> Option<PathBuf> {
    pinned
        .system
        .as_ref()
        .map(PathBuf::from)
        .or_else(config::system_file)
}

// pi as its README tells it, for when the user asks about pi itself: one
// source, so a feature written up there is known here.
const PI_SKILL: &str = concat!(
    "---\nname: pi-help\ndescription: How pi itself works — its commands, keys, config, \
     tools, sessions and limits (the README). Use when asked how to do something in pi, or \
     what a pi command, setting or feature does; /help only lists the commands.\n---\n\n",
    include_str!("../../../../README.md"),
);

/// A file as last seen: its path, time and size.
pub type Stamp = (PathBuf, Option<std::time::SystemTime>, u64);

/// The config files a resolve in `root` reads, as they stand now. Missing
/// ones are left out, so one appearing is a change too.
pub fn watched(pinned: &Pinned, root: &Path) -> Vec<Stamp> {
    let mut paths: Vec<PathBuf> = Vec::new();
    paths.extend(
        pinned
            .config
            .as_ref()
            .map(PathBuf::from)
            .or_else(config::global_path),
    );
    paths.extend(config::project_file(root));
    paths
        .into_iter()
        .filter_map(|path| {
            let meta = std::fs::metadata(&path).ok()?;
            Some((path, meta.modified().ok(), meta.len()))
        })
        .collect()
}

/// A file `/edit` offers: where it is, the name it is offered under, and
/// what it is to the model.
pub struct Editable {
    pub path: PathBuf,
    pub name: String,
    pub note: String,
}

impl From<Editable> for crate::input::commands::Choice {
    fn from(e: Editable) -> Self {
        Self {
            name: e.name,
            note: e.note,
        }
    }
}

/// What the model is given from files in `root`, to edit: the system prompt
/// and this checkout's `AGENTS.md` even before they exist, since saving one
/// is how it starts; every other instructions file and memory as they stand.
/// This checkout's own files come first, then those every checkout shares.
pub fn editable(pinned: &Pinned, root: &Path) -> Vec<Editable> {
    let mut files: Vec<(PathBuf, &str)> = Vec::new();
    let mut shared: Vec<(PathBuf, &str)> = Vec::new();
    if !pinned.no_context_files {
        let pi = pi_store::dir();
        let personal = pi.as_ref().map(|d| d.join("AGENTS.md"));
        let here = root.join("AGENTS.md");
        files.push((here.clone(), "instructions"));
        // Innermost first: the nearer a file, the likelier it is the one edited.
        for path in instructions::paths(root, instructions::home().as_deref(), pi.as_deref())
            .into_iter()
            .rev()
            .filter(|p| *p != here)
        {
            let to = if personal.as_ref() == Some(&path) {
                &mut shared
            } else {
                &mut files
            };
            to.push((path, "instructions"));
        }
        let memory = pi_store::memory::Memory::default();
        let main = crate::core::worktree::main_root(root);
        let project = memory.project_path(&main);
        for path in memory.paths(&main).into_iter().filter(|p| p.is_file()) {
            let to = if path == project {
                &mut files
            } else {
                &mut shared
            };
            to.push((path, "memory"));
        }
    }
    shared.extend(system_file(pinned).map(|p| (p, "replaces the system prompt")));
    files.extend(shared);
    files
        .into_iter()
        .map(|(path, what)| {
            let note = match std::fs::metadata(&path) {
                Ok(meta) => format!("{what} · {}", pi_store::text::size(meta.len())),
                Err(_) => format!("{what} · new, saving creates it"),
            };
            Editable {
                name: instructions::short(&path, root),
                path,
                note,
            }
        })
        .collect()
}

// The servers in force the project file does not name: those it does are
// said with the rest of what it sets.
fn mcp_names(config: &config::Config, settings: &Settings) -> Vec<String> {
    let project = settings
        .project_tree()
        .and_then(|(_, tree)| tree.get("mcp")?.as_table());
    config
        .mcp
        .keys()
        .filter(|name| !project.is_some_and(|names| names.contains_key(*name)))
        .cloned()
        .collect()
}

// The url requests go to, and the one source it came from, in the order
// they rank: the flag, the environment, the files.
fn endpoint(
    pinned: &Pinned,
    config: &config::Config,
    settings: &Settings,
    root: &Path,
) -> Option<String> {
    let (url, from) = match &pinned.base_url {
        Some(url) => (config::expand_base_url(url), "--base-url".to_string()),
        None => {
            let url = config.base_url.clone()?;
            let from = if let Some(var) = config::endpoint_env() {
                format!("${var}")
            } else if let Some(file) = settings.project_sets("base_url") {
                instructions::short(file, root)
            } else {
                match &pinned.config {
                    Some(file) => file.clone(),
                    None => config::global_path()
                        .map_or_else(|| "settings.toml".into(), |p| instructions::short(&p, root)),
                }
            };
            (url, from)
        }
    };
    Some(format!("endpoint: {url} ({from})"))
}

#[cfg(test)]
mod tests {
    // A built-in skill is dropped without a word when its header does not parse.
    #[test]
    fn pi_extend_is_a_builtin_skill_and_its_skill_example_parses() {
        let extend = skills::Skill::builtin(toolbox::scripts::SKILL).expect("pi-extend parses");
        assert_eq!(extend.name, "pi-extend");
        let example = toolbox::scripts::SKILL
            .split("```markdown\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("a skill example");
        let (_, description) = skills::frontmatter(example).unwrap();
        assert!(description.is_some_and(|d| !d.is_empty()));
    }

    #[test]
    fn pi_help_is_a_builtin_skill() {
        let help = skills::Skill::builtin(super::PI_SKILL).expect("pi-help parses");
        assert_eq!(help.name, "pi-help");
    }

    // `pi-help` answers from the README, so what the code has and the README
    // leaves out is something the model will deny exists.
    fn missing(
        names: impl IntoIterator<Item = String>,
        said: &[&str],
        forms: &[&str],
    ) -> Vec<String> {
        // Aligned columns (`tier   = "exec"`) read as one space.
        let said: Vec<String> = said
            .iter()
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        names
            .into_iter()
            .filter(|name| {
                !said.iter().any(|text| {
                    forms
                        .iter()
                        .any(|form| text.contains(&form.replace("{}", name)))
                })
            })
            .collect()
    }

    #[test]
    fn the_readme_names_every_command() {
        let words = crate::input::commands::BUILTIN
            .iter()
            .map(|c| c.word.to_string());
        let gone = missing(words, &[super::PI_SKILL], &["`{}`", "`{} ", "`{}["]);
        assert!(gone.is_empty(), "README.md lacks {gone:?}");
    }

    #[test]
    fn the_readme_names_every_tool() {
        let mut names = toolbox::builtin().names();
        names.extend(
            [
                skills::Load::NAME,
                subagent::Subagent::NAME,
                crate::driver::jobs::Jobs::NAME,
            ]
            .map(String::from),
        );
        let gone = missing(names, &[super::PI_SKILL], &["`{}`"]);
        assert!(gone.is_empty(), "README.md lacks {gone:?}");
    }

    // Every top-level key, read off the refusal of one that is not: serde
    // lists the fields it expected, so a new one cannot be left off here.
    #[test]
    fn the_readme_or_the_example_names_every_setting() {
        let refused = toml::from_str::<pi_store::config::Config>("not_a_setting = 1")
            .expect_err("an unknown key is refused")
            .to_string();
        let keys: Vec<String> = refused
            .split_once("expected one of ")
            .expect("serde names the fields")
            .1
            .split(", ")
            .map(|k| k.trim().trim_matches('`').to_string())
            .collect();
        assert!(keys.len() > 10, "{refused}");
        let example = include_str!("../../../../examples/pi.toml");
        let gone = missing(keys, &[super::PI_SKILL, example], &["{} =", "[{}", "`{}`"]);
        assert!(
            gone.is_empty(),
            "README.md and examples/pi.toml lack {gone:?}"
        );
    }
}
