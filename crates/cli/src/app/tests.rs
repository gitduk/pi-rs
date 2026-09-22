use crate::input::commands::{BUILTIN, Choice, Source, commands};
use crate::input::{Builtin, Fate, Intent, read};
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

// A App whose file tree is the given TOML, enough for the `/settings`
// surface to answer.
fn core_with_file(file: &str) -> crate::app::App {
    crate::app::App {
        store: crate::store::session::Store::new(std::env::temp_dir().join("pi-settings-get-test")),
        keys: std::sync::Arc::new(crate::store::keys::Keys::default()),
        config: std::sync::Arc::new(crate::store::config::Config::default()),
        args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
        commands: std::sync::Arc::new(Vec::new()),
        settings: crate::store::settings::Settings::new(toml::from_str(file).unwrap()),
        lanes: vec![a_lane("s")],
        current: 0,
    }
}

fn claimed_base_url() -> crate::app::App {
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
        Intent::Builtin(Builtin::Wechat("on".into())),
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

// Prose is the one thing a working run can still hear, so it does not
// wait for one — waiting is what makes a correction arrive too late.
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
fn a_lane(name: &str) -> crate::app::lane::Lane {
    let dir = std::env::temp_dir();
    let ws = tools::Workspace::new(&dir).expect("a workspace");
    let (events, inbox) = crate::app::lane::Lane::channel();
    crate::app::lane::Lane {
        token: crate::app::lane::next_token(),
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
        run: crate::app::lane::Run::Idle,
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
) -> crate::app::App {
    let ws = tools::Workspace::new(root).unwrap();
    let mut agent = agent::Agent::new(transport, test_spec(model));
    let store = crate::store::session::Store::new(root.join("state"));
    crate::app::subagent::hang(
        &mut agent,
        crate::app::subagent::Filed::armed(
            crate::store::session::Store::new(root.join("state")),
            root.to_path_buf(),
            model.into(),
        ),
        "standing",
    );

    let keys = std::sync::Arc::new(crate::store::keys::Keys::default());
    let commands = std::sync::Arc::new(Vec::<crate::app::Command>::new());
    let (events, inbox) = crate::app::lane::Lane::channel();
    let lane = crate::app::lane::Lane {
        token: crate::app::lane::next_token(),
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
        run: crate::app::lane::Run::Idle,
        keys: keys.clone(),
        commands: commands.clone(),
    };
    crate::app::App {
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
async fn run_the_child(core: &crate::app::App) {
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
    let dir = crate::app::worktree::test_repo();
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
    let dir = crate::app::worktree::test_repo();
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
    let dir = crate::app::worktree::test_repo();
    let transport = std::sync::Arc::new(Recording::default());
    let mut core = a_repl(dir.path(), transport, "model-a");

    core.enter_worktree("fix-tools").unwrap();
    assert_eq!(core.lanes.len(), 2);

    core.lanes[1].run = crate::app::lane::Run::Running {
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
