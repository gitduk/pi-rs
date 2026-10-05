//! The command line: what one run was started with.

use clap::Parser;

use pi_store::args::{EffortArg, TierArg};

#[derive(Parser, Debug)]
#[command(
    name = "pi",
    about = "A coding agent that stays inside one directory.",
    version = env!("CARGO_PKG_VERSION"),
    disable_version_flag = true
)]
pub struct Args {
    /// Print the version and exit.
    #[arg(short = 'v', long, action = clap::ArgAction::Version)]
    pub version: Option<bool>,
    /// The prompt. Reads stdin when omitted. Runs once and keeps nothing.
    pub prompt: Option<String>,

    /// Defaults and locally-defined models. Defaults to ~/.pi/settings.toml.
    #[arg(long, value_name = "FILE", env = "PI_CONFIG")]
    pub config: Option<String>,

    /// What the endpoint calls the model. A name ~/.pi/settings.toml does not
    /// describe is passed through with default numbers. Defaults to the
    /// resumed session's model, else the config's.
    #[arg(short, long)]
    pub model: Option<String>,

    /// Call this session something you will recognise later.
    #[arg(long, value_name = "TEXT")]
    pub name: Option<String>,

    /// Continue a saved session by id.
    #[arg(long, value_name = "ID")]
    pub resume: Option<String>,

    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long = "continue")]
    pub continue_last: bool,

    /// Overrides the base url the model's provider names, for pointing a
    /// configured model at a different host.
    #[arg(long)]
    pub base_url: Option<String>,

    /// Directory the agent may touch. Nothing outside it is reachable.
    #[arg(short = 'C', long, default_value = ".")]
    pub cwd: String,

    /// What this run may reach. read, write and exec each reach further
    /// into this machine; net reaches the web and nothing else. Defaults to
    /// exec, which covers net.
    #[arg(long, value_enum)]
    pub tier: Option<TierArg>,

    #[arg(long, value_enum)]
    pub effort: Option<EffortArg>,

    /// Override the model's context window, for a proxy whose real window is
    /// smaller than the config says.
    #[arg(long, value_name = "TOKENS")]
    pub context: Option<u32>,

    /// Replace the built-in system prompt.
    #[arg(long)]
    pub system: Option<String>,

    /// Ignore the skills on disk.
    #[arg(long)]
    pub no_skills: bool,

    /// Ignore ~/.pi/AGENTS.md, the project's, and memory.
    #[arg(long)]
    pub no_context_files: bool,

    /// Answer only; no progress, no usage line.
    #[arg(short, long)]
    pub quiet: bool,

    /// Distill memory and exit: what pi starts behind itself as it leaves,
    /// naming the session that just ended.
    #[arg(long, hide = true, value_name = "ID")]
    pub distill: Option<String>,
}

/// The flags that outrank the config for the whole run, and so are applied
/// again over every reload: all the core ever reads of the command line.
#[derive(Debug, Clone, Default)]
pub struct Pinned {
    pub config: Option<String>,
    pub base_url: Option<String>,
    pub context: Option<u32>,
    pub system: Option<String>,
    pub no_skills: bool,
    pub no_context_files: bool,
    pub effort: Option<EffortArg>,
    pub tier: Option<TierArg>,
}

impl Args {
    pub fn pinned(&self) -> Pinned {
        Pinned {
            config: self.config.clone(),
            base_url: self.base_url.clone(),
            context: self.context,
            system: self.system.clone(),
            no_skills: self.no_skills,
            no_context_files: self.no_context_files,
            effort: self.effort,
            tier: self.tier,
        }
    }
}
