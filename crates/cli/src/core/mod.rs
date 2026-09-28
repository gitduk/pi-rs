//! The state a session owns, and the verbs that move it.
//!
//! `Core` is the root — the store, the config in force, the key map, the command
//! table, the settings, the checkouts open in this run — and what outlives a
//! turn lives here or on a lane and nowhere else. Every verb lives with the
//! state it moves: `settings.rs` for the config and its panel, `lane.rs` for
//! the checkouts (all of them in one file: a `Core` field holds the list, a
//! `Lane` one of them), `status.rs` for what a
//! reader is shown, `meter.rs` for what it cost. The jobs hang off the side:
//! `bash.rs`, `looping.rs`, `subagent.rs`, `channel/`.

pub mod bash;
pub mod channel;
pub mod lane;
pub mod looping;
pub mod meter;
pub mod settings;
pub mod status;
pub mod subagent;
pub mod worktree;

use crate::core::lane::Lane;
use crate::input::commands::{Command, help};
use crate::input::{Builtin, ChannelCmd, Intent, Step, lines, step_for};
use crate::store::config;
use crate::store::listing::Listing;
use crate::store::session::Store;
use crate::store::settings::Settings;

/// What every surface says when the transcript did not reach the disk: one
/// sentence, so a reworded copy cannot make one failure read as two.
pub(crate) fn not_saved(e: &impl std::fmt::Display) -> String {
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

impl Core {
    // Put the lane in front's key map and command table in force. A skill
    // belongs to one tree and not another, and so does a rebound key;
    // leaving the last lane's in place had this one answering to another
    // tree's.
    fn in_force(&mut self) {
        self.keys = self.lane().resolved.keys.clone();
        self.commands = self.lane().resolved.commands.clone();
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
        crate::core::subagent::Filed::armed(self.store.clone(), root, model)
    }

    pub fn lane_mut(&mut self) -> &mut Lane {
        &mut self.lanes[self.current]
    }
}

impl Core {
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
            Intent::Builtin(Builtin::Loop(_)) => Step::Handled(Listing::default()),
            Intent::Builtin(Builtin::Quit) => Step::Quit,
            Intent::Builtin(Builtin::Help) => Step::Handled(Listing::say(help(&self.commands))),
            Intent::Builtin(Builtin::Keys) => Step::Handled(self.keys.listing()),
            Intent::Builtin(Builtin::Reload) => Step::Handled(Listing::say(self.reload())),
            Intent::Builtin(Builtin::Status) => Step::Handled(Listing::say(self.status_lines())),
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
                    self.lane_mut().name = None;
                    format!("{} is unnamed again", self.lane_mut().id)
                } else {
                    let said = format!("{} is now “{name}”", self.lane_mut().id);
                    self.lane_mut().name = Some(name);
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
                "" => Step::Channel(name, ChannelCmd::Status),
                "on" => Step::Channel(name, ChannelCmd::On),
                "off" => Step::Channel(name, ChannelCmd::Off),
                other => Step::Flash(format!("unknown /{name} verb `{other}` — bare, on or off")),
            },
            // Only the bare word opens the panel; an argument is refused
            // rather than half-remembered as a verb.
            Intent::Builtin(Builtin::Settings(rest)) if rest.trim().is_empty() => Step::Panel,
            Intent::Builtin(Builtin::Settings(_)) => {
                Step::Flash("settings are edited in the panel — bare /settings opens it".into())
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

    #[test]
    fn a_key_and_a_line_that_mean_the_same_thing_are_one_intent() {
        // `ctrl+l` twice returns `Intent::Builtin(Builtin::New)` directly. This is the other
        // half: the typed word lands on the same variant, so there is no
        // second value for `fate` to answer differently about.
        assert_eq!(read("/new", BUILTIN), Intent::Builtin(Builtin::New));
    }

    #[test]
    fn leaving_is_never_refused() {
        // One intent for `/exit`, `/quit`, ctrl+d and a double ctrl+c — the
        // words are checked with the rest of the table — and it always
        // proceeds: a wedged run must not be able to trap the user.
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
            Intent::Builtin(Builtin::Channel("wechat", "on".into())),
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
            crate::core::subagent::Filed::armed(
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
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands,
            settings: crate::store::settings::Settings::new(toml::Value::Table(Default::default())),
            lanes: vec![lane],
            current: 0,
        }
    }

    // What the child asked for, once.
    async fn run_the_child(core: &crate::core::Core) {
        let subagent = core
            .lane()
            .agent
            .brief
            .registry
            .get(subagent::Subagent::NAME)
            .unwrap();
        let ctx = tool::Ctx::new(core.lane().ctx.workspace.clone());
        subagent
            .execute(
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
}
