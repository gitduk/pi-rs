//! The state a session owns, and the verbs that move it.
//!
//! `App` is the root — the store, the config in force, the key map, the command
//! table, the settings, the checkouts open in this run — and what outlives a
//! turn lives here or on a lane and nowhere else. Every verb lives with the
//! state it moves: `settings.rs` for the config and its panel, `lanes.rs` for
//! the set of checkouts, `lane.rs` for one of them, `status.rs` for what a
//! reader is shown, `meter.rs` for what it cost. The jobs hang off the side:
//! `bash.rs`, `looping.rs`, `subagent.rs`, `wechat.rs`.

pub mod bash;
pub mod lane;
pub mod looping;
pub mod meter;
pub mod settings;
pub mod status;
pub mod subagent;
pub mod wechat;
pub mod worktree;

use crate::app::lane::Lane;
use crate::input::commands::{Command, help};
use crate::input::{Builtin, Intent, Step, WechatCmd, lines, step_for};
use crate::store::config;
use crate::store::session::Store;
use crate::store::settings::Settings;

/// Everything a run holds that outlives any one turn of it, and the one place
/// an intent is answered.
///
/// Both surfaces hold one and differ only in how they read a line and where
/// they put what comes back.
pub struct App {
    pub store: Store,
    /// Held so `/keys` can show what is actually in force, overrides included.
    pub keys: std::sync::Arc<crate::store::keys::Keys>,
    /// The config in force, as opposed to the one on disk. `/model` picks from
    /// this, so a switch cannot quietly apply an edit `/reload` has not.
    pub config: std::sync::Arc<config::Config>,
    /// The command line, kept because it outranks the config and so has to be
    /// re-applied over every reload.
    pub args: std::sync::Arc<crate::Args>,
    /// What a slash answers to, built-ins and skills together. Rebuilt by
    /// `/reload`, because a skill can appear between one turn and the next.
    ///
    /// Shared rather than copied, like the key map beside it: the terminal
    /// holds the same table to complete against and re-reads it whenever this
    /// one is replaced.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// The config file and what this session claimed on top of it: what
    /// `/settings` edits, and what `/reload` replaces the file's half of.
    pub settings: Settings,
    /// Every checkout open in this run, in the order they were opened. The
    /// main one is first, because that is where a run starts.
    pub lanes: Vec<Lane>,
    /// Which of them is in front. The surface shows one at a time.
    pub current: usize,
}

impl App {
    // Put the lane in front's key map and command table in force. A skill
    // belongs to one tree and not another, and so does a rebound key;
    // leaving the last lane's in place had this one answering to another
    // tree's.
    fn in_force(&mut self) {
        self.keys = self.lane().keys.clone();
        self.commands = self.lane().commands.clone();
    }

    /// The checkout in front. Indexing is safe by construction: `lanes` is
    /// never empty, so `current` always names one.
    pub fn lane(&self) -> &Lane {
        &self.lanes[self.current]
    }

    // Where a subagent started in this lane files what it did. Root and model
    // vary — a `/worktree` moves one, a `/model` the other — and the rest
    // never does.
    fn home(&self, root: std::path::PathBuf, model: String) -> std::sync::Arc<dyn agent::Home> {
        crate::app::subagent::Filed::armed(self.store.clone(), root, model)
    }

    pub fn lane_mut(&mut self) -> &mut Lane {
        &mut self.lanes[self.current]
    }
}

impl App {
    /// Carry out an intent, or say what the surface must do to carry it out.
    ///
    /// Exhaustive with no catch-all, like `Intent::fate`: the arms a surface
    /// answers for itself are named rather than swept up, so a new intent has
    /// to say which side of that line it falls on.
    pub fn dispatch(&mut self, intent: Intent) -> Step {
        match intent {
            Intent::Bash(command) => Step::Bash(command),
            Intent::Prompt(send) => Step::Prompt { send, typed: None },
            // The surface's: `/loop` arms the lane and queues its first round,
            // both of which only it can do, so it takes this before `run` is
            // reached. The arm stays so that a new intent has to say which side
            // of this line it falls on.
            Intent::Builtin(Builtin::Loop(_)) => Step::Handled(Vec::new()),
            Intent::Builtin(Builtin::Quit) => Step::Quit,
            Intent::Builtin(Builtin::Help) => Step::Handled(help(&self.commands)),
            Intent::Builtin(Builtin::Keys) => Step::Handled(self.keys.listing()),
            Intent::Builtin(Builtin::Reload) => Step::Handled(self.reload()),
            Intent::Builtin(Builtin::Status) => Step::Handled(self.status_lines()),
            Intent::Builtin(Builtin::New) => {
                self.fresh_session();
                Step::Swap(Vec::new())
            }
            Intent::Builtin(Builtin::Resume(name)) => {
                if name.is_empty() {
                    Step::Handled(self.resume_listing())
                } else {
                    match self.resume(&name) {
                        Ok(said) => Step::Swap(said),
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Intent::Builtin(Builtin::Name(name)) => {
                if name.is_empty() {
                    self.lane_mut().name = None;
                    lines(format!("{} is unnamed again", self.lane_mut().id))
                } else {
                    let said = format!("{} is now “{name}”", self.lane_mut().id);
                    self.lane_mut().name = Some(name);
                    lines(said)
                }
            }
            Intent::Builtin(Builtin::Compact(focus)) => {
                Step::Compact(Some(focus).filter(|f| !f.is_empty()))
            }
            Intent::Builtin(Builtin::Model(name)) => Step::Handled(if name.is_empty() {
                self.listing()
            } else {
                self.switch(&name)
            }),
            Intent::Builtin(Builtin::Worktree(name)) => {
                if name.is_empty() {
                    Step::Handled(self.worktree_listing())
                } else {
                    // `rm` + a name removes the tree; a bare `rm` still names
                    // a tree of its own, so only the two-word form is the verb.
                    let step = match name.split_once(char::is_whitespace) {
                        Some(("rm", name)) if !name.trim().is_empty() => {
                            self.remove_worktree(name.trim())
                        }
                        _ => self.enter_worktree(&name),
                    };
                    match step {
                        Ok(step) => step,
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Intent::Other { word, args } => step_for(&self.commands, &word, &args),
            Intent::Builtin(Builtin::Wechat(rest)) => match rest.trim() {
                "" => Step::Wechat(WechatCmd::Status),
                "on" => Step::Wechat(WechatCmd::On),
                "off" => Step::Wechat(WechatCmd::Off),
                other => Step::Flash(format!("unknown /wechat verb `{other}` — bare, on or off")),
            },
            Intent::Builtin(Builtin::Settings(rest)) => self.settings(&rest),
        }
    }
}

#[cfg(test)]
mod tests;
