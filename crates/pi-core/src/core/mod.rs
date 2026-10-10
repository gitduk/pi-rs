//! The state a session owns, and the verbs that move it: `Core` is the root
//! — the store, config, keys, commands, settings, open checkouts.
//!
//! Each verb lives with its state — see `settings.rs`, `lane.rs`,
//! `status.rs`, `meter.rs`, `bash.rs`, `driver/`.

pub mod archive;
pub mod bar;
pub mod bash;
pub mod content;
pub mod dial;
pub mod hooks;
pub mod lane;
pub mod mcp;
pub mod memory;
pub mod meter;
pub mod resolve;
pub mod settings;
pub mod status;
pub mod tools;
pub mod worktree;

use crate::core::lane::Lane;
use crate::input::commands::{Command, channel_command, help, with_channels, with_prompts};
use crate::input::{Builtin, ChannelCmd, Drive, Intent, Step, lines, step_for};
use pi_store::config;
use pi_store::listing::Listing;
use pi_store::session::Store;
use pi_store::settings::Settings;

/// Ask rtk what it is now, in the background, so `/status` can say before
/// the first command does.
pub fn warm_rtk() {
    tokio::spawn(toolbox::rtk::warm());
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
    format!("warning: the transcript was not saved: {e}")
}

/// Everything a run holds that outlives any one turn of it, and the one place
/// an intent is answered.
pub struct Core {
    pub store: Store,
    /// Held so `/keys` can show what is actually in force, overrides included.
    pub keys: std::sync::Arc<pi_store::keys::Keys>,
    /// The config in force, as opposed to the one on disk. `/model` picks from
    /// this, so a switch cannot quietly apply an edit a reload has not.
    pub config: std::sync::Arc<config::Config>,
    /// The command line's say over the config, re-applied over every reload.
    pub pinned: crate::args::Pinned,
    /// What a slash answers to: built-ins, channels and skills. Rebuilt by
    /// a reload, since a skill can appear between one turn and the next.
    ///
    /// Shared rather than copied: the terminal holds the same table and
    /// re-reads it whenever this is replaced.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// The commands of the run's channels, in every table put in force.
    pub channels: Vec<Command>,
    /// The servers' list generation `commands` holds the prompts of.
    pub prompts_seen: u64,
    /// The config files as last read: what `/settings` writes into and
    /// a reload reads again.
    pub settings: Settings,
    /// Every checkout open in this run, in the order they were opened. The
    /// main one is first, because that is where a run starts.
    pub lanes: Vec<Lane>,
    /// Which of them is in front. The surface shows one at a time.
    pub current: usize,
    /// By lane token, the files as they stood when a reload last refused
    /// them, so a broken file is said once rather than once a second.
    pub refused: std::collections::HashMap<u64, Vec<resolve::Stamp>>,
    /// Where `later` and `jobs` keep what comes back, when a surface serves
    /// them; a run with nothing to come back to offers neither tool.
    pub tables: Option<crate::driver::Tables>,
}

impl Core {
    // Put the lane in front's key map and command table in force: a skill
    // and a rebound key belong to one tree, not another.
    fn in_force(&mut self) {
        self.keys = self.lane().resolved().keys.clone();
        let table = with_channels(&self.lane().resolved().commands, &self.channels);
        self.prompts_seen = ::mcp::generation();
        self.commands = with_prompts(table, crate::core::mcp::prompts());
    }

    /// Put the servers' prompts in the table again when their lists moved.
    /// True when the table changed.
    pub fn refresh_prompts(&mut self) -> bool {
        if ::mcp::generation() == self.prompts_seen {
            return false;
        }
        self.in_force();
        true
    }

    /// Resolve the lane in front again when a file it was built from changed
    /// on disk. `Some` with what to say when it did; nothing to do otherwise.
    pub fn refresh_config(&mut self) -> Option<Vec<String>> {
        let lane = self.lane();
        let now = resolve::watched(&self.pinned, lane.root());
        let was = &lane.resolved().watched;
        let token = lane.token();
        if &now == was || self.refused.get(&token) == Some(&now) {
            return None;
        }
        let root = lane.root().to_path_buf();
        let moved: Vec<String> = now
            .iter()
            .filter(|s| !was.contains(s))
            .chain(was.iter().filter(|s| !now.iter().any(|n| n.0 == s.0)))
            .map(|(path, ..)| agent::instructions::short(path, &root))
            .collect();
        match self.try_reload() {
            Ok(mut said) => {
                self.refused.remove(&token);
                said.insert(0, format!("reloaded — {}", moved.join(", ")));
                Some(said)
            }
            Err(why) => {
                self.refused.insert(token, now);
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
        for lane in &mut self.lanes {
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
            fresh.commands =
                std::sync::Arc::new(crate::input::commands::commands(&found.skills, &mut notes));
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
        Some(said)
    }

    /// The context a turn on the lane in front runs with: the lane's, and
    /// where a call that outlives the turn goes on, when jobs are served.
    pub fn turn_ctx(&self, cancel: tokio_util::sync::CancellationToken) -> tool::Ctx {
        let ctx = self.lane().ctx_for(cancel);
        match &self.tables {
            Some(tables) => ctx.with_jobs(std::sync::Arc::new(crate::driver::jobs::Jobs::new(
                tables.jobs.clone(),
            ))),
            None => ctx,
        }
    }

    /// Offer `later` and `jobs` on every lane, writing into `tables`, which
    /// the surface serves: lanes armed before this are armed again.
    pub fn enable_tables(&mut self, tables: crate::driver::Tables) {
        self.tables = Some(tables);
        let retry = retry(&self.config);
        for at in 0..self.lanes.len() {
            let lane = &self.lanes[at];
            let archive =
                self.archive(lane.root().to_path_buf(), lane.agent().spec().model.clone());
            let resolved = lane.resolved().clone();
            self.lanes[at].rearm(resolved, archive, retry, self.tables.clone(), |_| {});
        }
    }

    /// Give each of `channels` its `/<name>` command.
    pub fn add_channels(&mut self, channels: &[std::sync::Arc<dyn ::channel::Channel>]) {
        self.channels = channels
            .iter()
            .map(|c| channel_command(c.as_ref()))
            .collect();
        self.in_force();
    }

    /// The checkout in front. Indexing is safe by construction: `lanes` is
    /// never empty, so `current` always names one.
    pub fn lane(&self) -> &Lane {
        &self.lanes[self.current]
    }

    // Where a subagent started in this lane files what it did. Root and
    // model vary (`/worktree`, `/model`); the rest never does.
    fn archive(
        &self,
        root: std::path::PathBuf,
        model: String,
    ) -> std::sync::Arc<dyn agent::Archive> {
        archive::Filed::armed(self.store.clone(), root, model)
    }

    pub fn lane_mut(&mut self) -> &mut Lane {
        &mut self.lanes[self.current]
    }

    /// Where the lane `token` names sits now; `None` once it is gone.
    pub fn position_of(&self, token: u64) -> Option<usize> {
        self.lanes.iter().position(|lane| lane.token() == token)
    }
}

impl Core {
    // Whether `goal` would start a turn. A skill is looked up, not
    // expanded — that would log it as invoked before any round has run.
    fn starts_turn(&self, goal: &str) -> bool {
        match crate::input::read(goal, &self.commands) {
            Intent::Prompt(_) | Intent::Bash(_) => true,
            Intent::Other { word, .. } => crate::input::starts_turn(&self.commands, &word),
            Intent::Builtin(_) => false,
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
            Intent::Prompt(send) => Step::Prompt { send, typed: None },
            Intent::Builtin(Builtin::Later(arg)) => Step::Drive(Drive::Later(arg)),
            Intent::Builtin(Builtin::Jobs(arg)) => Step::Drive(Drive::Jobs(arg)),
            Intent::Builtin(Builtin::Loop(goal)) => match goal.trim() {
                "" => Step::Drive(Drive::Loop(None)),
                goal if self.starts_turn(goal) => Step::Drive(Drive::Loop(Some(goal.to_string()))),
                goal => Step::Flash(format!(
                    "`{goal}` starts no turn — a loop needs one to measure"
                )),
            },
            Intent::Builtin(Builtin::Quit) => Step::Quit,
            Intent::Builtin(Builtin::Help) => Step::Handled(help(&self.commands)),
            Intent::Builtin(Builtin::Keys) => Step::Handled(self.keys.listing()),
            Intent::Builtin(Builtin::Status) => Step::Handled(self.status()),
            Intent::Builtin(Builtin::Effort(arg)) => Step::Handled(Listing::say(self.effort(&arg))),
            Intent::Builtin(Builtin::Mcp(arg)) => Step::Handled(crate::core::mcp::command(&arg)),
            Intent::Builtin(Builtin::Content) => Step::Handled(self.content()),
            Intent::Builtin(Builtin::New) => {
                self.fresh_session();
                Step::Swap(Listing::default())
            }
            Intent::Builtin(Builtin::Resume(name)) => {
                if name.is_empty() {
                    Step::Handled(Listing::say(self.resume_listing()))
                } else {
                    match self.resume(&name) {
                        Ok(said) => Step::Swap(Listing::say(said)),
                        Err(why) => Step::Handled(Listing::say([why])),
                    }
                }
            }
            Intent::Builtin(Builtin::Name(name)) => {
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
                    Ok(()) => lines(said),
                    Err(e) => Step::Handled(Listing::say([said, not_saved(&e)])),
                }
            }
            Intent::Builtin(Builtin::Compact(focus)) => {
                Step::Compact(Some(focus).filter(|f| !f.is_empty()))
            }
            Intent::Builtin(Builtin::Model(name)) => {
                let rows = if name.is_empty() {
                    self.listing()
                } else {
                    self.switch(&name)
                };
                Step::Handled(Listing::say(rows))
            }
            Intent::Builtin(Builtin::Worktree(name)) => {
                if name.is_empty() {
                    Step::Handled(Listing::say(self.worktree_listing()))
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
                        Err(why) => Step::Handled(Listing::say([why])),
                    }
                }
            }
            Intent::Other { word, args } => step_for(&self.commands, &word, &args),
            Intent::Builtin(Builtin::Channel(name, rest)) => match rest.trim() {
                "" => Step::Drive(Drive::Channel(name, ChannelCmd::Status)),
                "on" => Step::Drive(Drive::Channel(name, ChannelCmd::On)),
                "off" => Step::Drive(Drive::Channel(name, ChannelCmd::Off)),
                other => Step::Flash(format!("unknown /{name} verb `{other}` — bare, on or off")),
            },
            // Only the bare word opens the file; an argument is refused
            // rather than half-remembered as a verb.
            Intent::Builtin(Builtin::Settings(rest)) if rest.trim().is_empty() => {
                match pi_store::config::project_target(self.lane().root()) {
                    Some(file) => Step::Edit(file),
                    None => {
                        Step::Flash("no project here: a .pi.toml in $HOME is never read".into())
                    }
                }
            }
            Intent::Builtin(Builtin::Settings(_)) => {
                Step::Flash("bare /settings opens the project's .pi.toml".into())
            }
            Intent::Builtin(Builtin::Edit(name)) => self.edit(name.trim()),
        }
    }
}

#[cfg(test)]
mod tests {

    use crate::input::commands::{BUILTIN, Choice, Source, commands};
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
        let editable = || -> &[Choice] {
            asked.borrow_mut().push("editable");
            &[]
        };
        let mut notes = Vec::new();
        let commands = commands(&[], &mut notes);
        let line = |text: &str| {
            asked.borrow_mut().clear();
            crate::input::commands::complete(text, &commands, &[], sessions, worktrees, editable);
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
        let offered = crate::input::commands::complete(
            "/resume f",
            &commands,
            &[],
            || &one[..],
            || &[],
            || &[],
        );
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].line, "/resume s1");
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            dir: std::path::PathBuf::from("/nowhere").join(name),
            builtin: None,
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
            .save("s2", &root, "model-a", None, 0, &talk)
            .unwrap();

        let elsewhere = core.store.claim(&root, "s2").unwrap();
        let err = core.resume("s2").unwrap_err();
        assert!(err.contains("another pi"), "{err}");
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
        resolved.shelf = Some(std::sync::Arc::new(skills::Shelf::new(&shelf)));
        core.lane_mut().keep(std::sync::Arc::new(resolved));
        core.refresh_skills();
        assert!(
            core.refresh_skills().is_none(),
            "nothing new, nothing to do"
        );

        std::fs::create_dir_all(shelf.join("haiku")).unwrap();
        std::fs::write(shelf.join("haiku/SKILL.md"), "---\ndescription: d\n---\n").unwrap();
        assert!(core.refresh_skills().is_some());
        assert!(core.commands.iter().any(|c| c.word == "/haiku"));
    }

    #[test]
    fn a_skill_cannot_take_a_built_in_word() {
        // A skill that could redefine /new would make a copied-in file the
        // owner of a word the session depends on.
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

    #[test]
    fn a_key_and_a_line_that_mean_the_same_thing_are_one_intent() {
        // `ctrl+l` twice also returns `Intent::Builtin(Builtin::New)`; the
        // typed word lands on the same variant, so `fate` sees one value.
        assert_eq!(read("/new", BUILTIN), Intent::Builtin(Builtin::New));
    }

    #[test]
    fn leaving_is_never_refused() {
        // One intent for `/exit`, `/quit`, ctrl+d and double ctrl+c; it
        // always proceeds — a wedged run must not trap the user.
        assert!(matches!(Intent::Builtin(Builtin::Quit).fate(), Fate::Now));
    }

    #[test]
    fn reaching_for_the_transcript_is_refused_while_a_run_holds_it() {
        for intent in [
            Intent::Builtin(Builtin::New),
            Intent::Builtin(Builtin::Resume("1756240000-1".into())),
            Intent::Builtin(Builtin::Compact(String::new())),
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
            Intent::Builtin(Builtin::Keys),
            Intent::Builtin(Builtin::Model(String::new())),
            Intent::Builtin(Builtin::Worktree("tree".into())),
            Intent::Builtin(Builtin::Channel("wechat".into(), "on".into())),
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
            Intent::Builtin(Builtin::Settings(String::new())),
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
    pub(super) fn a_lane(name: &str) -> crate::core::lane::Lane {
        let dir = std::env::temp_dir();
        let ws = tool::Workspace::new(&dir).expect("a workspace");
        let agent = std::sync::Arc::new(agent::Agent::new(
            std::sync::Arc::new(Recording::default()),
            test_spec("m"),
        ));
        crate::core::lane::Lane::opened(crate::core::lane::Opening {
            id: name.into(),
            name: Some(name.to_string()),
            ..crate::core::lane::Opening::new(
                agent,
                crate::core::lane::a_resolved(""),
                tool::Ctx::new(ws),
            )
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
    pub(super) fn a_repl(
        root: &std::path::Path,
        transport: std::sync::Arc<Recording>,
        model: &str,
    ) -> crate::core::Core {
        let ws = tool::Workspace::new(root).unwrap();
        let mut agent = agent::Agent::new(transport, test_spec(model));
        let store = pi_store::session::Store::new(root.join("state"));
        let resolved = crate::core::lane::arm(
            &mut agent,
            crate::core::lane::a_resolved("standing"),
            super::archive::Filed::armed(
                pi_store::session::Store::new(root.join("state")),
                root.to_path_buf(),
                model.into(),
            ),
            agent::Retry::default(),
            None,
        );

        let keys = resolved.keys.clone();
        let commands = resolved.commands.clone();
        let mut lane = crate::core::lane::Lane::opened(crate::core::lane::Opening {
            id: "s1".into(),
            ..crate::core::lane::Opening::new(
                std::sync::Arc::new(agent),
                resolved,
                tool::Ctx::new(ws),
            )
        });
        lane.return_session(agent::session::Session::default());
        crate::core::Core {
            store,
            keys,
            config: std::sync::Arc::new(pi_store::config::Config::default()),
            pinned: crate::args::Pinned::default(),
            commands,
            channels: Vec::new(),
            prompts_seen: 0,
            settings: pi_store::settings::Settings::new(
                toml::Value::Table(Default::default()),
                None,
            ),
            lanes: vec![lane],
            refused: Default::default(),
            tables: None,
            current: 0,
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
}
