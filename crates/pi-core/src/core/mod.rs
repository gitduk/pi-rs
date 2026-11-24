//! The state a session owns, and the verbs that move it: `Core` is the root
//! — the store, config, commands, settings, open checkouts.
//!
//! Each verb lives with its state — see `settings.rs`, `lane.rs`,
//! `status.rs`, `meter.rs`, `bash.rs`, `driver/`.

pub mod archive;
pub mod ask;
pub mod bash;
pub mod catalog;
pub mod context;
pub mod dial;
pub mod environment;
pub mod hooks;
pub mod instructions;
pub mod lane;
pub mod lanes;
pub mod mask;
pub mod mcp;
pub mod memory;
pub mod meter;
mod models;
mod net;
pub mod prompt;
pub mod question;
pub mod resolve;
pub mod saves;
pub mod settings;
pub mod shells;
mod skill_paths;
pub mod status;
pub mod statusline;
pub mod tools;
pub mod update;
pub mod worktree;

use crate::core::lane::Lane;
use crate::input::commands::{Command, help, with_prompts};
use crate::input::{Builtin, Intent, Step, notice, step_for};
use pi_store::config;
use pi_store::listing::Listing;
use pi_store::session::Store;
use pi_store::settings::Settings;

/// What the next pi is started with: every lane as `(root, filed session)`
/// in order, `front` in front, with the flags in force now (`/effort` moves one).
pub struct Restart {
    pub lanes: Vec<(std::path::PathBuf, Option<String>)>,
    pub front: usize,
    pub flags: Vec<String>,
}

/// Ask rtk what it is now, in the background, so `/status` can say before
/// the first command does.
pub fn warm_rtk() {
    tokio::spawn(pi_tools::rtk::warm());
}

/// The retry schedule, with the defaults the file may leave out. Read where
/// a run starts rather than kept on the agent, so a reload reaches it.
pub fn retry(config: &config::Config) -> agent::Retry {
    let mut retry = agent::Retry::default();
    if let Some(n) = config.retries {
        retry.attempts = n;
    }
    if let Some(secs) = config.idle_timeout {
        retry.idle = std::time::Duration::from_secs(secs.max(1));
    }
    retry
}

/// What every surface says when the transcript did not reach the disk: one
/// sentence, so a reworded copy cannot make one failure read as two.
pub fn not_saved(e: &impl std::fmt::Display) -> String {
    format!("the transcript was not saved: {e}")
}

/// Everything a run holds that outlives any one turn of it, and the one place
/// an intent is answered.
pub struct Core {
    pub runtime: crate::execution::host::Runtime,
    pub store: Store,
    pub catalog: catalog::Catalog,
    /// Command-line overrides shared by workspace configurations.
    pub pinned: crate::args::Pinned,
    /// Every checkout open in this run, in the order they were opened, and
    /// which of them is in front. One value: apart, the two can disagree.
    pub lanes: lanes::Lanes,
    /// Where calls that outlive their turn are kept, when a surface serves
    /// them; a run with nothing to come back to keeps no executions.
    pub executions: Option<std::sync::Arc<crate::driver::background::Table>>,
    /// Words the surface answers itself, listed beside the built-ins: each
    /// comes back as [`Step::Surface`]. Every resolve is handed them.
    pub surface: &'static [Command],
    /// Checkouts a removal has claimed until the surface lands it: no lane
    /// may open one while its directory is going.
    pub removals: Vec<crate::input::Target>,
}

impl Core {
    pub fn config(&self) -> &std::sync::Arc<config::Config> {
        &self.lane().environment().config
    }
    #[cfg(any(test, feature = "testing"))]
    pub fn config_mut(&mut self) -> &mut std::sync::Arc<config::Config> {
        &mut self.lane_mut().environment_mut().config
    }
    pub fn settings(&self) -> &Settings {
        &self.lane().environment().settings
    }
    pub fn commands(&self) -> &std::sync::Arc<Vec<Command>> {
        &self.lane().environment().commands
    }
    pub fn mcp(&self) -> &mcp::Live {
        &self.lane().environment().mcp
    }

    // Put the lane in front's command table in force: a skill belongs to one
    // tree, not another.
    fn in_force(&mut self) {
        self.in_force_at(self.lanes.front());
    }

    fn in_force_at(&mut self, at: usize) {
        let lane = self.lanes.at_mut(at);
        let table = lane.resolved().commands.clone();
        let environment = lane.environment_mut();
        environment.prompts_seen = environment.mcp.generation();
        environment.commands = with_prompts(table, environment.mcp.prompts());
    }

    pub fn refresh_prompts(&mut self) -> bool {
        if self.mcp().generation() == self.lane().environment().prompts_seen {
            return false;
        }
        self.in_force();
        true
    }

    fn model_config_changed_at(&self, at: usize) -> bool {
        let lane = &self.lanes[at];
        lane.resolved().catalog_generation != self.catalog.generation()
            || lane.environment().settings.project() != config::project_file(lane.root()).as_deref()
    }

    pub fn refresh_model_config(&mut self) -> Option<Vec<String>> {
        let at = self.lanes.front();
        self.model_config_changed_at(at)
            .then(|| self.refresh_config_at(at))
            .flatten()
    }

    pub fn refresh_config(&mut self) -> Option<Vec<String>> {
        self.refresh_config_at(self.lanes.front())
    }

    pub(crate) fn refresh_config_at(&mut self, at: usize) -> Option<Vec<String>> {
        let lane = &self.lanes[at];
        let now = resolve::watched(&self.pinned, lane.root());
        let was = &lane.resolved().watched;
        let token = lane.token();
        let files_changed = &now != was;
        let model_config_changed = self.model_config_changed_at(at);
        if (!files_changed && !model_config_changed)
            || lane.environment().refused.as_ref() == Some(&now)
        {
            return None;
        }
        let root = lane.root().to_path_buf();
        let moved: Vec<String> = now
            .iter()
            .filter(|s| !was.contains(s))
            .chain(was.iter().filter(|s| !now.iter().any(|n| n.0 == s.0)))
            .map(|(path, ..)| crate::core::instructions::short(path, &root))
            .collect();
        let reloaded = self.try_reload_at(at);
        tracing::info!(target: "pi::session", lane=token, moved=%moved.join(", "), model_config_changed, refused=reloaded.as_ref().err().map(String::as_str), "workspace config checked");
        match reloaded {
            Ok(mut said) => {
                self.lanes.at_mut(at).environment_mut().refused = None;
                if files_changed {
                    said.insert(0, format!("reloaded — {}", moved.join(", ")));
                }
                Some(said)
            }
            Err(why) => {
                self.lanes.at_mut(at).environment_mut().refused = Some(now);
                Some(vec![why])
            }
        }
    }

    /// Give each lane the skill commands its shelf now holds. `Some` when the
    /// table in force changed, with what is worth saying about the new skills;
    /// cheap when nothing did: one look at the directory, no parsing.
    pub fn refresh_skills(&mut self) -> Option<Vec<String>> {
        let mut moved = false;
        let mut said = Vec::new();
        for lane in self.lanes.all_mut() {
            let resolved = lane.resolved();
            let Some(shelf) = &resolved.shelf else {
                continue;
            };
            let (found, seen) = shelf.now();
            if seen == resolved.shelf_seen {
                continue;
            }
            let mut notes = Vec::new();
            let mut fresh = (**resolved).clone();
            fresh.commands = std::sync::Arc::new(crate::input::commands::commands(
                &found.skills,
                self.surface,
                &mut notes,
            ));
            fresh.shelf_seen = seen;
            said.extend(
                found
                    .problems
                    .iter()
                    .map(|p| format!("skill skipped — {p}")),
            );
            said.extend(notes);
            lane.keep(std::sync::Arc::new(fresh));
            moved = true;
        }
        if !moved {
            return None;
        }
        self.in_force();
        said.sort();
        said.dedup();
        tracing::info!(target: "pi::session", said = %said.join("; "), "skills changed on disk");
        Some(said)
    }

    /// The lane that holds the checkout at `root`, if one is open.
    pub fn lane_at(&self, root: &std::path::Path) -> Option<usize> {
        self.lanes.iter().position(|lane| lane.root() == root)
    }

    /// Offer background controls through each lane's shell, writing into
    /// `table`, which the surface serves; re-arm lanes already open.
    pub fn enable_background(&mut self, table: std::sync::Arc<crate::driver::background::Table>) {
        self.executions = Some(table);
        for at in 0..self.lanes.len() {
            let lane = &self.lanes[at];
            let retry = lane.resolved().retry;
            let archive = self.archive(lane.root().to_path_buf());
            let resolved = lane.resolved().clone();
            self.lanes
                .at_mut(at)
                .rearm(resolved, archive, retry, self.executions.clone(), |_| {});
        }
    }

    /// The checkout in front. Always one: `Lanes` keeps a lane in front even
    /// while others come and go.
    pub fn lane(&self) -> &Lane {
        self.lanes.ahead()
    }

    // Children file their work in the checkout they started in.
    fn archive(&self, root: std::path::PathBuf) -> std::sync::Arc<dyn agent::Archive> {
        archive::Filed::armed(self.store.clone(), root)
    }

    pub fn lane_mut(&mut self) -> &mut Lane {
        self.lanes.ahead_mut()
    }

    /// Where the lane `token` names sits now; `None` once it is gone.
    pub fn position_of(&self, token: u64) -> Option<usize> {
        self.lanes.position_of(token)
    }
}

impl Core {
    /// What to start the installed pi with, or `None` while it would lose
    /// something: it takes up every lane's transcript, in order, and nothing
    /// else this one holds.
    pub fn restart(&self) -> Option<Restart> {
        crate::core::update::launcher()?;
        if self
            .lanes
            .iter()
            .any(|lane| lane.is_running() || lane.unsaved().is_some())
        {
            return None;
        }
        if let Some(executions) = &self.executions
            && self.lanes.iter().any(|lane| executions.busy(lane.root()))
        {
            return None;
        }
        let lanes = (self.lanes.iter())
            .map(|lane| {
                let filed = self.store.filed(lane.root(), lane.id());
                (
                    lane.root().to_path_buf(),
                    filed.then(|| lane.id().to_string()),
                )
            })
            .collect();
        Some(Restart {
            lanes,
            front: self.lanes.front(),
            flags: self.pinned.args(),
        })
    }

    /// Leave each saved lane's executions beside its transcript, for the pi that
    /// opens that session next; an unsaved lane has nowhere to keep them.
    pub fn hand_over_background(&self) {
        let Some(table) = &self.executions else {
            return;
        };
        for lane in self.lanes.all() {
            let executions = table.carried(lane.root());
            if !self.store.filed(lane.root(), lane.id()) {
                if !executions.is_empty() {
                    tracing::warn!(target: "pi::background", executions = executions.len(), session = lane.id(), "not carried: session never saved");
                }
                continue;
            }
            let path = self.store.background_path(lane.root(), lane.id());
            if let Err(e) = crate::driver::background::hand_over(&path, &executions) {
                tracing::warn!(target: "pi::background", error = %e, path = %path.display(), executions = executions.len(), "not carried");
            }
        }
    }

    /// List again the executions an earlier pi left with each lane's session.
    pub fn revive_commands(&self) {
        let Some(table) = &self.executions else {
            return;
        };
        for lane in self.lanes.all() {
            let path = self.store.background_path(lane.root(), lane.id());
            let carried = match crate::driver::background::take_over(&path) {
                Ok(carried) => carried,
                Err(e) => {
                    tracing::warn!(target: "pi::background", error = %e, path = %path.display(), "carried executions lost");
                    continue;
                }
            };
            for execution in carried {
                // A session moved here from another checkout: its executions ran there.
                if execution.root != lane.root() {
                    tracing::warn!(target: "pi::background", execution = execution.id, root = %execution.root.display(), "no lane to carry it to");
                    continue;
                }
                let rerun = pi_tools::bash::rerun(execution.again.clone(), lane.ctx());
                table.revive(execution, rerun);
            }
        }
    }

    /// Carry out an intent, or say what the surface must do to carry it out.
    ///
    /// Exhaustive with no catch-all, like `Intent::fate`: the arms a surface
    /// answers for itself are named rather than swept up, so a new intent has
    /// to say which side of that line it falls on.
    pub fn dispatch(&mut self, intent: Intent) -> Step {
        match intent {
            Intent::Bash(command) => Step::Bash(command),
            // The shell already runs commands beside the turn, reports them
            // and carries them over; a `!!` is that, asked for by hand.
            Intent::Background(command) => Step::Tool {
                name: pi_tools::bash::Bash::NAME.to_string(),
                args: serde_json::json!({ "command": command, "background": true }),
            },
            Intent::Prompt(send) => Step::Prompt {
                send: send.into(),
                typed: None,
            },
            Intent::Surface { word, args } => Step::Surface { word, args },
            Intent::Tool { name, args } => {
                match crate::input::commands::call_of(
                    &name,
                    &args,
                    &self.lane().agent().brief.registry,
                ) {
                    Ok(args) => Step::Tool { name, args },
                    Err(why) => Step::Notice(why),
                }
            }
            Intent::Builtin(builtin) => self.builtin(builtin),
            Intent::Other { word, args } => step_for(self.commands(), &word, &args),
        }
    }

    // A built-in command, carried out. Exhaustive for the reason `dispatch` is.
    fn builtin(&mut self, builtin: Builtin) -> Step {
        match builtin {
            Builtin::Quit => Step::Quit,
            Builtin::Help => {
                Step::Handled(help(self.commands(), &self.lane().agent().brief.registry))
            }
            Builtin::Status => Step::Handled(self.status()),
            Builtin::Mask => Step::Handled(mask::listing()),
            Builtin::Context(arg) => Step::Handled(self.context(&arg)),
            // Bare, it is asked; with a level, a change the surface shows.
            Builtin::Effort(arg) if arg.trim().is_empty() => {
                Step::Handled(Listing::say(self.effort("")))
            }
            Builtin::Effort(arg) => Step::Notice(notice(self.effort(&arg))),
            Builtin::Mcp(arg) => self.mcp().command(&arg),
            Builtin::New => match self.new_session() {
                Ok(()) => Step::Swap(String::new()),
                Err(why) => Step::Notice(why),
            },
            Builtin::Resume(name) if name.is_empty() => {
                Step::Notice("/resume takes the id of a session".into())
            }
            Builtin::Resume(name) => match self.resume(&name) {
                Ok(said) => Step::Swap(notice(said)),
                Err(why) => Step::Notice(why),
            },
            Builtin::Rewind(id) => self.rewind_step(id.trim()),
            Builtin::Name(name) => self.name_step(name),
            Builtin::Compact(focus) => Step::Compact(Some(focus).filter(|f| !f.is_empty())),
            Builtin::ListModels => Step::Handled(self.model_listing()),
            Builtin::RefreshModels => {
                let config = self.config().clone();
                self.catalog.refresh(&self.pinned, &config);
                self.catalog.publish_status(self.lane().resolved());
                Step::Handled(self.model_listing())
            }
            Builtin::Model(name) => Step::Notice(notice(self.switch(&name))),
            Builtin::Mv(dir) => match dir.trim() {
                "" => Step::Handled(Listing::say([self.lane().root().display().to_string()])),
                dir => self.move_to(dir).unwrap_or_else(Step::Notice),
            },
            Builtin::Worktree(name) => self.worktree_step(&name),
            Builtin::Edit(name) => self.edit(&name),
        }
    }

    fn rewind_step(&mut self, id: &str) -> Step {
        match id.parse() {
            Ok(id) => {
                let (rewound, saved) = self.rewind_to(agent::session::EntryId(id));
                Step::Rewound(rewound, saved.err().map(|e| not_saved(&e)))
            }
            Err(_) => Step::Notice("/rewind takes the id of something you said".into()),
        }
    }

    fn name_step(&mut self, name: String) -> Step {
        let said = if name.is_empty() {
            self.lane_mut().set_name(None);
            format!("{} is unnamed again", self.lane().id())
        } else {
            let said = format!("{} is now “{name}”", self.lane().id());
            self.lane_mut().set_name(Some(name));
            said
        };
        // `/resume` reads its row off the file, so the name has to land
        // now; a run in flight has the session away and saves it later.
        match self.save() {
            Ok(()) => Step::Notice(said),
            Err(e) => Step::Notice(notice([said, not_saved(&e)])),
        }
    }

    fn worktree_step(&mut self, name: &str) -> Step {
        let name = name.trim();
        if name.is_empty() {
            return Step::Handled(self.worktree_listing());
        }
        let step = match name.split_once(char::is_whitespace) {
            None if name == "rm" => self.remove_all_worktrees(),
            Some(("rm", name)) if !name.trim().is_empty() => self.remove_worktree(name.trim()),
            _ => self.enter_worktree(name),
        };
        step.unwrap_or_else(Step::Notice)
    }
}

#[cfg(test)]
mod tests {

    use crate::input::commands::{BUILTIN, Choice, EditChoice, Input, Source, Sources, commands};
    use crate::input::{Builtin, Fate, Intent, read};
    use agent::session::{Entry, Prompt, Session};
    use skills::Skill;

    #[test]
    fn a_line_asks_for_the_lists_only_when_it_completes_against_one() {
        let asked = std::cell::RefCell::new(Vec::new());
        let sessions = || -> &[pi_store::session::ResumeChoice] {
            asked.borrow_mut().push("sessions");
            &[]
        };
        let worktrees = || -> &[Choice] {
            asked.borrow_mut().push("worktrees");
            &[]
        };
        let editable = || -> &[EditChoice] {
            asked.borrow_mut().push("editable");
            &[]
        };
        let mut notes = Vec::new();
        let commands = commands(&[], &[], &mut notes);
        let line = |text: &str| {
            asked.borrow_mut().clear();
            Input::new(text, text.len(), &commands, None).complete(&Sources {
                sessions: Some(&sessions),
                worktrees: Some(&worktrees),
                editable: Some(&editable),
                ..Default::default()
            });
            asked.borrow().join(",")
        };

        assert_eq!(line("fix the flaky test"), "", "a prompt completes nothing");
        assert_eq!(line("/new"), "", "a command word needs no list");
        assert_eq!(line("/model "), "", "models are not one of the two");
        assert_eq!(line("/resume "), "sessions");
        assert_eq!(line("/worktree "), "worktrees");
        assert_eq!(line("/worktree rm "), "worktrees");
        assert_eq!(line("/edit "), "editable");

        // And what the call asks for is what it completes against.
        let one = [pi_store::session::ResumeChoice {
            id: "s1".into(),
            prompt: "fix the flaky test".into(),
            name: None,
            touched: 0,
            rounds: 1,
            bytes: 0,
        }];
        let sessions = || &one[..];
        let offered = Input::new("/resume f", 9, &commands, None).complete(&Sources {
            sessions: Some(&sessions),
            ..Default::default()
        });
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].line, "/resume s1");
    }

    // Enter runs the row nearest the line: an accepted id must leave only its
    // own row there, not a newer message that happens to hold its digits.
    #[test]
    fn an_accepted_rewind_id_offers_only_its_own_row() {
        let mut notes = Vec::new();
        let commands = commands(&[], &[], &mut notes);
        let said = [
            Choice {
                name: "12".into(),
                note: "the first question".into(),
            },
            Choice {
                name: "40".into(),
                note: "retry issue 12".into(),
            },
        ];
        let offered = |text: &str| -> Vec<String> {
            Input::new(text, text.len(), &commands, None)
                .complete(&Sources {
                    said: &said,
                    ..Default::default()
                })
                .into_iter()
                .map(|c| c.line)
                .collect()
        };
        assert_eq!(offered("/rewind 12"), ["/rewind 12"]);
        assert_eq!(offered("/rewind issue"), ["/rewind 40"]);
        assert!(
            offered("/rewind ISSUE").is_empty(),
            "a capital asks for exact case"
        );
        assert_eq!(
            offered("/rewind "),
            ["/rewind 40", "/rewind 12"],
            "newest first"
        );
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            dir: std::path::PathBuf::from("/nowhere").join(name),
            paths: Default::default(),
            builtin: None,
            builtin_files: Vec::new(),
        }
    }

    #[test]
    fn a_session_open_in_another_pi_is_not_resumed_here() {
        let dir = tempfile::tempdir().unwrap();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");
        let root = dir.path().canonicalize().unwrap();
        let mut talk = agent::session::Session::default();
        talk.send_prompt("hi", None);
        core.store
            .save(
                &root,
                pi_store::session::Header {
                    id: "s2",
                    model: "model-a",
                    name: None,
                    created: 0,
                    spent: Default::default(),
                },
                &talk,
            )
            .unwrap();

        let elsewhere = core.store.claim(&root, "s2").unwrap();
        let err = core.resume("s2").unwrap_err();
        assert!(err.contains("already open"), "{err}");
        drop(elsewhere);
        // A child another test is forking may hold the lock's descriptor a
        // moment longer: the release is eventual, not instant.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let back = loop {
            match core.resume("s2") {
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                back => break back,
            }
        };
        assert!(back.is_ok(), "{back:?}");
        assert_eq!(core.lane().id(), "s2");
    }

    #[test]
    fn a_skill_written_mid_run_is_a_command_without_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");
        let shelf = dir.path().join("skills");
        let mut resolved = (**core.lane().resolved()).clone();
        resolved.shelf = Some(std::sync::Arc::new(skills::Shelf::new([&shelf])));
        core.lane_mut().keep(std::sync::Arc::new(resolved));
        core.refresh_skills();
        assert!(
            core.refresh_skills().is_none(),
            "nothing new, nothing to do"
        );

        std::fs::create_dir_all(shelf.join("haiku")).unwrap();
        std::fs::write(shelf.join("haiku/SKILL.md"), "---\ndescription: d\n---\n").unwrap();
        assert!(core.refresh_skills().is_some());
        assert!(core.commands().iter().any(|c| c.word == "/haiku"));
    }

    #[test]
    fn a_skill_cannot_take_a_built_in_word() {
        // A skill that could redefine /new would make a copied-in file the
        // owner of a word the session depends on.
        // A surface's own word holds the same way.
        let found = [
            skill("new", "not this one"),
            skill("look", "nor this"),
            skill("archify", "diagrams"),
        ];
        let words = [crate::input::commands::Command::surface(
            "/look",
            crate::input::commands::Args::None,
            "the surface's",
        )];
        let mut notes = Vec::new();
        let table = commands(&found, &words, &mut notes);
        for word in ["/new", "/look"] {
            let rows: Vec<_> = table.iter().filter(|c| c.word == word).collect();
            assert_eq!(rows.len(), 1, "{word}");
            assert!(matches!(rows[0].source, Source::Builtin), "{word}");
        }
        assert!(table.iter().any(|c| c.word == "/archify"));
        // Silently absent is how a user goes looking in the wrong place.
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[0].contains("/new"), "{}", notes[0]);
        assert!(notes[1].contains("/look"), "{}", notes[1]);
    }

    #[test]
    fn a_key_and_a_line_that_mean_the_same_thing_are_one_intent() {
        // The surface asks for it as `Intent::Builtin(Builtin::New)` too; the
        // typed word lands on the same variant, so `fate` sees one value.
        assert_eq!(read("/new", BUILTIN, None), Intent::Builtin(Builtin::New));
    }

    #[test]
    fn a_direct_tool_dispatches_beside_the_run_and_reports_bad_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(
            dir.path(),
            std::sync::Arc::new(Recording::default()),
            "model-a",
        );
        for (line, valid) in [("/read path=README.md", true), ("/read", false)] {
            let intent = read(line, BUILTIN, Some(&core.lane().agent().brief.registry));
            assert!(matches!(intent.fate(), Fate::Now));
            match core.dispatch(intent) {
                super::Step::Tool { name, args } if valid => {
                    assert_eq!(name, "read");
                    assert_eq!(args, serde_json::json!({"path": "README.md"}));
                }
                super::Step::Notice(why) if !valid => assert!(why.contains("needs path")),
                step => panic!("unexpected step for {line}: {step:?}"),
            }
        }
    }

    #[test]
    fn leaving_is_never_refused() {
        // One intent for `/exit`, `/quit` and the surface's way of leaving; it
        // always proceeds — a wedged run must not trap the user.
        assert!(matches!(Intent::Builtin(Builtin::Quit).fate(), Fate::Now));
    }

    #[test]
    fn reaching_for_the_transcript_is_refused_while_a_run_holds_it() {
        for intent in [
            Intent::Builtin(Builtin::New),
            Intent::Builtin(Builtin::Resume("1756240000-1".into())),
            Intent::Builtin(Builtin::Compact(String::new())),
            Intent::Builtin(Builtin::Mv("~/elsewhere".into())),
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
            Intent::Builtin(Builtin::Status),
            Intent::Builtin(Builtin::Help),
            Intent::Builtin(Builtin::ListModels),
            Intent::Builtin(Builtin::Worktree("tree".into())),
            Intent::Builtin(Builtin::Edit("AGENTS.md".into())),
            Intent::Tool {
                name: "wechat".into(),
                args: String::new(),
            },
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
            Intent::Prompt("the bug is in parse.rs".into()),
            Intent::Bash("ls".into()),
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

    #[test]
    fn a_bang_line_stores_what_was_typed_beside_what_was_sent() {
        let mut s = Session::new();
        s.push_bash(Prompt {
            text: "Ran `git status`\nnothing to commit".into(),
            images: Vec::new(),
            shown: Some("!git status".into()),
            relayed: None,
            context: Vec::new(),
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

    // One lane that has spent nothing.
    pub(super) fn a_lane(name: &str) -> crate::core::lane::Lane {
        let dir = std::env::temp_dir();
        let ws = tool::Workspace::new(&dir).expect("a workspace");
        let agent = std::sync::Arc::new(agent::Agent::new(
            std::sync::Arc::new(Recording::default()),
            test_spec("m"),
        ));
        let resolved = crate::core::lane::a_resolved("");
        let environment = super::environment::Environment::fixture(resolved.commands.clone());
        crate::core::lane::Lane::opened(crate::core::lane::Opening {
            id: name.into(),
            name: Some(name.to_string()),
            ..crate::core::lane::Opening::new(agent, resolved, tool::Ctx::new(ws), environment)
        })
    }

    // A transport that records the model each request asks for, and answers
    // one empty turn.
    #[derive(Default)]
    pub(super) struct Recording {
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

    pub(super) fn test_spec(model: &str) -> llm::model::ModelSpec {
        llm::model::ModelSpec {
            model: model.into(),
            wire_id: None,
            base_url: "http://localhost".into(),
            headers: Vec::new(),
            session_header: None,
            format: llm::model::Format::Anthropic {
                cache_control: llm::model::CacheControl::Off,
            },
            context_window: 200_000,
            max_output_tokens: 8_000,
            vision: true,
            accepts_effort: true,
            accepts_temperature: true,
            can_force_tool: true,
            pricing: llm::model::Pricing {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                ..Default::default()
            },
        }
    }

    // One lane on the given transport, with a subagent hung off it.
    pub(super) fn a_repl(
        root: &std::path::Path,
        transport: std::sync::Arc<Recording>,
        model: &str,
    ) -> crate::core::Core {
        let ws = tool::Workspace::new(root).unwrap();
        let mut agent = agent::Agent::new(transport, test_spec(model));
        let store = pi_store::session::Store::new(root.join("state"));
        let file = root.join("test-settings.toml");
        std::fs::write(&file, "").unwrap();
        let pinned = crate::args::Pinned {
            config: Some(file.display().to_string()),
            ..Default::default()
        };
        let mut resolved = crate::core::lane::a_resolved("standing");
        std::sync::Arc::make_mut(&mut resolved).watched = super::resolve::watched(&pinned, root);
        let resolved = crate::core::lane::arm(
            &mut agent,
            resolved,
            super::archive::Filed::armed(
                pi_store::session::Store::new(root.join("state")),
                root.to_path_buf(),
            ),
            agent::Retry::default(),
            None,
        );

        let environment = super::environment::Environment::fixture(resolved.commands.clone());
        let ctx = tool::Ctx::new(ws).with_session("s1", root.join("spill"));
        let mut lane = crate::core::lane::Lane::opened(crate::core::lane::Opening {
            id: "s1".into(),
            ..crate::core::lane::Opening::new(
                std::sync::Arc::new(agent),
                resolved,
                ctx,
                environment,
            )
        });
        lane.return_session(agent::session::Session::default());
        crate::core::Core {
            runtime: Default::default(),
            store,
            catalog: Default::default(),
            pinned,
            lanes: crate::core::lanes::Lanes::of(lane),
            executions: None,
            removals: Default::default(),
            surface: &[],
        }
    }

    // What the child asked for, once.
    async fn run_the_child(core: &crate::core::Core) {
        let subagent = core
            .lane()
            .agent()
            .brief
            .registry
            .get(subagent::Subagent::NAME)
            .unwrap();
        let ctx = tool::Ctx::new(core.lane().workspace().clone());
        subagent
            .execute(
                serde_json::json!({ "description": "go", "prompt": "go" }),
                &ctx,
            )
            .await
            .unwrap();
    }

    // `/model` rebuilds the subagent with the new endpoint and model, or a
    // child called after the switch keeps talking to the old one.
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

    // A menu's rows read upward from its line: the first is nearest to it.
    #[tokio::test]
    async fn the_model_list_puts_the_unpriced_then_the_cheapest_nearest_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut core = a_repl(dir.path(), std::sync::Arc::new(Recording::default()), "m");
        let tree: toml::Value = toml::from_str(
            r#"
[provider.local]
base_url = "http://localhost:7896/v1"
format   = "chat"

[provider.home]
base_url = "https://api.anthropic.com"
format   = "anthropic"

[models."home/pricey".pricing]
input_per_mtok = 20.0

[models."home/plain"]

[models."local/dear".pricing]
input_per_mtok = 10.0

[models."local/cheap"]
context_window = 262_100

[models."local/cheap".pricing]
input_per_mtok = 1.0

[models."local/deep".pricing]
input_per_mtok  = 1.0
output_per_mtok = 5.0
"#,
        )
        .unwrap();
        *core.config_mut() = std::sync::Arc::new(pi_store::config::Config::in_force(tree).unwrap());

        let choices = core.choices();
        let names: Vec<&str> = choices.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "home/plain",
                "local/cheap",
                "local/deep",
                "local/dear",
                "home/pricey"
            ]
        );
        // A format or a window of another width must not split the price column.
        let prices: Vec<usize> = choices.iter().filter_map(|c| c.note.find('$')).collect();
        assert_eq!(prices.len(), 4, "{:?}", choices);
        assert!(prices.iter().all(|at| *at == prices[0]), "{prices:?}");
    }

    #[test]
    fn edit_lists_and_opens_project_and_user_settings() {
        use crate::input::Step;
        use pi_store::config::PROJECT_FILE;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let mut core = a_repl(&root, std::sync::Arc::new(Recording::default()), "m");
        let user = "personal config.toml ";
        std::fs::write(root.join(user), "").unwrap();
        core.pinned.config = Some(root.join(user).display().to_string());
        let system = root.join("system.md");
        std::fs::write(&system, "Test prompt").unwrap();
        core.pinned.system = Some(system.display().to_string());
        core.pinned.no_context_files = true;
        let commands = commands(&[], &[], &mut Vec::new());
        let edit = |core: &mut super::Core, name: &str| {
            let line = format!("/edit {name}");
            let intent = read(&line, &commands, None);
            core.dispatch(intent)
        };

        let Step::Handled(list) = edit(&mut core, "") else {
            panic!("bare /edit should list its files");
        };
        let choices: Vec<EditChoice> = super::resolve::editable(&core.pinned, &root)
            .into_iter()
            .map(EditChoice::from)
            .collect();
        assert_eq!(choices[0].name, user);
        assert_eq!(choices[1].name, PROJECT_FILE);
        assert_eq!(
            choices[1].note,
            "[PROJ] Overrides user settings · new on save"
        );
        assert_eq!(choices[2].name, "system.md");
        let editable = || choices.as_slice();
        let sources = Sources {
            editable: Some(&editable),
            ..Default::default()
        };
        for (prefix, name) in [
            (".pi/set", PROJECT_FILE),
            ("set", PROJECT_FILE),
            ("personal ", user),
        ] {
            let line = format!("/edit {prefix}");
            let offered = Input::new(&line, line.len(), &commands, None).complete(&sources);
            assert!(
                offered.iter().any(|c| read(&c.line, &commands, None)
                    == Intent::Builtin(Builtin::Edit(name.into()))),
                "{line:?} offered {offered:?} from {choices:?}"
            );
        }
        assert_eq!(list.rows[1].cells, ["settings.toml", &choices[1].note]);
        for (argument, name) in [
            (PROJECT_FILE, PROJECT_FILE),
            ("\"personal config.toml \"", user),
        ] {
            let Step::Edit(file) = edit(&mut core, argument) else {
                panic!("/edit should open {name}");
            };
            assert_eq!(file.path, root.join(name));
            assert!(!file.is_prompt());
        }
        let step = edit(&mut core, "reset");
        assert!(matches!(step, Step::Notice(_)), "{step:?}");
        assert!(!commands.iter().any(|c| c.word == "/settings"));

        core.pinned.config = Some(root.join(PROJECT_FILE).display().to_string());
        let files = super::resolve::editable(&core.pinned, &root);
        let mut shared = files.iter().filter(|f| f.name == PROJECT_FILE);
        assert!(shared.next().unwrap().note.starts_with("[USER+PROJ]"));
        assert!(shared.next().is_none());
    }
}
