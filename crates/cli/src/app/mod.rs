pub mod bash;
pub mod lane;
pub mod looping;
pub mod meter;
pub mod settings;
pub mod subagent;
pub mod wechat;
pub mod worktree;

use agent::Totals;
use agent::session::Session;

use crate::app::lane::Lane;
use crate::app::meter::Tally;
use crate::input::commands::{Command, RESUME_WIDTH, ago, help};
use crate::input::{Builtin, Intent, Rewound, Step, WechatCmd, lines, refused, step_for};
use crate::store::config;
use crate::store::icons;
use crate::store::journal;
use crate::store::session::{self, Store, Stored};
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

    pub fn remove_lane(&mut self, at: usize) -> Lane {
        let lane = self.lanes.remove(at);
        if at < self.current || self.current >= self.lanes.len() {
            self.current = self.current.saturating_sub(1);
        }
        lane
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
    /// Move this session to another model.
    ///
    /// The transcript comes with it. Reasoning blocks carry the model that
    /// produced them and every transport demotes one it did not write —
    /// signature dropped, replayed as text or as `<think>` per the new model's
    /// `thinking_replay` — so the history stays sendable instead of becoming a
    /// 400 on the next turn. Nothing is rewritten on the way: switch back and
    /// the original blocks are native again.
    ///
    /// What has been spent stays spent. Each turn was priced by the spec in
    /// force when it ran, and the total is the sum of those, so a switch to a
    /// dearer model does not reprice the cheap turns behind it.
    pub fn switch(&mut self, name: &str) -> Vec<String> {
        let dialled = match crate::dial(&self.args, &self.config, name, config::Origin::Command) {
            Ok(d) => d,
            Err(e) => {
                let held = self.lane_mut().agent.spec.model.clone();
                return vec![format!("still on {held} — {}", refused("switch", e))];
            }
        };
        // Compared after resolving, not before: `find` accepts a model's
        // `wire_id` as well as its table name, so the name typed and the id it
        // lands on need not be the same string. Comparing the typed one would
        // re-dial the model already running and then announce a reasoning
        // demotion that never happened.
        if dialled.spec.model == self.lane_mut().agent.spec.model {
            return vec![format!("already on {}", self.lane_mut().agent.spec.model)];
        }
        let mut said: Vec<String> = dialled.warning.into_iter().chain(dialled.notes).collect();
        let spec = &dialled.spec;
        said.push(format!(
            "now on {}{}{}",
            spec.model,
            icons::PART_SEP,
            summary(spec.format.name(), spec.context_window, &spec.pricing)
        ));
        // An absent transcript is one a run has, and it is writing this
        // model's reasoning into it as we speak — so say it either way.
        if self
            .lane_mut()
            .session
            .as_ref()
            .is_none_or(carries_reasoning)
        {
            said.push(demotion(spec.replay_thinking).into());
        }
        tracing::info!(
            target: "pi::session",
            from = %self.lane_mut().agent.spec.model,
            to = %spec.model,
            format = spec.format.name(),
            context_window = spec.context_window,
            "model switched"
        );
        self.retarget(dialled.transport, dialled.spec);
        said
    }

    // What `/model` on its own shows.
    fn listing(&self) -> Vec<String> {
        let here = &self.lane().agent.spec.model;
        let choices = self.choices();
        if choices.is_empty() {
            return vec![
                format!("on {here}, and ~/.pi/settings.toml now defines no model to switch to"),
                "see examples/pi.toml for what a [models.<name>] entry looks like".into(),
            ];
        }
        let width = choices.iter().map(|c| c.name.len()).max().unwrap_or(0);
        choices
            .iter()
            .map(|c| {
                let mark = if &c.name == here {
                    icons::CURRENT_ITEM
                } else {
                    " "
                };
                format!("{mark} {:width$}  {}", c.name, c.note)
            })
            .collect()
    }

    /// Save the transcript. Called after every turn: an interrupted one is
    /// exactly the one worth keeping.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_lane(self.current)
    }

    /// The same for a lane that is not in front: a run that ended out of sight
    /// still has to reach disk, and it is not the screen's turn that decides.
    pub fn save_lane(&self, at: usize) -> anyhow::Result<()> {
        // Away with a run, which saves it itself on the way back.
        let Some(lane) = self.lanes.get(at) else {
            return Ok(());
        };
        let Some(session) = &lane.session else {
            return Ok(());
        };
        self.store.save(
            &lane.id,
            lane.ctx.workspace.root(),
            &lane.agent.spec.model,
            lane.name.as_deref(),
            lane.created,
            session,
        )?;
        Ok(())
    }

    /// Shrink the transcript, or None when there was nothing to shrink — and
    /// likewise when a run has it, which is why `/compact` is refused then.
    ///
    /// Here rather than at each surface: both asked the agent directly, and
    /// both had to reach past the lane for the session to do it.
    pub async fn compact_now(
        &mut self,
        focus: Option<&str>,
    ) -> Option<(agent::Report, llm::stream::Usage)> {
        // One borrow of the lane, two of its fields: they are disjoint, and
        // asking twice would not be.
        let lane = self.lane_mut();
        let session = lane.session.as_mut()?;
        lane.agent.compact_now(session, focus).await
    }

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

    /// Rewind the conversation to an entry and write the shorter transcript
    /// back.
    ///
    /// The entry decides which of the two it is — the caller cannot get the
    /// pairing wrong, and a menu row that went stale against the transcript
    /// falls through to removing nothing.
    pub fn rewind_to(&mut self, entry: agent::session::EntryId) -> anyhow::Result<Rewound> {
        // A rewind is refused while a run has the transcript, so this is the
        // idle path; without it there is nothing to go back through.
        let Some(session) = &mut self.lane_mut().session else {
            return Ok(Rewound::Nothing);
        };
        let unsent = session.unsent_text(entry);
        let removed = match unsent {
            Some(_) => session.rollback_before(entry),
            None => session.rollback_to(entry),
        };
        if removed == 0 {
            return Ok(Rewound::Nothing);
        }
        self.save()?;
        Ok(match unsent {
            Some(text) => Rewound::Unsent(text),
            None => Rewound::Kept,
        })
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

    // Become the session this id names: the stamp that dates it, the journal
    // it writes to, and the namespace its spills are filed under.
    //
    // One place because the id and the stamp always travel together and the
    // two callers each set what the other did not — `created` was the one
    // that got missed, and a resumed session was then re-dated on its next
    // save with the stamp of the session it had just left.
    fn becomes(&mut self, id: String, created: u64) {
        self.lane_mut().id = id;
        self.lane_mut().created = created;
        // What `/status` reports is the lane's tally over its settled totals;
        // a new session starts both at nothing rather than the one just left.
        self.lane_mut().totals = Totals::default();
        self.lane_mut().tally = Tally::default();
        let id = self.lane().id.clone();
        let path = self
            .store
            .journal_path(self.lane().ctx.workspace.root(), &id);
        journal::switched(&path, &id);
        // Spills are filed under the session id; a session has to own its own
        // namespace or the one before it keeps swallowing them.
        self.lane_mut().ctx = self
            .lane_mut()
            .ctx
            .clone()
            .with_session(&self.lane_mut().id);
    }

    // Drop the in-memory conversation and open a fresh session under a new
    // id. The old transcript stays on disk.
    //
    // Says nothing: the screen it is rebuilt into is empty, which is the
    // whole of the news, and the id it opened under is the surface's own
    // business — as with a resumed one.
    fn fresh_session(&mut self) {
        self.lane_mut().session = Some(Session::default());
        // A name identifies one session; carried over it would name two, which
        // is what `/name` exists to prevent.
        self.lane_mut().name = None;
        self.becomes(session::new_id(), session::now());
    }

    // Take a stored transcript as the running one — entries, name and id.
    // Parting with what is being left is the caller's; they differ on when.
    fn adopt_session(&mut self, stored: Stored) -> Vec<String> {
        let (id, name, created) = (stored.id.clone(), stored.name.clone(), stored.created);
        let session = stored.into_session();
        self.lane_mut().name = name;
        self.lane_mut().session = Some(session);
        self.becomes(id, created);
        // The id is a timestamp with a pid in it — nothing to read, and the
        // transcript coming back on screen already says what was resumed. A
        // name is worth a line, being what the user called it.
        match self.lane_mut().name.as_deref() {
            Some(name) => vec![format!("resumed “{name}”")],
            None => Vec::new(),
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

    // Open a checkout as a lane of its own, and put it in front.
    //
    // Whole or not at all, like every other path that reads a config: a tree
    // whose config or skills will not resolve leaves the run where it was.
    fn open_lane(
        &mut self,
        ws: tools::Workspace,
        worktree: Option<String>,
    ) -> Result<Vec<String>, String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let project = config::load_project(&root).map_err(failed)?;
        let mut resolved = crate::resolve(
            &self.args,
            &ws,
            &self.config,
            &project,
            self.settings.claimed(),
        )
        .map_err(failed)?;

        let (events, inbox) = Lane::channel();
        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let home = self.home(root.clone(), self.lane().agent.spec.model.clone());
        let mut ag = (*self.lane().agent).clone();
        ag.apply(agent::Setup {
            registry: std::mem::take(&mut resolved.registry),
            system: std::mem::take(&mut resolved.system),
            tier: resolved.tier,
            effort: resolved.effort,
            task_max_turns: resolved.max_turns,
            task_deadline: resolved.task_deadline,
        });
        crate::app::subagent::hang(&mut ag, home, &resolved.standing);

        // Built, not cloned from the lane being left: a `Ctx`'s tables key on
        // absolute paths in one tree, and none of that lane's describe this.
        self.lanes.push(Lane {
            token: crate::app::lane::next_token(),
            agent: std::sync::Arc::new(ag),
            session: Some(Session::default()),
            id: String::new(),
            created: 0,
            name: None,
            totals: Totals::default(),
            tally: Tally::default(),

            context: resolved.context,
            standing: resolved.standing,
            ctx: tools::Ctx::new(ws),
            keys: std::sync::Arc::new(resolved.keys),
            commands: std::sync::Arc::new(resolved.commands),
            worktree,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            run: crate::app::lane::Run::Idle,
        });
        self.current = self.lanes.len() - 1;
        self.in_force();

        // Asked with the root the next save will file under, so a tree is found
        // by the same key it was stored by.
        let found = self.store.latest(&root);
        Ok(match found {
            Ok(stored) => self.adopt_session(stored),
            Err(e) => {
                // Nothing recorded for this tree is the ordinary case; an
                // archive that will not load is not, and says so only here.
                tracing::debug!(target: "pi::session", error = %e, "no session to resume in this worktree");
                self.fresh_session();
                Vec::new()
            }
        })
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

    // The sessions `/resume` can switch to, newest first, the one running
    // now marked.
    fn resume_listing(&self) -> Vec<String> {
        let list = self.store.choices(self.lane().ctx.workspace.root());
        if list.is_empty() {
            return vec![
                "no sessions recorded for this workspace".into(),
                "one is saved here at the end of every turn".into(),
            ];
        }
        // What a session is known by is its first question, not its id; a
        // session with nothing said yet is not worth naming.
        let shown: Vec<(bool, String, u64)> = list
            .iter()
            .map(|s| {
                let text = if s.prompt.is_empty() {
                    "(no question)".into()
                } else {
                    crate::store::text::clip(&s.prompt, RESUME_WIDTH)
                };
                (s.id == self.lane().id, text, s.created)
            })
            .collect();
        let width = shown
            .iter()
            .map(|(_, t, _)| unicode_width::UnicodeWidthStr::width(t.as_str()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = shown
            .iter()
            .map(|(mark, text, created)| {
                format!(
                    "{} {}  {:>10}",
                    if *mark { icons::CURRENT_ITEM } else { " " },
                    crate::store::text::pad(text, width),
                    ago(*created)
                )
            })
            .collect();
        out.push("resume one by typing /resume and Tab".into());
        out
    }

    // Switch to a saved session: its transcript, name and id become this
    // one's, and every further turn extends it. The session being left is
    // saved first, so nothing is lost on the way out.
    //
    // The model in charge does not change — that stays the prompt's decision
    // — so reasoning a different model wrote is demoted the way it is after
    // a `/model` switch.
    fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
        // Resuming the session already running is no switch; going through
        // would only zero the totals the bar is mid-way through showing.
        if id == self.lane().id {
            return Ok(Vec::new());
        }
        // The session being left has to survive too, or /resume throws it
        // away. An empty one — just opened, nothing said — has nothing to keep.
        if self
            .lane_mut()
            .session
            .as_ref()
            .is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "resume could not save the leaving session");
        }
        let stored = self.store.load(id).map_err(|e| refused("resume", e))?;
        Ok(self.adopt_session(stored))
    }
}

#[cfg(test)]
mod tests;
