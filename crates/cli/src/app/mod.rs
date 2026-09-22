pub mod bash;
pub mod lane;
pub mod lanes;
pub mod looping;
pub mod meter;
pub mod settings;
pub mod subagent;
pub mod wechat;
pub mod worktree;

use agent::Totals;

use crate::app::lane::Lane;
use crate::input::commands::{Command, help};
use crate::input::{Builtin, Intent, Step, WechatCmd, lines, refused, step_for};
use crate::store::config;
use crate::store::icons;
use crate::store::journal;
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
    /// What the transcript occupies now, for the line that says why there was
    /// nothing to compact. Zero while a run has it.
    pub fn tokens_now(&self) -> usize {
        self.tokens_now_at(self.current)
    }

    /// The same for a named lane: a compaction that finishes after the screen
    /// has moved on still has to say what it found.
    pub fn tokens_now_at(&self, at: usize) -> usize {
        let Some(lane) = self.lanes.get(at) else {
            return 0;
        };
        lane.session
            .as_ref()
            .map_or(0, |s| llm::estimate::tokens(&s.context(), &lane.agent.spec))
    }
}

// Enough about a model to choose between them: who serves it, how much it
// holds, and what it costs where that is known.
//
// Takes the three pieces rather than a config entry, because the running model
// may never have been one — a name passed through with default numbers has no
// entry to read.
fn summary(format: &str, window: u32, p: &llm::model::Pricing) -> String {
    let mut parts = vec![format.to_string(), format!("{}k", window / 1000)];
    if p.input_per_mtok > 0.0 || p.output_per_mtok > 0.0 {
        parts.push(format!(
            "${:.2}/${:.2} per Mtok",
            p.input_per_mtok, p.output_per_mtok
        ));
    }
    parts.join(icons::PART_SEP)
}

// What becomes of the transcript's reasoning once another model is reading it.
//
// Only ever asked about a model that did not write it — the origin recorded on
// each block cannot match after a switch — so the signed path is out and one of
// these three is what the transport will do with it.
fn demotion(replay: llm::model::ReplayThinking) -> &'static str {
    use llm::model::ReplayThinking as R;
    match replay {
        R::Tagged => "reasoning from the earlier turns replays wrapped in <think> tags",
        R::Off => "reasoning from the earlier turns is dropped rather than replayed",
    }
}

// Whether the transcript holds any prior-turn reasoning at all.
//
// Worth saying at a switch: it is the one part of the history that does not
// survive intact, and a model that suddenly reads its own earlier thinking as
// quoted prose is otherwise an unexplained change in tone.
fn carries_reasoning(session: &agent::session::Session) -> bool {
    // The view, not every entry: what compaction has already dropped is not
    // going to reach the new model in any form, demoted or otherwise.
    session.view().iter().any(|s| {
        s.entry().blocks().is_some_and(|bs| {
            bs.iter()
                .any(|b| matches!(b, llm::message::AssistantContent::Reasoning(_)))
        })
    })
}

impl App {
    // What this run stands on, in one place: the tail of the system prompt as
    // the model receives it, the two files a person opens when a run goes
    // wrong, and what the session has spent. The instruction files are named
    // rather than quoted — `standing` carries them whole, and their content is
    // in the files themselves.
    //
    // The spend is the session's — the status lines carry the run's own — read
    // live from the lane's tally rather than the lane's settled totals, so a run
    // under way is counted rather than waiting for it to end.
    fn status_lines(&self) -> Vec<String> {
        let lane = self.lane();
        let mut out = standing_head(&lane.standing);
        if !lane.context.is_empty() {
            out.push("context:".into());
            out.extend(lane.context.iter().map(|c| format!("- {c}")));
        }
        out.push(match journal::path() {
            Some(p) => format!("journal: {}", p.display()),
            None => "journal: not recording — PI_LOG is off, or it would not open".into(),
        });
        out.push(format!(
            "session: {}",
            self.store
                .path_of(lane.ctx.workspace.root(), &lane.id)
                .display()
        ));
        // Left out until something has been spent: a session nothing has been
        // asked of yet has no figure, and a row of dashes is not one.
        let spent = lane.tally.session();
        if spent != Totals::default() {
            out.push(format!("spent: {}", crate::store::text::spent(&spent)));
        }
        out
    }
}

// What the system prompt's tail says about the run, which is all of it up to
// the first instruction file. Split on the tag rather than counting parts, so
// that a field added to the head shows up here without being told to; the
// files are named separately because `standing` carries them whole.
fn standing_head(standing: &str) -> Vec<String> {
    standing
        .split("<instructions")
        .next()
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
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

    // `/worktree <name>`: create or reuse a checkout of this repository and
    // move the session into it.
    //
    // Each tree keeps its own transcript rather than one transcript following
    // the move: paths in it are workspace-relative, so under another root the
    // same string names a different file, and the file locks and edit shifts
    // are keyed by absolute path. Coming back therefore resumes what was being
    // said in that tree, not an empty page.
    fn enter_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane_mut().ctx.workspace.root().to_path_buf();
        let tree = crate::app::worktree::enter(&from, name).map_err(|e| refused("worktree", e))?;
        // Built before the comparison: both sides are then canonical, and a
        // path git and the workspace spell differently is still one directory.
        let ws = tools::Workspace::new(&tree.path)
            .and_then(|ws| ws.with_write_roots(&self.config.write_roots))
            .map_err(|e| refused("worktree", anyhow::anyhow!("{}: {e}", tree.path.display())))?;
        if ws.root() == from {
            return Ok(Step::Flash(format!("already in {}", tree.name)));
        }
        // Against the root it belongs to, so before the move, not after. An
        // empty session — nothing said yet — has nothing to keep, and one a run
        // has is saved by the run.
        if self.lane().session.as_ref().is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "the leaving session was not saved");
        }
        // Already open: the lane that holds it comes back whole. Nothing is
        // said — the screen changing, bar included, says where you are.
        if let Some(i) = self
            .lanes
            .iter()
            .position(|lane| lane.ctx.workspace.root() == ws.root())
        {
            self.current = i;
            self.in_force();
            return Ok(Step::Handled(Vec::new()));
        }
        let said = self.open_lane(ws, (!tree.main).then(|| tree.name.clone()))?;
        Ok(Step::Swap(said))
    }
    // `/worktree rm <name>`: remove the checkout `name` refers to — its
    // directory, the branch it was on, and every transcript recorded under
    // it. Git says no to a checkout with changes in it, and that refusal is
    // passed on rather than forced past.
    fn remove_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane().ctx.workspace.root().to_path_buf();
        if let Some(target) = crate::app::worktree::list(&from)
            .ok()
            .and_then(|trees| trees.into_iter().find(|t| !t.main && t.name == name))
        {
            let running = self.lanes.iter().enumerate().any(|(i, lane)| {
                i != self.current
                    && lane.ctx.workspace.root().starts_with(&target.path)
                    && (lane.is_running() || lane.looping.is_some())
            });
            if running {
                return Err(format!(
                    "`{name}` is running in another lane of this run — stop it first"
                ));
            }
        }
        let removed =
            crate::app::worktree::remove(&from, name).map_err(|e| refused("worktree", e))?;
        for i in (0..self.lanes.len()).rev() {
            if i != self.current
                && self.lanes[i]
                    .ctx
                    .workspace
                    .root()
                    .starts_with(&removed.path)
            {
                self.remove_lane(i);
            }
        }
        let dropped = self.store.drop_under(&removed.path);
        let mut said = vec![
            format!("removed {name}"),
            removed.path.display().to_string(),
        ];
        if let Some(branch) = removed.branch {
            said.push(format!("branch {branch} deleted"));
        }
        if let Some(note) = removed.note {
            said.push(note);
        }
        if dropped > 0 {
            said.push(format!("{dropped} session record(s) dropped"));
        }
        Ok(Step::Worktrees(said))
    }

    // The checkouts `/worktree` can move to, the repository's own first, the
    // one the session is in marked.
    fn worktree_listing(&self) -> Vec<String> {
        let here = self.lane().ctx.workspace.root();
        let trees = match crate::app::worktree::list(here) {
            Ok(t) => t,
            Err(e) => return vec![refused("worktree", e)],
        };
        // By containment rather than equality: a run started in a subdirectory
        // is still in that checkout, and it is the one to mark.
        let at = crate::app::worktree::holding(&trees, here).map(|t| t.path.clone());
        let width = trees
            .iter()
            .map(|t| unicode_width::UnicodeWidthStr::width(t.name.as_str()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = trees
            .iter()
            .map(|t| {
                let mark = if at.as_ref() == Some(&t.path) {
                    "*"
                } else {
                    " "
                };
                let on = t.branch.as_deref().unwrap_or("detached HEAD");
                format!("{mark} {}  {on}", crate::store::text::pad(&t.name, width))
            })
            .collect();
        out.push(format!(
            "/worktree <name> works in one, creating it under {}/ if it is not there",
            crate::app::worktree::DIR
        ));
        out.push("/worktree rm <name> removes one — its checkout, sessions and branch".into());
        out
    }
}

#[cfg(test)]
mod tests;
