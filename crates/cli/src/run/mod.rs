pub mod bash;
pub mod lane;
pub mod looping;
pub mod meter;
pub mod subagent;
pub mod wechat;
pub mod worktree;

use agent::Totals;
use agent::session::Session;

use serde::Deserialize;

use crate::input::commands::{Choice, Command, RESUME_WIDTH, ago, help};
use crate::input::{Intent, Rewound, Step, WechatCmd, lines, refused, step_for};
use crate::run::lane::Lane;
use crate::run::meter::Tally;
use crate::store::config::{self, Config};
use crate::store::icons;
use crate::store::journal;
use crate::store::session::{self, Store, Stored};
use crate::store::settings::{self, Settings, mask_secret};

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
        crate::run::subagent::Filed::armed(self.store.clone(), root, model)
    }

    pub fn lane_mut(&mut self) -> &mut Lane {
        &mut self.lanes[self.current]
    }
}

impl App {
    /// Re-read the config and everything it decides.
    ///
    /// Whole or not at all: on any failure nothing changes, which is why the
    /// new state is computed in full before a field is touched. Pi separates
    /// global from project so a broken one of each does not take the other
    /// down; here the whole reload is refused instead, and what was running
    /// keeps running — the case that separation protects against cannot arise
    /// when nothing is applied.
    ///
    /// What the session owns is untouched by construction: the transcript, the
    /// name, the history, the model. Only what the config decides is
    /// replaced.
    pub fn reload(&mut self) -> Vec<String> {
        // Re-read the file tree; the claimed overrides stay.
        if let Err(e) = self.settings.reread(self.args.config.as_deref()) {
            return vec![format!("nothing reloaded — {}", refused("reload", e))];
        }
        let mut said = self.rebuild();
        // Name any claim that still shadows a line the file just changed.
        for path in self.settings.claimed().keys() {
            if let Some(old) = self.settings.file_value(path)
                && old != &self.settings.claimed()[path]
            {
                said.push(format!(
                    "{path}: the file changed it, but this session is still shadowing it — /settings, then r on the row takes the file back"
                ));
            }
        }
        said
    }

    // Take this config as the one in force: recompute everything it decides
    // and swap it in. Whole or not at all — nothing is touched until all of
    // it has been computed. `/reload` reads the file first; `/settings`
    // hands over a tree it has just edited.
    fn adopt(&mut self, config: Config) -> Result<Vec<String>, String> {
        let root = self.lane().ctx.workspace.root().to_path_buf();
        let failed = |e| Err(format!("nothing reloaded — {}", refused("reload", e)));
        let project = match config::load_project(&root) {
            Ok(p) => p,
            Err(e) => return failed(e),
        };
        let mut resolved = match crate::resolve(
            &self.args,
            &self.lane().ctx.workspace,
            &config,
            &project,
            self.settings.claimed(),
        ) {
            Ok(r) => r,
            Err(e) => return failed(e),
        };
        // Only here, with everything computed: a config or a skill set that
        // will not resolve leaves what is running exactly as it was.
        //
        // One `make_mut`: a run in flight holds the other reference, so this
        // is where the copy is taken, and taking it four times copies thrice
        // over.
        let home = self.home(root.clone(), self.lane().agent.spec.model.clone());
        let ag = std::sync::Arc::make_mut(&mut self.lane_mut().agent);
        ag.apply(agent::Setup {
            registry: std::mem::take(&mut resolved.registry),
            system: std::mem::take(&mut resolved.system),
            tier: resolved.tier,
            effort: resolved.effort,
            task_max_turns: resolved.max_turns,
            task_deadline: resolved.task_deadline,
        });
        crate::run::subagent::hang(ag, home, &resolved.standing);
        self.lane_mut().context = resolved.context;
        self.lane_mut().standing = resolved.standing;
        // A skill can appear between one turn and the next, so the table of
        // what a slash answers to is recomputed like everything else here —
        // onto the lane it belongs to, then into force.
        self.lane_mut().keys = std::sync::Arc::new(resolved.keys);
        self.lane_mut().commands = std::sync::Arc::new(resolved.commands);
        self.in_force();
        // The running model is deliberately not re-dialled: a reload re-reads
        // preferences, and which model this session is on was a decision, not a
        // preference. `/model` is how that one changes.
        self.config = std::sync::Arc::new(config);

        // A spec change forces a re-dial; compare with the same `dial` call
        // the running spec came from, so the command line's --base-url /
        // --context overrides keep applying exactly as they do at startup.
        let mut notes = Vec::new();
        match crate::dial(
            &self.args,
            &self.config,
            &self.lane().agent.spec.model,
            config::Origin::Command,
        ) {
            Ok(dialled) if dialled.spec != self.lane().agent.spec => {
                self.retarget(dialled.transport, dialled.spec);
                notes.extend(
                    dialled
                        .notes
                        .into_iter()
                        .filter(|n| !n.starts_with("assuming a")),
                );
                notes.extend(dialled.warning);
            }
            // Same spec, nothing to change; a failed dial keeps the old
            // transport but has to say so, or the config the model just
            // accepted disagrees with the endpoint it still talks to.
            Ok(_) => {}
            Err(e) => notes.push(format!(
                "`{}` not re-dialled — {}",
                self.lane_mut().agent.spec.model,
                e
            )),
        }
        tracing::info!(
            target: "pi::session",
            models = self.config.names().len(),
            rebound_keys = self.config.keys.len(),
            commands = self.commands.len(),
            effort = ?self.lane().agent.effort,
            system_bytes = self.lane().agent.system.len(),
            "reloaded"
        );
        Ok(notes)
    }

    // Recompute the config from the file tree plus the session's claimed
    // overrides, and adopt it.
    fn rebuild(&mut self) -> Vec<String> {
        self.rebuilt().unwrap_or_else(|why| vec![why])
    }

    // The file's rows with the session's claims on top — what the panel
    // shows and the read-only list prints. Path by path rather than one
    // overlaid tree, so a claim the file can no longer address (an ancestor
    // the file has turned into a non-table) still answers, with the file's
    // own value beside it for the mark.
    pub fn setting_rows(&self) -> Vec<settings::SettingRow> {
        self.settings.rows()
    }

    // The same, saying why when nothing could be adopted.
    fn rebuilt(&mut self) -> Result<Vec<String>, String> {
        let tree = self
            .settings
            .effective()
            .map_err(|e| refused("settings", e))?;
        let mut config = match config::Config::deserialize(tree) {
            Ok(c) => c,
            Err(e) => return Err(refused("settings", anyhow::anyhow!(e))),
        };
        config.apply_env_unclaimed(self.settings.claimed());
        self.adopt(config)
    }

    /// Take a value into the session: try the write on a scratch tree first,
    /// so a bad value touches nothing, then record it as a claim and rebuild.
    /// The panel's edit line answers through here, so a refusal comes back
    /// named, to be shown beside the edit that earned it.
    pub fn edit(&mut self, path: &str, raw: &str) -> Result<Vec<String>, String> {
        let (old, new) = self
            .settings
            .claim(path, raw)
            .map_err(|e| refused("settings", e))?;
        let mut said = self.rebuild();
        let old_shown = match &old {
            Some(v) => mask_secret(path, &settings::render(v)),
            None => "<unset>".to_string(),
        };
        said.push(format!(
            "{path}: {old_shown} → {} (session only)",
            mask_secret(path, &settings::render(&new))
        ));
        Ok(said)
    }

    /// The panel's r: the file's value takes the session back. No claim on
    /// the path means the file is already in force and there is nothing to
    /// say.
    pub fn revert(&mut self, path: &str) -> Vec<String> {
        if !self.settings.drop_claim(path) {
            return Vec::new();
        }
        let mut said = self.rebuild();
        said.push(format!("{path}: back to what the file says"));
        said
    }

    /// The panel's space: the session value at `path` replaces the file's
    /// line, the claim goes, and the config adopts what the file now says.
    /// The value was validated when the session took it, so only the disk
    /// can refuse.
    pub fn write_to_file(&mut self, path: &str) -> Result<Vec<String>, String> {
        let Some(value) = self.settings.claimed_value(path) else {
            return Ok(vec![format!("{path}: the session and the file agree")]);
        };
        let file = self
            .args
            .config
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(config::global_path)
            .ok_or_else(|| "no settings file to write".to_string())?;
        config::write(&file, path, value.clone()).map_err(|e| format!("{e:#}"))?;
        self.settings.drop_claim(path);
        self.settings
            .reread(self.args.config.as_deref())
            .map_err(|e| format!("{e:#}"))?;
        let mut said = self.rebuild();
        said.push(format!(
            "{path} = {} — written to the file",
            mask_secret(path, &settings::render(&value))
        ));
        Ok(said)
    }

    /// The models `/model` can reach, with what tells them apart.
    pub fn choices(&self) -> Vec<Choice> {
        let format = self.config.format.map(|f| f.name()).unwrap_or_default();
        self.config
            .models
            .iter()
            .map(|(name, entry)| Choice {
                name: name.clone(),
                note: summary(format, entry.context_window, &entry.pricing),
            })
            .collect()
    }

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

    // Point this lane at a new endpoint, and rebuild the subagent behind it.
    //
    // `Task` holds a snapshot of the agent it was built from, so a retarget
    // that stopped at the lane would leave the child on the old provider —
    // with the old key — while the status line named the new model.
    fn retarget(&mut self, transport: std::sync::Arc<dyn llm::Transport>, spec: llm::ModelSpec) {
        let home = self.home(
            self.lane().ctx.workspace.root().to_path_buf(),
            spec.model.clone(),
        );
        let standing = self.lane().standing.clone();
        let ag = std::sync::Arc::make_mut(&mut self.lane_mut().agent);
        ag.retarget(transport, spec);
        crate::run::subagent::hang(ag, home, &standing);
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
            // The surface's, like `Submit`: arming a lane and re-submitting a
            // line through `read` are both things only it can do, so it takes
            // this before `run` is reached.
            Intent::Loop(_) | Intent::LoopRound { .. } => Step::Handled(Vec::new()),
            Intent::Quit => Step::Quit,
            Intent::Help => Step::Handled(help(&self.commands)),
            Intent::Keys => Step::Handled(self.keys.listing()),
            Intent::Reload => Step::Handled(self.reload()),
            Intent::Status => Step::Handled(self.status_lines()),
            Intent::New => {
                self.fresh_session();
                Step::Swap(Vec::new())
            }
            Intent::Resume(name) => {
                if name.is_empty() {
                    Step::Handled(self.resume_listing())
                } else {
                    match self.resume(&name) {
                        Ok(said) => Step::Swap(said),
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Intent::Name(name) => {
                if name.is_empty() {
                    self.lane_mut().name = None;
                    lines(format!("{} is unnamed again", self.lane_mut().id))
                } else {
                    let said = format!("{} is now “{name}”", self.lane_mut().id);
                    self.lane_mut().name = Some(name);
                    lines(said)
                }
            }
            Intent::Compact(focus) => Step::Compact(Some(focus).filter(|f| !f.is_empty())),
            Intent::Model(name) => Step::Handled(if name.is_empty() {
                self.listing()
            } else {
                self.switch(&name)
            }),
            Intent::Worktree(name) => {
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
            Intent::Wechat(rest) => match rest.trim() {
                "" => Step::Wechat(WechatCmd::Status),
                "on" => Step::Wechat(WechatCmd::On),
                "off" => Step::Wechat(WechatCmd::Off),
                other => Step::Flash(format!("unknown /wechat verb `{other}` — bare, on or off")),
            },
            Intent::Settings(rest) => self.settings(&rest),
        }
    }

    // `/settings`. The panel is the whole surface: bare opens it, and anything
    // after the word is refused rather than half-remembered as a verb.
    fn settings(&mut self, rest: &str) -> Step {
        if rest.trim().is_empty() {
            self.open_panel()
        } else {
            Step::Flash("settings are edited in the panel — bare /settings opens it".into())
        }
    }

    // The bare `/settings`: the TUI's panel, or a read-only list when this
    // is not a terminal.
    fn open_panel(&mut self) -> Step {
        // The TUI intercepts bare `/settings` before it reaches here; the
        // line surface can only list.
        let mut out = Vec::new();
        for row in self.setting_rows() {
            let mut line = format!("{} = {}", row.path, mask_secret(&row.path, &row.value));
            if row.changed {
                line.push_str(&format!(" {}", icons::CHANGED_MARK));
                if let Some(file) = self.settings.file_value(&row.path) {
                    let file = mask_secret(&row.path, &settings::render(file));
                    line.push_str(&format!(" file: {file}"));
                }
            }
            out.push(line);
        }
        if out.is_empty() {
            out.push("nothing in ~/.pi/settings.toml yet".into());
        }
        Step::Handled(out)
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
        let tree = crate::run::worktree::enter(&from, name).map_err(|e| refused("worktree", e))?;
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
        if let Some(target) = crate::run::worktree::list(&from)
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
            crate::run::worktree::remove(&from, name).map_err(|e| refused("worktree", e))?;
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
        crate::run::subagent::hang(&mut ag, home, &resolved.standing);

        // Built, not cloned from the lane being left: a `Ctx`'s tables key on
        // absolute paths in one tree, and none of that lane's describe this.
        self.lanes.push(Lane {
            token: crate::run::lane::next_token(),
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
            run: crate::run::lane::Run::Idle,
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
        let trees = match crate::run::worktree::list(here) {
            Ok(t) => t,
            Err(e) => return vec![refused("worktree", e)],
        };
        // By containment rather than equality: a run started in a subdirectory
        // is still in that checkout, and it is the one to mark.
        let at = crate::run::worktree::holding(&trees, here).map(|t| t.path.clone());
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
            crate::run::worktree::DIR
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
mod tests {

    use crate::input::commands::{BUILTIN, Choice, Source, commands};
    use crate::input::{Fate, Intent, read};
    use agent::session::{Entry, Prompt, Session};
    use skills::Skill;

    // The sets behind `/resume` and `/worktree` cost a walk of every
    // transcript in the workspace and a `git worktree list`. A line that
    // completes against neither must not pay for either: the frame after every
    // turn was reading the whole bucket to redraw a menu nobody had opened.
    #[test]
    fn a_line_asks_for_the_lists_only_when_it_completes_against_one() {
        let asked = std::cell::RefCell::new(Vec::new());
        let sessions = || -> &[crate::store::session::ResumeChoice] {
            asked.borrow_mut().push("sessions");
            &[]
        };
        let worktrees = || -> &[Choice] {
            asked.borrow_mut().push("worktrees");
            &[]
        };
        let mut notes = Vec::new();
        let commands = commands(&[], &mut notes);
        let line = |text: &str| {
            asked.borrow_mut().clear();
            crate::input::commands::complete(text, &commands, &[], sessions, worktrees);
            asked.borrow().join(",")
        };

        assert_eq!(line("fix the flaky test"), "", "a prompt completes nothing");
        assert_eq!(line("/new"), "", "a command word needs no list");
        assert_eq!(line("/model "), "", "models are not one of the two");
        assert_eq!(line("/resume "), "sessions");
        assert_eq!(line("/worktree "), "worktrees");
        assert_eq!(line("/worktree rm "), "worktrees");

        // And what the call asks for is what it completes against.
        let one = [crate::store::session::ResumeChoice {
            id: "s1".into(),
            prompt: "fix the flaky test".into(),
            created: 0,
        }];
        let offered =
            crate::input::commands::complete("/resume f", &commands, &[], || &one[..], || &[]);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].line, "/resume s1");
    }

    #[test]
    fn every_listed_command_actually_parses() {
        // The table drives help and completion; a word that reached the list
        // without reaching `parse` would offer to complete into nothing.
        for c in BUILTIN {
            assert!(
                !matches!(read(&c.word), Intent::Other { .. } | Intent::Prompt(_)),
                "{} is listed but does not parse",
                c.word
            );
        }
    }

    // A App whose file tree is the given TOML, enough for the `/settings`
    // surface to answer.
    fn core_with_file(file: &str) -> crate::run::App {
        crate::run::App {
            store: crate::store::session::Store::new(
                std::env::temp_dir().join("pi-settings-get-test"),
            ),
            keys: std::sync::Arc::new(crate::store::keys::Keys::default()),
            config: std::sync::Arc::new(crate::store::config::Config::default()),
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands: std::sync::Arc::new(Vec::new()),
            settings: crate::store::settings::Settings::new(toml::from_str(file).unwrap()),
            lanes: vec![a_lane("s")],
            current: 0,
        }
    }

    fn claimed_base_url() -> crate::run::App {
        let mut core = core_with_file(r#"base_url = "http://127.0.0.1:7896""#);
        core.settings
            .claim("base_url", "http://127.0.0.1:7897")
            .expect("a valid claim");
        core
    }

    #[test]
    fn rows_answer_path_by_path_when_the_file_breaks_under_a_claim() {
        // `models` is no longer a table, so the claim cannot be overlaid onto
        // the file — the row is still due, with the mark and its r.
        let mut core = core_with_file("model = \"flash\"\nmodels = 3");
        core.settings
            .claim_unchecked("models.flash", toml::Value::String("m2".into()));
        let rows = core.setting_rows();
        let model = rows
            .iter()
            .find(|r| r.path == "model")
            .expect("the file's own row");
        assert!(!model.changed);
        let claimed = rows
            .iter()
            .find(|r| r.path == "models.flash")
            .expect("the claimed row");
        assert_eq!(claimed.value, "m2");
        assert!(claimed.changed, "the file has no value there to agree with");
    }

    // The panel's rows read file plus claims, so a claimed value is what the
    // panel shows — and the row is marked, the file still holding another.
    // A claim on a path the file lacks is its own row, not a patch onto a
    // value that is not there.
    #[test]
    fn panel_rows_read_the_claim_over_the_file_and_mark_it() {
        let core = claimed_base_url();
        let rows = core.setting_rows();
        let row = rows
            .iter()
            .find(|r| r.path == "base_url")
            .expect("the claimed path is a row");
        assert_eq!(row.value, "http://127.0.0.1:7897");
        assert!(row.changed, "the file still says 7896");

        let mut core = core_with_file("model = \"flash\"");
        core.settings
            .claim_unchecked("margins", toml::Value::Integer(2));
        let rows = core.setting_rows();
        let added = rows.iter().find(|r| r.path == "margins").expect("added");
        assert_eq!(added.value, "2");
        assert!(added.changed);
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            dir: std::path::PathBuf::from("/nowhere").join(name),
        }
    }

    #[test]
    fn a_skill_cannot_take_a_built_in_word() {
        // A repository contributes skills, and one that could redefine /new
        // would be a checkout taking the session over.
        let found = [skill("new", "not this one"), skill("archify", "diagrams")];
        let mut notes = Vec::new();
        let table = commands(&found, &mut notes);
        let new = table.iter().find(|c| c.word == "/new").unwrap();
        assert!(matches!(new.source, Source::Builtin));
        assert_eq!(table.iter().filter(|c| c.word == "/new").count(), 1);
        assert!(table.iter().any(|c| c.word == "/archify"));
        // Silently absent is how a user goes looking in the wrong place.
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("/new"), "{}", notes[0]);
    }

    // The gate, table by table. Every input reaches `fate`, so these stand in
    // for the key presses and phone messages that used to be answered by hand
    // in the loop — where nothing could test them without a terminal.

    #[test]
    fn a_key_and_a_line_that_mean_the_same_thing_are_one_intent() {
        // `ctrl+l` twice returns `Intent::New` directly. This is the other
        // half: the typed word lands on the same variant, so there is no
        // second value for `fate` to answer differently about.
        assert_eq!(read("/new"), Intent::New);
    }

    #[test]
    fn leaving_is_never_refused() {
        // One intent for `/exit`, `/quit`, ctrl+d and a double ctrl+c — the
        // words are checked with the rest of the table — and it always
        // proceeds: a wedged run must not be able to trap the user.
        assert!(matches!(Intent::Quit.fate(), Fate::Now));
    }

    #[test]
    fn reaching_for_the_transcript_is_refused_while_a_run_holds_it() {
        for intent in [
            Intent::New,
            Intent::Resume("1756240000-1".into()),
            Intent::Compact(String::new()),
        ] {
            assert!(
                matches!(intent.fate(), Fate::Refused(_)),
                "{intent:?} should be refused"
            );
        }
    }

    #[test]
    fn what_the_run_is_not_standing_on_goes_through() {
        for intent in [
            Intent::Status,
            Intent::Help,
            Intent::Keys,
            Intent::Reload,
            Intent::Model(String::new()),
            Intent::Worktree("tree".into()),
            Intent::Wechat("on".into()),
        ] {
            assert!(
                matches!(intent.fate(), Fate::Now),
                "{intent:?} should proceed"
            );
        }
    }

    #[test]
    fn what_wants_the_model_or_the_surface_waits() {
        for intent in [
            Intent::Bash("ls".into()),
            Intent::Settings(String::new()),
            Intent::Other {
                word: "/commit".into(),
                args: String::new(),
            },
        ] {
            assert!(
                matches!(intent.fate(), Fate::Queued),
                "{intent:?} should wait"
            );
        }
    }

    // Prose is the one thing a working run can still hear, so it does not
    // wait for one — waiting is what makes a correction arrive too late.
    #[test]
    fn prose_reaches_the_run_rather_than_waiting_for_it() {
        let said = "the bug is in parse.rs";
        assert!(matches!(Intent::Prompt(said.into()).fate(), Fate::Steered(text) if text == said));
        // The line the door read says the same thing the word would.
        assert!(matches!(
            read(said).fate(),
            Fate::Steered(text) if text == said
        ));
        // A slash word is not prose and still waits.
        assert!(matches!(read("/settings").fate(), Fate::Queued));
    }

    // The transcript and the screen want different strings from a `!` line:
    // the model needs the command *and* its output, the reader needs the line
    // they typed. Storing only the first is what made the rebuilt scrollback
    // print `Ran \`git status\`` as a prompt with the output indented beneath.
    #[test]
    fn a_bang_line_stores_what_was_typed_beside_what_was_sent() {
        let mut s = Session::new();
        s.push_bash(Prompt {
            text: "Ran `git status`\nnothing to commit".into(),
            image: None,
            shown: Some("!git status".into()),
        });

        let Entry::Bash { run: t, .. } = &s.entries()[0] else {
            panic!("a text entry")
        };
        assert!(
            t.text.contains("nothing to commit"),
            "the model reads the output"
        );
        assert_eq!(
            t.shown.as_deref(),
            Some("!git status"),
            "the reader sees the line"
        );
    }

    // One lane that has spent nothing, enough for `/settings` to answer.
    fn a_lane(name: &str) -> crate::run::lane::Lane {
        let dir = std::env::temp_dir();
        let ws = tools::Workspace::new(&dir).expect("a workspace");
        let (events, inbox) = crate::run::lane::Lane::channel();
        crate::run::lane::Lane {
            token: crate::run::lane::next_token(),
            agent: std::sync::Arc::new(agent::Agent::new(
                std::sync::Arc::new(Recording::default()),
                test_spec("m"),
            )),
            session: None,
            id: name.into(),
            created: 0,
            name: Some(name.to_string()),
            totals: agent::Totals::default(),
            tally: Default::default(),
            context: Vec::new(),
            standing: std::sync::Arc::from(""),
            ctx: tools::Ctx::new(ws),
            worktree: None,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            run: crate::run::lane::Run::Idle,
            keys: std::sync::Arc::new(crate::store::keys::Keys::default()),
            commands: std::sync::Arc::new(Vec::new()),
        }
    }

    // A transport that records the model each request asks for, and answers
    // one empty turn.
    #[derive(Default)]
    struct Recording {
        saw: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl llm::Transport for Recording {
        async fn stream(
            &self,
            spec: &llm::model::ModelSpec,
            _req: &llm::request::Request,
        ) -> llm::Result<futures::stream::BoxStream<'static, llm::Result<llm::stream::StreamEvent>>>
        {
            self.saw.lock().unwrap().push(spec.model.clone());
            let done = Ok(llm::stream::StreamEvent::Done {
                stop: llm::stream::StopReason::EndTurn,
                usage: llm::stream::Usage::default(),
            });
            Ok(Box::pin(futures::stream::iter(std::iter::once(done))))
        }
    }

    fn test_spec(model: &str) -> llm::model::ModelSpec {
        llm::model::ModelSpec {
            model: model.into(),
            base_url: "http://localhost".into(),
            format: llm::model::Format::Anthropic {
                cache_control: llm::model::CacheControl::Off,
            },
            context_window: 200_000,
            max_output_tokens: 8_000,
            vision: true,
            thinking: Some(llm::model::ThinkingControl::Budget),
            accepts_temperature: true,
            can_force_tool: true,
            replay_thinking: llm::model::ReplayThinking::Tagged,
            pricing: llm::model::Pricing {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                ..Default::default()
            },
        }
    }

    // One lane on the given transport, with a subagent hung off it.
    fn a_repl(
        root: &std::path::Path,
        transport: std::sync::Arc<Recording>,
        model: &str,
    ) -> crate::run::App {
        let ws = tools::Workspace::new(root).unwrap();
        let mut agent = agent::Agent::new(transport, test_spec(model));
        let store = crate::store::session::Store::new(root.join("state"));
        crate::run::subagent::hang(
            &mut agent,
            crate::run::subagent::Filed::armed(
                crate::store::session::Store::new(root.join("state")),
                root.to_path_buf(),
                model.into(),
            ),
            "standing",
        );

        let keys = std::sync::Arc::new(crate::store::keys::Keys::default());
        let commands = std::sync::Arc::new(Vec::<crate::run::Command>::new());
        let (events, inbox) = crate::run::lane::Lane::channel();
        let lane = crate::run::lane::Lane {
            token: crate::run::lane::next_token(),
            agent: std::sync::Arc::new(agent),
            session: Some(agent::session::Session::default()),
            id: "s1".into(),
            created: 0,
            name: None,
            context: Vec::new(),
            standing: std::sync::Arc::from("standing"),
            totals: agent::Totals::default(),
            tally: Default::default(),
            ctx: tools::Ctx::new(ws),
            worktree: None,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            run: crate::run::lane::Run::Idle,
            keys: keys.clone(),
            commands: commands.clone(),
        };
        crate::run::App {
            store,
            keys,
            config: std::sync::Arc::new(crate::store::config::Config::default()),
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands,
            settings: crate::store::settings::Settings::new(toml::Value::Table(Default::default())),
            lanes: vec![lane],
            current: 0,
        }
    }

    // What the child asked for, once.
    async fn run_the_child(core: &crate::run::App) {
        let task = core.lane().agent.registry.get(task::Task::NAME).unwrap();
        let ctx = tools::Ctx::new(core.lane().ctx.workspace.clone());
        task.execute(
            serde_json::json!({ "description": "go", "prompt": "go" }),
            &ctx,
        )
        .await
        .unwrap();
    }

    // `/model` retargets the lane's agent and rebuilds the subagent behind
    // it; the rebuild has to carry the new endpoint and model, or a child
    // called after the switch keeps talking to the old one with the old key.
    #[tokio::test]
    async fn retarget_rebuilds_the_subagent_on_the_new_model() {
        let dir = tempfile::tempdir().unwrap();
        let old_saw = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let old_transport = std::sync::Arc::new(Recording {
            saw: old_saw.clone(),
        });
        let mut core = a_repl(dir.path(), old_transport, "model-a");

        let new_saw = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let new_transport = std::sync::Arc::new(Recording {
            saw: new_saw.clone(),
        });
        core.retarget(new_transport, test_spec("model-b"));
        run_the_child(&core).await;

        let saw = new_saw.lock().unwrap();
        assert!(
            saw.iter().any(|m| m == "model-b"),
            "the child asks the new model: {saw:?}"
        );
        assert!(!saw.iter().any(|m| m == "model-a"), "{saw:?}");
        assert!(
            old_saw.lock().unwrap().is_empty(),
            "the old endpoint is gone"
        );
    }

    #[test]
    fn remove_worktree_closes_idle_lane_in_same_run() {
        let dir = crate::run::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("fix-tools").unwrap();
        assert_eq!(core.lanes.len(), 2);
        assert_eq!(core.current, 1);

        core.current = 0;
        core.in_force();

        let res = core.remove_worktree("fix-tools");
        assert!(res.is_ok(), "remove_worktree failed: {res:?}");
        assert_eq!(core.lanes.len(), 1);
        assert_eq!(core.current, 0);
        assert!(!dir.path().join(".worktrees/fix-tools").exists());
    }

    #[test]
    fn remove_worktree_updates_current_index_when_earlier_lane_is_closed() {
        let dir = crate::run::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("feat-one").unwrap();
        core.enter_worktree("feat-two").unwrap();
        assert_eq!(core.lanes.len(), 3);
        assert_eq!(core.current, 2);

        let res = core.remove_worktree("feat-one");
        assert!(res.is_ok(), "remove_worktree failed: {res:?}");
        assert_eq!(core.lanes.len(), 2);
        assert_eq!(core.current, 1);
        assert_eq!(core.lane().worktree.as_deref(), Some("feat-two"));
    }

    #[test]
    fn remove_worktree_refuses_when_another_lane_is_running() {
        let dir = crate::run::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("fix-tools").unwrap();
        assert_eq!(core.lanes.len(), 2);

        core.lanes[1].run = crate::run::lane::Run::Running {
            cancel: tokio_util::sync::CancellationToken::new(),
            steer: None,
            unsend: false,
        };

        core.current = 0;
        core.in_force();

        let err = core.remove_worktree("fix-tools").unwrap_err();
        assert!(err.contains("running in another lane"), "{err}");
        assert_eq!(core.lanes.len(), 2);
        assert!(dir.path().join(".worktrees/fix-tools").exists());
    }
}
