//! What the App does to its lanes: opening, removing, switching, resuming
//! and saving the checkouts a run has in hand.
//!
//! The lane itself is `lane.rs`; this is the set of them. The methods stay on
//! `App` because most of them read the store, the config and the settings it
//! holds — a `Lanes` type would be those four passed down one at a time.

use agent::Totals;
use agent::session::Session;

use super::App;
use super::meter::summary;
use super::status::{carries_reasoning, demotion};
use crate::app::lane::Lane;
use crate::app::meter::Tally;
use crate::input::commands::{RESUME_WIDTH, ago};
use crate::input::{Rewound, refused};
use crate::store::config;
use crate::store::icons;
use crate::store::journal;
use crate::store::session::{self, Stored};

impl App {
    pub fn remove_lane(&mut self, at: usize) -> Lane {
        let lane = self.lanes.remove(at);
        if at < self.current || self.current >= self.lanes.len() {
            self.current = self.current.saturating_sub(1);
        }
        lane
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
    // What `/model` on its own shows.
    pub(super) fn listing(&self) -> Vec<String> {
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
    pub(super) fn fresh_session(&mut self) {
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
    // Open a checkout as a lane of its own, and put it in front.
    //
    // Whole or not at all, like every other path that reads a config: a tree
    // whose config or skills will not resolve leaves the run where it was.
    pub(super) fn open_lane(
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
    // The sessions `/resume` can switch to, newest first, the one running
    // now marked.
    pub(super) fn resume_listing(&self) -> Vec<String> {
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
    pub(super) fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
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
