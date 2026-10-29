//! What the config and the workspace decide for one checkout: the tools, the
//! prompt, the ceiling, the commands. Startup, a reload and every new lane
//! come through here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use llm::request::Effort;
use tool::stamp::Stamp;

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
    /// The config's hooks, which the brief carries once armed: before then
    /// its hooks can ask no one.
    pub hooks: crate::core::hooks::Declared,
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
    /// The built-ins and the surface's words, plus one command per skill.
    /// Here rather than in the Core because a skill discovered at reload has
    /// to reach the prompt the same way everything else the config decides does.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// The config files this was built from, as they stood: when one moves,
    /// the lane is resolved again. Skills and scripts are read live, and the
    /// prompt's files are `prompt_files`, so neither is here.
    pub watched: Vec<Stamp>,
    /// The script directories `brief` reads, for a lane to swap for ones whose
    /// scripts may detach: the subagent keeps these.
    pub scripts: Vec<Arc<toolbox::scripts::Dir>>,
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
    /// Whether the run reads memory, and so may write it.
    pub remembers: bool,
    /// The file that replaced the built-in system prompt, if one did.
    pub system: Option<PathBuf>,
    /// The files that replaced other built-in prompts.
    pub prompts: Vec<PathBuf>,
    /// Where requests go and which source said so, for `/status`: two files
    /// can each name one now.
    pub endpoint: Option<String>,
    /// The MCP servers the config names, shown with what the run stands on.
    pub mcp: Vec<String>,
    /// What the checkout sets that reaches past it, as key and value — its
    /// project file's keys and its own scripts — said at startup: nothing refuses it.
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
    prompts: Vec<(PathBuf, String)>,
    instructions: Vec<(PathBuf, String)>,
    memory: Vec<(String, String)>,
    // Memory files past the budget, by name: not in the prompt.
    left_out: Vec<String>,
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
        let prompts = replacements()
            .map(|(path, _)| path)
            .filter(|path| path.is_file())
            .map(|path| {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read prompt {}", path.display()))?;
                Ok((path, text))
            })
            .collect::<Result<_>>()?;
        let (instructions, (memory, left_out)) = if pinned.no_context_files {
            (Vec::new(), (Vec::new(), Vec::new()))
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
            prompts,
            instructions,
            memory,
            left_out,
            day: day.to_string(),
        })
    }

    /// The text the built-in prompt `name` runs under: its replacement's, or
    /// its own.
    pub fn prompt(&self, name: &str) -> &str {
        let path = config::prompt_file(name);
        self.prompts
            .iter()
            .find(|(p, _)| Some(p) == path.as_ref())
            .map(|(_, text)| text.as_str())
            .or_else(|| {
                crate::core::prompt::REPLACEABLE
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, text)| *text)
            })
            .unwrap_or_default()
    }

    /// The files that read differently in `now`, named as a person would.
    pub fn moved(&self, now: &Self, root: &Path) -> Vec<String> {
        let mut moved = Vec::new();
        if self.system != now.system {
            let path = now.system_file.as_ref().or(self.system_file.as_ref());
            moved.push(path.map_or("system prompt".into(), |p| instructions::short(p, root)));
        }
        moved.extend(
            changed(&self.prompts, &now.prompts)
                .chain(changed(&self.instructions, &now.instructions))
                .map(|p| instructions::short(p, root)),
        );
        moved.extend(changed(&self.memory, &now.memory).map(|name| memory_path(name, root)));
        moved
    }

    /// What to tell a person of the memory past the budget, if any is: a
    /// fact the model stops seeing would otherwise look forgotten.
    pub fn left_out(&self, root: &Path) -> Option<String> {
        let names: Vec<String> = self.left_out.iter().map(|n| memory_path(n, root)).collect();
        (!names.is_empty()).then(|| {
            format!(
                "memory left out — over the prompt's budget: {}",
                names.join(", ")
            )
        })
    }
}

// A memory file named as a person would, from `root`.
fn memory_path(name: &str, root: &Path) -> String {
    let main = crate::core::worktree::main_root(root);
    instructions::short(
        &pi_store::memory::Memory::default().path_of(name, &main),
        root,
    )
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

/// `workspace` held to the boundary the run's ceiling calls for. A run with a
/// shell reaches everywhere through it, so a fence would only make write and
/// edit refuse what bash then does without their checks.
pub fn fence(
    workspace: tool::Workspace,
    config: &config::Config,
    pinned: &Pinned,
) -> std::io::Result<tool::Workspace> {
    let settled = config.settle(config::Flags {
        effort: pinned.effort,
        tier: pinned.tier,
    });
    match tool::Tier::from(settled.tier) {
        tool::Tier::Exec => Ok(workspace.unfenced()),
        _ => workspace.with_write_roots(&config.writable()),
    }
}

/// Fails whole or not at all. A half-applied config is worse than a stale one,
/// which is why a reload computes all of this before touching anything.
///
/// `prompt_files` is the prompt's files as last read; `None` reads them now.
/// `words` are the surface's own, listed with the built-ins.
pub fn resolve(
    pinned: &Pinned,
    workspace: &tool::Workspace,
    config: &config::Config,
    settings: &Settings,
    prompt_files: Option<Arc<PromptFiles>>,
    words: &[Command],
) -> Result<Resolved> {
    let root = workspace.root();
    let mut notes = Vec::new();
    // Before any of it is read, so a write landing meanwhile reads as a change.
    let read_from = watched(pinned, root);

    // Sources offer their tools in order, built-ins first.
    let mut registry = toolbox::builtin();
    // Whether two directories are the one directory, seen from different
    // routes: a home that is the checkout's own `.pi` would otherwise be read
    // twice and every skill and script in it reported as colliding with itself.
    let same = |a: &Path, b: &Path| {
        a.canonicalize()
            .ok()
            .is_some_and(|a| b.canonicalize().ok() == Some(a))
    };
    // Read live, like the scripts: a skill written while pi runs can be loaded
    // on the next turn, and typed once `Core::refresh_skills` has seen it.
    let home_skills = pi_store::dir().map(|pi| pi.join("skills"));
    let project_skills = config::project_dir(root, "skills")
        .filter(|dir| !home_skills.as_deref().is_some_and(|home| same(dir, home)));
    let skill_dirs: Vec<PathBuf> = project_first(project_skills.as_ref(), home_skills)
        .into_iter()
        .map(|(dir, _)| dir)
        .collect();
    let shelf = (!pinned.no_skills).then(|| {
        let shelf = skills::Shelf::new(skill_dirs);
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
    // Named on the banner the way a project's own scripts are.
    let mut project_skill_names = Vec::new();
    if let Some(dir) = &project_skills {
        project_skill_names.extend(
            found
                .skills
                .iter()
                .filter(|s| s.dir.starts_with(dir))
                .map(|s| s.name.clone()),
        );
    }
    project_skill_names.sort();
    // A skill is two things at once, a command the user can type and a body
    // the model can load, and both read the same list.
    let commands = commands(&found.skills, words, &mut notes);
    if let Some(shelf) = &shelf {
        if registry.get(skills::Load::NAME).is_some() {
            notes.push("tool skipped — skill: the name is taken".to_string());
        }
        registry.read(shelf.clone());
    }

    // Read live: a script written while pi runs is a tool from the next turn,
    // the checkout's directory too, though it is not there yet.
    let home_tools = pi_store::dir().map(|root| root.join("tools"));
    let project_tools = config::project_dir(root, "tools")
        .filter(|dir| !home_tools.as_deref().is_some_and(|home| same(dir, home)));
    let script_dirs: Vec<(Arc<toolbox::scripts::Dir>, bool)> =
        project_first(project_tools.as_ref(), home_tools)
            .into_iter()
            .map(|(dir, project)| {
                notes.extend(toolbox::scripts::env::loose(&dir));
                (Arc::new(toolbox::scripts::Dir::new(dir)), project)
            })
            .collect();
    let mut project_scripts = Vec::new();
    let mut named = std::collections::BTreeSet::new();
    for (scripts, project) in &script_dirs {
        notes.extend(
            scripts
                .skipped()
                .iter()
                .map(|p| format!("tool skipped — {p}")),
        );
        let whose = if *project { "project" } else { "user" };
        // Offered tools win a name; between directories, the one read earlier.
        // A tool hung on after this (`bash`, `subagent`, `memory`) asks the
        // same question later and stands down instead of taking the name.
        for script in tool::Source::tools(&**scripts) {
            let name = script.name();
            let earlier = !named.insert(name.to_string());
            if registry.get(name).is_some() || earlier {
                notes.push(format!(
                    "tool skipped — {whose} script {name}: the name is taken"
                ));
            } else if *project {
                project_scripts.push(name.to_string());
            }
        }
    }
    let scripts: Vec<_> = script_dirs.into_iter().map(|(dir, _)| dir).collect();
    for dir in &scripts {
        registry.read(dir.clone());
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
    let memory_files = (prompt_files.memory.iter())
        .map(|(name, _)| memory_path(name, root))
        .collect();
    notes.extend(prompt_files.left_out(root));
    // Appended rather than sent as a message: standing instructions don't
    // change within a run, and the system prompt is what a provider caches.
    let standing = crate::core::prompt::Standing {
        workspace: root.to_path_buf(),
        project_tools,
        fenced: workspace.fenced(),
        pi_home,
        instructions: prompt_files.instructions.clone(),
        memory: prompt_files.memory.clone(),
        day: prompt_files.day.clone(),
        tier,
    }
    .render();
    system.push_str(&standing);

    let hooks = crate::core::hooks::Declared::new(&config.hooks, workspace);
    Ok(Resolved {
        brief: std::sync::Arc::new(agent::Briefing {
            registry: registry.within(tier),
            system,
            summarize: prompt_files.prompt("summarize").to_string(),
            effort,
            approver: std::sync::Arc::new(agent::Ceiling(tier)),
            hooks: hooks.arm(std::sync::Arc::new(toolbox::background::NoOne)),
        }),
        hooks,
        subagent_deadline: config
            .subagent_deadline
            .map(|s| std::time::Duration::from_secs(s.max(1))),
        standing: standing.into(),
        ceiling: tier,
        commands: std::sync::Arc::new(commands),
        watched: read_from,
        scripts,
        shelf,
        shelf_seen,
        notes,
        instructions: instruction_names,
        memory: memory_files,
        remembers: !pinned.no_context_files,
        system: prompt_files.system_file.clone(),
        prompts: prompt_files
            .prompts
            .iter()
            .map(|(p, _)| p.clone())
            .collect(),
        prompt_files,
        endpoint: endpoint(pinned, config, settings, root),
        mcp: mcp_names(config, settings),
        project: settings
            .project_reaching()
            .map(|(_, keys)| keys)
            .unwrap_or_default()
            .into_iter()
            .chain(
                (!project_scripts.is_empty()).then(|| ("tools".into(), project_scripts.join(", "))),
            )
            .chain(
                (!project_skill_names.is_empty())
                    .then(|| ("skills".into(), project_skill_names.join(", "))),
            )
            .collect(),
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
    "---\nname: pi-help\ndescription: Read pi's README for its configuration, tools, sessions, and limits. \
     Use this skill when asked how to use pi or how a pi feature works.\n---\n\n",
    include_str!("../../../../README.md"),
);

/// The checkout's directories before the home's, so a name both use is the
/// project's — skill and script alike. The reason to keep something in a
/// checkout is that it belongs to that tree, and what is general must not
/// shadow it.
fn project_first(project: Option<&PathBuf>, home: Option<PathBuf>) -> Vec<(PathBuf, bool)> {
    let project = project.map(|dir| (dir.clone(), true));
    project
        .into_iter()
        .chain(home.map(|dir| (dir, false)))
        .collect()
}

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
    paths.into_iter().filter_map(tool::stamp::of).collect()
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
    shared.extend(replacements().map(|(p, _)| (p, "replaces a built-in prompt")));
    files.extend(shared);
    files
        .into_iter()
        .map(|(path, what)| {
            let note = match std::fs::metadata(&path) {
                Ok(meta) => format!("{what} · {}", pi_store::text::size(meta.len())),
                Err(_) if builtin_of(&path).is_some() => {
                    format!("{what} · new, opens on the built-in")
                }
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

// Where each replaceable prompt's file would be, with the text it replaces.
fn replacements() -> impl Iterator<Item = (PathBuf, &'static str)> {
    crate::core::prompt::REPLACEABLE
        .iter()
        .filter_map(|(name, text)| Some((config::prompt_file(name)?, *text)))
}

// The built-in text the file at `path` replaces, when it replaces one. A
// `--system` file is not one: the flag needs it there, so it is never removed.
fn builtin_of(path: &Path) -> Option<&'static str> {
    std::iter::once((config::system_file(), crate::core::prompt::DEFAULT_SYSTEM))
        .filter_map(|(p, text)| Some((p?, text)))
        .chain(replacements())
        .find(|(p, _)| p == path)
        .map(|(_, text)| text)
}

/// Before `/edit` opens a replacement not written yet: start it as the
/// built-in reads, so the change is made to that rather than to nothing.
pub fn seed(path: &Path) -> std::io::Result<()> {
    match builtin_of(path) {
        Some(text) if !path.exists() => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, text)
        }
        _ => Ok(()),
    }
}

/// After the editor: a replacement still reading as the built-in replaces
/// nothing, and kept, it would hold the built-in back at this version.
/// True when it went.
pub fn unseed(path: &Path) -> bool {
    builtin_of(path).is_some_and(|text| {
        std::fs::read_to_string(path).is_ok_and(|now| now == text)
            && std::fs::remove_file(path).is_ok()
    })
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
    use std::path::PathBuf;

    #[test]
    fn the_prompt_names_the_project_directory_used_for_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let root_tools = repo.join(".pi/tools");
        let nested = repo.join("packages/app");
        let near_tools = nested.join(".pi/tools");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        let workspace = tool::Workspace::new(&nested).unwrap().unfenced();
        let pinned = crate::args::Pinned {
            no_skills: true,
            no_context_files: true,
            tier: Some(pi_store::args::TierArg::Exec),
            ..Default::default()
        };
        let settings = pi_store::settings::Settings::new(toml::Table::new().into(), None);
        let files = std::sync::Arc::new(super::PromptFiles {
            system: "A custom system prompt.".into(),
            ..Default::default()
        });
        for (dir, create) in [
            (&root_tools, false),
            (&root_tools, true),
            (&near_tools, true),
        ] {
            if create {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::write(
                    dir.join("project_tool.sh"),
                    "#!/bin/sh\n# ---\n# description = 'A project tool.'\n# ---\necho ok\n",
                )
                .unwrap();
            }
            let resolved = super::resolve(
                &pinned,
                &workspace,
                &Default::default(),
                &settings,
                Some(files.clone()),
                &[],
            )
            .unwrap();
            let block = format!("<project_tools path=\"{}\">", dir.display());
            assert!(resolved.standing.contains(&block), "{}", resolved.standing);
            assert!(resolved.brief.system.contains(&block));
            assert_eq!(
                resolved.brief.registry.get("project_tool").is_some(),
                create
            );
            assert_eq!(dir.exists(), create);
        }
    }

    // A checkout's skill or script of a name an already-known one holds must
    // be the one offered: the reason to put one in the project is that it
    // belongs to that tree.
    #[test]
    fn a_project_directory_is_read_before_the_home_one() {
        let project = PathBuf::from("/repo/.pi/skills");
        let home = Some(PathBuf::from("/home/u/.pi/skills"));
        assert_eq!(
            super::project_first(Some(&project), home.clone()),
            [
                (PathBuf::from("/repo/.pi/skills"), true),
                (PathBuf::from("/home/u/.pi/skills"), false)
            ]
        );
        assert_eq!(
            super::project_first(None, home),
            [(PathBuf::from("/home/u/.pi/skills"), false)],
            "no project directory, the home alone"
        );
    }

    // A name the table and a lookup spell apart would run that part of pi
    // on an empty prompt, and nothing on screen would say so.
    #[test]
    fn every_replaceable_prompt_falls_back_to_its_built_in() {
        let files = super::PromptFiles::default();
        for (name, text) in crate::core::prompt::REPLACEABLE {
            assert!(!text.trim().is_empty(), "{name}");
            assert_eq!(files.prompt(name), text, "{name}");
        }
    }

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
        let said: Vec<String> = said.iter().map(|t| pi_store::text::collapsed(t)).collect();
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
                toolbox::bash::Bash::NAME,
                skills::Load::NAME,
                subagent::Subagent::NAME,
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
