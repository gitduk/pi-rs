//! What the config and the workspace decide for one checkout: the tools, the
//! prompt, the ceiling, the commands. Startup, a reload and every new lane
//! come through here.

use std::borrow::Cow;
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
    /// Main system blocks in wire order, with sources, sizes and fingerprints.
    pub prompt_blocks: Vec<crate::core::prompt::Block>,
    /// The config's hooks, which the brief carries once armed: before then
    /// its hooks can ask no one.
    pub hooks: crate::core::hooks::Declared,
    /// The tail of the prompt a child run stands on: the workspace anchor,
    /// what the run is, and the instructions files. Rendered apart from the
    /// lane's own because the subagent has its own prompt and the same tree,
    /// machine and tier, but is addressed by neither of the tool-authoring
    /// blocks.
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
    /// The catalog snapshot used for this lane's model connection.
    pub catalog_generation: super::catalog::Generation,
    /// The script directories `brief` reads, for a lane to swap for ones whose
    /// background wrappers are armed: the subagent keeps these.
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
            None => crate::core::prompt::default_system(),
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
    pub fn prompt(&self, name: &str) -> Cow<'_, str> {
        let path = config::prompt_file(name);
        self.prompts
            .iter()
            .find(|(p, _)| Some(p) == path.as_ref())
            .map(|(_, text)| Cow::Borrowed(text.as_str()))
            .or_else(|| {
                crate::core::prompt::REPLACEABLE
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, text)| Cow::Owned(text()))
            })
            .unwrap_or_default()
    }

    fn system_blocks(&self) -> Vec<crate::core::prompt::Block> {
        use crate::core::prompt::{Block, DEFAULT_SYSTEM, default_system};
        match &self.system_file {
            Some(path) => vec![Block::new(
                "system",
                path.display().to_string(),
                &self.system,
            )],
            None if self.system == default_system() => vec![
                Block::new("role", "built-in system", DEFAULT_SYSTEM),
                Block::new(
                    "execution",
                    "built-in execution",
                    &format!("\n\n{}", agent::EXECUTION),
                ),
            ],
            None => vec![Block::new("system", "provided system", &self.system)],
        }
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
        let builtins = [ARCHITECTURE];
        Arc::new(
            builtins
                .into_iter()
                .filter_map(skills::Skill::builtin)
                .chain([tool_creator(), skill_creator(), pi_setup(), pi_usage()])
                .chain(pinned.skills.iter().cloned())
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
        // A tool offered after this (`bash`, `subagent`, `memory`) still keeps
        // its name, and says so where it is offered.
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
    // Said only where skills are offered and the home may be written; the
    // write tool's own check, so a symlinked home is judged as it is.
    let pi_home = pi_store::dir().filter(|home| {
        shelf.is_some()
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
    let facts = crate::core::prompt::Standing {
        workspace: root.to_path_buf(),
        // Only a run that may write is told to put tools there.
        project_tools: project_tools.filter(|_| tool::Tier::Write.under(tier)),
        fenced: workspace.fenced(),
        pi_home,
        instructions: prompt_files.instructions.clone(),
        memory: prompt_files.memory.clone(),
        day: prompt_files.day.clone(),
        tier,
    };
    let mut prompt_blocks = prompt_files.system_blocks();
    for (block, text) in facts.blocks() {
        prompt_blocks.push(block);
        system.push_str(&text);
    }
    // A child stands on the same tree, machine and tier, but not on the blocks
    // that address a run serving a user: nobody can ask it for a tool, and it
    // has no next turn for one to appear in.
    let for_child = crate::core::prompt::Standing {
        project_tools: None,
        pi_home: None,
        ..facts
    }
    .render();

    let hooks = crate::core::hooks::Declared::new(&config.hooks, workspace);
    Ok(Resolved {
        brief: std::sync::Arc::new(agent::Briefing {
            registry: registry.within(tier),
            system,
            summarize: prompt_files.prompt("summarize").into_owned(),
            effort,
            approver: std::sync::Arc::new(agent::Ceiling(tier)),
            hooks: hooks.arm(std::sync::Arc::new(toolbox::channel::NoOne)),
        }),
        prompt_blocks,
        hooks,
        subagent_deadline: config
            .subagent_deadline
            .map(|s| std::time::Duration::from_secs(s.max(1))),
        standing: for_child.into(),
        ceiling: tier,
        commands: std::sync::Arc::new(commands),
        watched: read_from,
        catalog_generation: Default::default(),
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
        endpoint: endpoint(pinned),
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

const TOOL_CREATOR: &str = include_str!("../../skills/tool-creator/SKILL.md");

// The parts few tools need stay out of the instructions until asked for.
// Both creators write text that a model reads, so they share one guide.
const WRITING: (&str, &str) = ("writing.md", include_str!("../../skills/writing.md"));

// A script's fd 3 protocol: what it may say and what pi writes back. A script
// author and a driver of one read the same page.
const EVENTS: (&str, &str) = ("events.md", include_str!("../../skills/events.md"));

const TOOL_CREATOR_FILES: [(&str, &str); 4] = [
    (
        "interface.md",
        include_str!("../../skills/tool-creator/interface.md"),
    ),
    (
        "background.md",
        include_str!("../../skills/tool-creator/background.md"),
    ),
    EVENTS,
    WRITING,
];

fn tool_creator() -> skills::Skill {
    skills::Skill::builtin(TOOL_CREATOR)
        .expect("a valid built-in skill")
        .with_files(
            TOOL_CREATOR_FILES
                .iter()
                .map(|(name, text)| (name.to_string(), *text))
                .collect(),
        )
}
const SKILL_CREATOR: &str = include_str!("../../skills/skill-creator/SKILL.md");

fn skill_creator() -> skills::Skill {
    skills::Skill::builtin(SKILL_CREATOR)
        .expect("a valid built-in skill")
        .with_files(vec![(WRITING.0.to_string(), WRITING.1)])
}
const PI_SETUP: &str = include_str!("../../skills/pi-setup/SKILL.md");

// The field reference travels with the skill: it is the file a user copies,
// so the model edits the shape that user will read.
const PI_SETUP_FILES: [(&str, &str); 3] = [
    ("settings.toml", pi_store::config::EXAMPLE_SETTINGS),
    (
        "references/provider.md",
        include_str!("../../skills/pi-setup/references/provider.md"),
    ),
    (
        "references/mcp.md",
        include_str!("../../skills/pi-setup/references/mcp.md"),
    ),
];

fn pi_setup() -> skills::Skill {
    skills::Skill::builtin(PI_SETUP)
        .expect("a valid built-in skill")
        .with_files(
            PI_SETUP_FILES
                .iter()
                .map(|(name, text)| (name.to_string(), *text))
                .collect(),
        )
}
const ARCHITECTURE: &str = include_str!("../../skills/architecture/SKILL.md");

// What a session, a tool, a script or a file does: the pages the README no
// longer carries, so the model can answer for them.
const PI_USAGE: &str = include_str!("../../skills/pi-usage/SKILL.md");
const PI_USAGE_FILES: [(&str, &str); 5] = [
    (
        "references/session.md",
        include_str!("../../skills/pi-usage/references/session.md"),
    ),
    (
        "references/tools.md",
        include_str!("../../skills/pi-usage/references/tools.md"),
    ),
    ("references/events.md", EVENTS.1),
    (
        "references/context.md",
        include_str!("../../skills/pi-usage/references/context.md"),
    ),
    (
        "references/files.md",
        include_str!("../../skills/pi-usage/references/files.md"),
    ),
];

fn pi_usage() -> skills::Skill {
    skills::Skill::builtin(PI_USAGE)
        .expect("a valid built-in skill")
        .with_files(
            PI_USAGE_FILES
                .iter()
                .map(|(name, text)| (name.to_string(), *text))
                .collect(),
        )
}

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
fn replacements() -> impl Iterator<Item = (PathBuf, String)> {
    crate::core::prompt::REPLACEABLE
        .iter()
        .filter_map(|(name, text)| Some((config::prompt_file(name)?, text())))
}

// The built-in text the file at `path` replaces, when it replaces one. A
// `--system` file is not one: the flag needs it there, so it is never removed.
fn builtin_of(path: &Path) -> Option<String> {
    std::iter::once((config::system_file(), crate::core::prompt::default_system()))
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

// An explicit URL applies to every provider; otherwise the active model's
// connection is shown by the caller that owns its resolved specification.
fn endpoint(pinned: &Pinned) -> Option<String> {
    let url = config::expand_base_url(pinned.base_url.as_deref()?);
    Some(format!("endpoint: {url} (--base-url)"))
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
            assert!(
                resolved.brief.system.contains(&block),
                "{}",
                resolved.brief.system
            );
            // Nobody can ask a child for a tool, so it is not told where to put one.
            assert!(!resolved.standing.contains("<project_tools"));
            assert!(!resolved.standing.contains("<pi_home"));
            assert_eq!(
                resolved.brief.registry.get("project_tool").is_some(),
                create
            );
            assert_eq!(dir.exists(), create);
        }
        let read_only = crate::args::Pinned {
            tier: Some(pi_store::args::TierArg::Read),
            ..pinned
        };
        let resolved = super::resolve(
            &read_only,
            &workspace,
            &Default::default(),
            &settings,
            Some(files),
            &[],
        )
        .unwrap();
        assert!(!resolved.brief.system.contains("<project_tools"));
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
            let text = text();
            assert!(!text.trim().is_empty(), "{name}");
            assert_eq!(files.prompt(name), text, "{name}");
        }
    }

    #[test]
    fn defaults_share_execution_rules_but_replacements_replace_them_whole() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tool::Workspace::new(dir.path()).unwrap();
        let settings = pi_store::settings::Settings::new(toml::Table::new().into(), None);
        let pinned = crate::args::Pinned {
            no_skills: true,
            no_context_files: true,
            tier: Some(pi_store::args::TierArg::Read),
            ..Default::default()
        };
        for custom in [None, Some("Only answer the question."), Some("")] {
            let files = std::sync::Arc::new(super::PromptFiles {
                system: custom.map_or_else(crate::core::prompt::default_system, str::to_string),
                system_file: custom.map(|_| dir.path().join("system.md")),
                day: "2026-11-01".into(),
                ..Default::default()
            });
            let resolved = super::resolve(
                &pinned,
                &workspace,
                &Default::default(),
                &settings,
                Some(files.clone()),
                &[],
            )
            .unwrap();
            assert!(resolved.brief.system.starts_with(&files.system));
            assert_eq!(
                resolved.brief.system.matches(agent::EXECUTION).count(),
                usize::from(custom.is_none())
            );
            assert!(resolved.brief.system.contains("<workspace path="));
            assert!(!resolved.brief.system.contains("<project_tools"));
            assert!(!resolved.brief.system.contains("`write` and `edit`"));
            assert_eq!(
                resolved
                    .prompt_blocks
                    .iter()
                    .map(|b| b.bytes)
                    .sum::<usize>(),
                resolved.brief.system.len(),
            );
            assert!(files.prompt("subagent").contains(agent::EXECUTION));
            assert!(!files.prompt("summarize").contains(agent::EXECUTION));
        }

        let child_path = pi_store::config::prompt_file("subagent").unwrap();
        for text in ["Only return the answer.", ""] {
            let files = super::PromptFiles {
                prompts: vec![(child_path.clone(), text.into())],
                ..Default::default()
            };
            assert_eq!(files.prompt("subagent"), text);
        }
    }

    // A built-in skill is dropped without a word when its header does not parse.
    #[test]
    fn the_skill_creators_and_connection_instructions_are_builtin_skills() {
        for (name, text) in [
            ("tool-creator", super::TOOL_CREATOR),
            ("skill-creator", super::SKILL_CREATOR),
            ("pi-setup", super::PI_SETUP),
            ("pi-usage", super::PI_USAGE),
            ("architecture", super::ARCHITECTURE),
        ] {
            let skill = skills::Skill::builtin(text).expect("a valid built-in skill");
            assert_eq!(skill.name, name);
        }
    }

    // A file the instructions name but the skill cannot serve fails only when the model asks for it.
    #[test]
    fn the_creators_serve_every_file_their_instructions_name() {
        for skill in [
            super::tool_creator(),
            super::skill_creator(),
            super::pi_setup(),
            super::pi_usage(),
        ] {
            let text = skill.text().unwrap();
            // A reference is a `.md` file the instructions list, or a name they
            // ask for with `file:`. Files an example tells the model to create
            // are neither.
            let listed = text
                .split('`')
                .skip(1)
                .step_by(2)
                .filter(|word| word.ends_with(".md") && *word != "SKILL.md");
            let asked = text.match_indices("file: \"").filter_map(|(start, asked)| {
                let rest = &text[start + asked.len()..];
                let name = &rest[..rest.find('"')?];
                (!name.is_empty() && !name.contains('<') && name != "SKILL.md").then_some(name)
            });
            for file in listed.chain(asked) {
                assert!(
                    skill.builtin_files.iter().any(|(name, _)| name == file),
                    "{} names {file}",
                    skill.name
                );
            }
        }
    }

    #[test]
    fn the_creators_are_offered_as_skills_and_commands_unless_skills_are_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = tool::Workspace::new(dir.path()).unwrap();
        let settings = pi_store::settings::Settings::new(toml::Table::new().into(), None);
        for no_skills in [false, true] {
            let pinned = crate::args::Pinned {
                no_skills,
                skills: vec![skills::Skill::builtin(
                    "---\nname: caller-skill\ndescription: Instructions supplied by the caller.\n---\n",
                ).unwrap().with_files(vec![("example.txt".into(), "example")])],
                no_context_files: true,
                ..Default::default()
            };
            let resolved = super::resolve(
                &pinned,
                &workspace,
                &Default::default(),
                &settings,
                Some(std::sync::Arc::new(super::PromptFiles::default())),
                &[],
            )
            .unwrap();
            let loader = resolved.brief.registry.get(skills::Load::NAME);
            assert_eq!(loader.is_some(), !no_skills);
            if let Some(shelf) = &resolved.shelf {
                let found = shelf.now().0;
                let supplied = found
                    .skills
                    .iter()
                    .find(|s| s.name == "caller-skill")
                    .unwrap();
                assert_eq!(
                    supplied.builtin_files,
                    vec![("example.txt".into(), "example")]
                );
            }
            for name in [
                "tool-creator",
                "skill-creator",
                "pi-setup",
                "architecture",
                "caller-skill",
            ] {
                assert_eq!(
                    resolved
                        .commands
                        .iter()
                        .any(|command| command.word.strip_prefix('/') == Some(name)),
                    !no_skills,
                    "{name}"
                );
                if let Some(loader) = &loader {
                    assert!(loader.description().contains(name), "{name}");
                }
            }
        }
    }

    // A file the instructions never name is one the model never reads.
    #[test]
    fn the_tool_creator_names_each_of_its_files() {
        for (name, _) in super::TOOL_CREATOR_FILES {
            assert!(super::TOOL_CREATOR.contains(&format!("`{name}`")), "{name}");
        }
    }

    #[test]
    fn the_skill_creator_example_parses() {
        let example = super::SKILL_CREATOR
            .split("```markdown\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("a skill example");
        let (_, description) = skills::frontmatter(example).unwrap();
        assert!(description.is_some_and(|d| !d.is_empty()));
    }

    // Models copy these examples, so exercise them through tool discovery.
    #[test]
    fn the_tool_creator_examples_are_scripts_pi_accepts() {
        let dir = tempfile::tempdir().unwrap();
        for (language, filename) in [("rust", "count.rs"), ("sh", "count-shell")] {
            let example = super::TOOL_CREATOR
                .split(&format!("```{language}\n"))
                .nth(1)
                .and_then(|rest| rest.split("```").next())
                .expect("a tool example");
            std::fs::write(dir.path().join(filename), example).unwrap();
        }
        let (found, skipped) = toolbox::scripts::discover_in(dir.path());
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(found.len(), 2);
        for script in found {
            let schema = tool::Tool::schema(&script);
            assert_eq!(schema["required"], serde_json::json!(["file"]));
            assert_eq!(schema["properties"]["file"]["type"], "string");
        }
    }
}
