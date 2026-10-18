//! The command line: what one run was started with.

use clap::{Parser, ValueEnum as _};

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

    /// Replace the system prompt, `SYSTEM.md` in pi's home included.
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

    /// A checkout to open again beside `--cwd`, as `ID=ROOT` (ID empty when
    /// nothing was filed there); repeated, in the order they were open.
    #[arg(long, value_name = "ID=ROOT", hide = true)]
    pub lane: Vec<String>,
}

/// One `--lane` value.
pub fn lane_arg(root: &std::path::Path, id: Option<&str>) -> String {
    format!("{}={}", id.unwrap_or_default(), root.display())
}

/// The command line a restart starts pi with: in `lanes[front]`, on its
/// session, every other lane carried as a `--lane`, then `flags`.
pub fn restart(
    lanes: &[(std::path::PathBuf, Option<String>)],
    front: usize,
    flags: &[String],
) -> Vec<std::ffi::OsString> {
    let (root, id) = &lanes[front];
    let mut out = vec!["-C".into(), root.into()];
    if let Some(id) = id {
        out.extend(["--resume".into(), id.into()]);
    }
    // Alone, the lane in front is all there is to carry.
    if lanes.len() > 1 {
        for (root, id) in lanes {
            out.extend(["--lane".into(), lane_arg(root, id.as_deref()).into()]);
        }
    }
    out.extend(flags.iter().map(Into::into));
    out
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

impl Pinned {
    /// As flags again, for a pi started in this one's place. Destructured so
    /// a field added here and not passed on fails to compile.
    pub fn args(&self) -> Vec<String> {
        let Self {
            config,
            base_url,
            context,
            system,
            no_skills,
            no_context_files,
            effort,
            tier,
        } = self;
        let mut out = Vec::new();
        let mut pass = |flag: &str, value: Option<String>| {
            if let Some(value) = value {
                out.extend([flag.to_string(), value]);
            }
        };
        pass("--config", config.clone());
        pass("--base-url", base_url.clone());
        pass("--context", context.map(|c| c.to_string()));
        pass("--system", system.clone());
        pass(
            "--effort",
            effort.and_then(|e| Some(e.to_possible_value()?.get_name().to_string())),
        );
        pass("--tier", tier.map(TierArg::name));
        out.extend(no_skills.then(|| "--no-skills".to_string()));
        out.extend(no_context_files.then(|| "--no-context-files".to_string()));
        out
    }
}

impl Args {
    /// The `--lane`s, as [`Core::reopen`](crate::core::Core::reopen) takes them.
    pub fn lanes(&self) -> Vec<(std::path::PathBuf, Option<String>)> {
        // An id never holds `=`, so the first one ends it.
        (self.lane.iter())
            .filter_map(|arg| arg.split_once('='))
            .map(|(id, root)| (root.into(), (!id.is_empty()).then(|| id.to_string())))
            .collect()
    }

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

#[cfg(test)]
mod tests {
    use super::{Args, lane_arg};
    use clap::Parser;
    use std::path::{Path, PathBuf};

    // What a restart writes is what the next pi reads, `=` in a path and all.
    #[test]
    fn a_lane_reads_back_as_it_was_written() {
        let filed = lane_arg(Path::new("/w/a=b"), Some("1792332839-3028349-1"));
        let fresh = lane_arg(Path::new("/w/c"), None);
        let args = Args::parse_from(["pi", "--lane", &filed, "--lane", &fresh]);
        assert_eq!(
            args.lanes(),
            [
                (PathBuf::from("/w/a=b"), Some("1792332839-3028349-1".into())),
                (PathBuf::from("/w/c"), None),
            ]
        );
    }
}
