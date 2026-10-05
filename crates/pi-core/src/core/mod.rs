//! The state a session owns, and the verbs that move it: `Core` is the root
//! — the store, config, keys, commands, settings, open checkouts.
//!
//! Each verb lives with its state — see `settings.rs`, `lane.rs`,
//! `status.rs`, `meter.rs`, `bash.rs`, `driver/`.

pub mod bash;
pub mod dial;
pub mod lane;
pub mod meter;
pub mod resolve;
pub mod settings;
pub mod status;
pub mod tools;
pub mod worktree;

use crate::core::lane::Lane;
use crate::input::commands::{Command, channel_command, help, with_channels};
use crate::input::{Builtin, ChannelCmd, Drive, Intent, Step, lines, step_for};
use crate::store::config;
use crate::store::listing::Listing;
use crate::store::session::Store;
use crate::store::settings::Settings;

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
    pub keys: std::sync::Arc<crate::store::keys::Keys>,
    /// The config in force, as opposed to the one on disk. `/model` picks from
    /// this, so a switch cannot quietly apply an edit `/reload` has not.
    pub config: std::sync::Arc<config::Config>,
    /// The command line's say over the config, re-applied over every reload.
    pub pinned: crate::args::Pinned,
    /// What a slash answers to: built-ins, channels and skills. Rebuilt by
    /// `/reload`, since a skill can appear between one turn and the next.
    ///
    /// Shared rather than copied: the terminal holds the same table and
    /// re-reads it whenever this is replaced.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// The commands of the run's channels, in every table put in force.
    pub channels: Vec<Command>,
    /// The config files as last read: what `/settings` writes into and
    /// `/reload` reads again.
    pub settings: Settings,
    /// Every checkout open in this run, in the order they were opened. The
    /// main one is first, because that is where a run starts.
    pub lanes: Vec<Lane>,
    /// Which of them is in front. The surface shows one at a time.
    pub current: usize,
}

impl Core {
    // Put the lane in front's key map and command table in force: a skill
    // and a rebound key belong to one tree, not another.
    fn in_force(&mut self) {
        self.keys = self.lane().resolved().keys.clone();
        self.commands = with_channels(&self.lane().resolved().commands, &self.channels);
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
        crate::store::archive::Filed::armed(self.store.clone(), root, model)
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
            Intent::Other { word, .. } => crate::input::skill_for(&self.commands, &word).is_some(),
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
            Intent::Builtin(Builtin::Loop(goal)) => match goal.trim() {
                "" => Step::Drive(Drive::Loop(None)),
                goal if self.starts_turn(goal) => Step::Drive(Drive::Loop(Some(goal.to_string()))),
                goal => Step::Flash(format!(
                    "`{goal}` starts no turn — a loop needs one to measure"
                )),
            },
            Intent::Builtin(Builtin::Quit) => Step::Quit,
            Intent::Builtin(Builtin::Help) => Step::Handled(Listing::say(help(&self.commands))),
            Intent::Builtin(Builtin::Keys) => Step::Handled(self.keys.listing()),
            Intent::Builtin(Builtin::Reload) => Step::Handled(Listing::say(self.reload())),
            Intent::Builtin(Builtin::Status) => Step::Handled(self.status()),
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
                match crate::store::config::project_target(self.lane().root()) {
                    Some(file) => Step::EditConfig(file),
                    None => {
                        Step::Flash("no project here: a .pi.toml in $HOME is never read".into())
                    }
                }
            }
            Intent::Builtin(Builtin::Settings(_)) => {
                Step::Flash("bare /settings opens the project's .pi.toml".into())
            }
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
            name: None,
            created: 0,
        }];
        let offered =
            crate::input::commands::complete("/resume f", &commands, &[], || &one[..], || &[]);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].line, "/resume s1");
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
            Intent::Builtin(Builtin::Reload),
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
    fn prose_reaches_the_run_rather_than_waiting_for_it() {
        let said = "the bug is in parse.rs";
        assert!(matches!(Intent::Prompt(said.into()).fate(), Fate::Steered(text) if text == said));
        // The line the door read says the same thing the word would.
        assert!(matches!(
            read(said, BUILTIN).fate(),
            Fate::Steered(text) if text == said
        ));
        // A slash word is not prose and still waits.
        assert!(matches!(read("/settings", BUILTIN).fate(), Fate::Queued));
    }

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
        let store = crate::store::session::Store::new(root.join("state"));
        let resolved = crate::core::lane::arm(
            &mut agent,
            crate::core::lane::a_resolved("standing"),
            crate::store::archive::Filed::armed(
                crate::store::session::Store::new(root.join("state")),
                root.to_path_buf(),
                model.into(),
            ),
            agent::Retry::default(),
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
            config: std::sync::Arc::new(crate::store::config::Config::default()),
            pinned: crate::args::Pinned::default(),
            commands,
            channels: Vec::new(),
            settings: crate::store::settings::Settings::new(
                toml::Value::Table(Default::default()),
                None,
            ),
            lanes: vec![lane],
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
