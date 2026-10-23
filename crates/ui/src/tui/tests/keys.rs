use crate::tui::{Asked, Deed, Intent, View, view_at};
use pi_core::input::commands::{Command, Source};
use pi_core::input::{Builtin, Fate};
use pi_store::listing::Listing;

use super::harness::*;

#[test]
fn a_deed_says_whether_a_run_in_flight_allows_it() {
    // Only the two that rewrite the transcript care about a run in flight;
    // the rest are the screen's own and always go through.
    assert!(matches!(Deed::Rewind.fate(), Fate::Refused(_)));
    for deed in [Deed::Nothing, Deed::External, Deed::Interrupt, Deed::Unsend] {
        assert!(matches!(deed.fate(), Fate::Now), "{deed:?} should proceed");
    }
}

// `fate` only judges a run in flight; an idle lane admits the rewind
// whatever the deed says — the case fate alone cannot cover.
#[tokio::test]
async fn an_idle_lane_opens_the_rewind_selector() {
    use agent::session::Session;

    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    assert!(
        matches!(
            tui.admit(Asked::Own(Deed::Rewind), crate::tui::Origin::Typed, None),
            crate::tui::Wake::Nothing
        ),
        "the run in flight refuses it"
    );
    assert!(tui.ui.flash.is_some(), "and says why");

    // `esc` stopped the run and its job came home: the same lane, idle.
    let mut session = Session::new();
    session.prompt("the first question");
    tui.ui.flash = None;
    let token = tui.core.lanes[0].token();
    tui.settle(crate::tui::run::Done {
        token,
        kind: crate::tui::run::Kind::Turn,
        ran: Some((session, Ok(llm::stream::Usage::default()))),
    })
    .await;

    // `esc esc` on the empty line, as the user presses it: the key has to
    // arrive at the deed before the deed can be admitted.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let esc = || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let armed = tui.ui.key(
        tui.core.lane(),
        view_at(&mut tui.views, token),
        esc(),
        false,
    );
    assert!(
        matches!(&armed, Asked::Own(Deed::Nothing)),
        "the first press only arms it: {armed:?}"
    );
    let asked = tui.ui.key(
        tui.core.lane(),
        view_at(&mut tui.views, token),
        esc(),
        false,
    );
    assert!(
        matches!(&asked, Asked::Own(Deed::Rewind)),
        "the second opens the selector: {asked:?}"
    );

    let crate::tui::Wake::Do(Asked::Own(deed), _) =
        tui.admit(asked, crate::tui::Origin::Typed, None)
    else {
        panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
    };
    tui.carry(deed).await;
    assert_eq!(tui.ui.editor.text(), "/rewind ");
    let offered: Vec<String> = tui.ui.menu().iter().map(|m| m.show().to_string()).collect();
    assert_eq!(
        offered,
        ["the first question"],
        "completing it with what the user said"
    );
    assert!(tui.ui.flash.is_none(), "and nothing was refused");
}

// The transcript rewinds in memory before it is written: a failed write must
// still hand the unsent prompt back, its only copy, and say it was not saved.
#[tokio::test]
async fn an_unsent_prompt_survives_a_failed_write() {
    use agent::session::Session;

    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let mut session = Session::new();
    session.prompt("the first question");
    let token = tui.core.lanes[0].token();
    tui.settle(crate::tui::run::Done {
        token,
        kind: crate::tui::run::Kind::Turn,
        ran: Some((session, Ok(llm::stream::Usage::default()))),
    })
    .await;
    // A file where the store's directory goes: no write gets under it.
    let state = dir.path().join("state");
    let _ = std::fs::remove_dir_all(&state);
    std::fs::write(&state, "").expect("a file in the way");
    tui.ui.flash = None;

    let id = tui.core.lane().last_ask().expect("the prompt");
    tui.rewind_turn(id);

    assert_eq!(tui.ui.editor.text(), "the first question");
    let flash = tui.ui.flash.as_ref().map(|(text, _)| text.as_str());
    assert!(flash.is_some_and(|f| f.contains("not saved")), "{flash:?}");
}

// A panicked job leaves the lane idle with nothing in it (`NO_TRANSCRIPT`).
// `esc esc` still arrives since `fate` only guards a run, not this state.
#[tokio::test]
async fn the_rewind_of_a_lane_with_no_transcript_says_so() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());

    // Nothing came home, and nothing was saved to read back: the session has
    // never been written.
    tui.settle(crate::tui::run::Done {
        token: tui.core.lanes[0].token(),
        kind: crate::tui::run::Kind::Turn,
        ran: None,
    })
    .await;
    tui.ui.flash = None;
    assert!(
        tui.core.lane().session().is_none(),
        "the transcript is gone"
    );

    let crate::tui::Wake::Do(Asked::Own(deed), _) =
        tui.admit(Asked::Own(Deed::Rewind), crate::tui::Origin::Typed, None)
    else {
        panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
    };
    tui.carry(deed).await;

    assert!(tui.ui.editor.is_empty(), "there is nothing to go back to");
}

// `/new`, `/worktree` to an open checkout, and `/loop` (which only arms a
// lane) all answer empty; a dismiss-for-nothing overlay is worse than silence.
#[tokio::test]
async fn an_empty_command_page_is_not_opened() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_slash(Listing::default());
    assert!(tui.ui.read.is_none());
}

// Esc goes to the slash page, not past it: reaching the run would stop a turn
// the user meant to leave alone.
#[tokio::test]
async fn esc_closes_the_command_page_rather_than_reaching_the_run() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_slash(Listing::say(["/help answered this"]));

    let token = tui.core.lane().token();
    let esc = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let asked = tui
        .ui
        .key(tui.core.lane(), view_at(&mut tui.views, token), esc, false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert!(tui.ui.read.is_none(), "the esc closed it");
}

// A command's output and a typo's refusal are dismissed pages, so echoing
// them strands the line above nothing; a prompt is echoed since the turn
// streams under it. Decided by the answer, which only dispatch knows.
#[tokio::test]
async fn only_an_answer_under_the_line_echoes_it() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    // The real table, or `/keys` is a word the door does not know and the
    // line is read as a prompt — which is a different test.
    let table = std::sync::Arc::new(pi_core::input::commands::commands(
        &[],
        crate::tui::WORDS,
        &mut Vec::new(),
    ));
    tui.core.commands = table.clone();
    tui.ui.commands = table;
    let token = tui.core.lane().token();
    let rows = |tui: &mut crate::tui::Tui| view_at(&mut tui.views, token).surface.scrollback.len();
    let enter = || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    for (typed, echoed) in [("/keys", 0), ("/nosuch'", 0), ("what changed?", 1)] {
        let before = rows(&mut tui);
        tui.ui.editor.set_line(typed);
        let lane = tui.core.lane_mut();
        let Asked::Core(intent) = tui
            .ui
            .key(lane, view_at(&mut tui.views, token), enter(), false)
        else {
            panic!("{typed} is read");
        };
        let line = tui.ui.took_submit();
        assert_eq!(line.as_deref(), Some(typed));
        let step = tui.core.dispatch(intent);
        tui.echo_under(&step, line);
        assert_eq!(rows(&mut tui), before + echoed, "{typed}");
    }
}

// The line keeps the keys under a slash page: a letter types on it and the
// page stays up to be read beside it, until `esc` takes it down.
#[tokio::test]
async fn the_line_keeps_the_keys_under_a_command_page() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_slash(Listing::say(["/status answered this"]));
    let token = tui.core.lane().token();
    let press = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));

    let lane = tui.core.lane_mut();
    tui.ui.key(
        lane,
        view_at(&mut tui.views, token),
        press(KeyCode::Char('x')),
        false,
    );
    assert_eq!(tui.ui.editor.text(), "x");
    assert!(tui.ui.read.is_some(), "the page stays up");

    let lane = tui.core.lane_mut();
    tui.ui.key(
        lane,
        view_at(&mut tui.views, token),
        press(KeyCode::Esc),
        false,
    );
    assert!(tui.ui.read.is_none(), "esc takes it down");
    assert_eq!(tui.ui.editor.text(), "x", "and leaves the line be");
}

// Enter sends the line and takes the page down, its answer coming on the
// transcript; `ctrl+c` closes it and, with a run in flight, still stops the run.
#[tokio::test]
async fn enter_and_the_stop_close_a_command_page() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.editor.set_line("what changed?");
    tui.ui.open_slash(Listing::say(["/status answered this"]));

    let token = tui.core.lane().token();
    let enter = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), enter, false);
    assert!(tui.ui.read.is_none(), "Enter closed it");
    assert_eq!(tui.ui.took_submit().as_deref(), Some("what changed?"));

    tui.ui.open_slash(Listing::say(["/status answered this"]));
    let ctrl_c =
        crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let lane = tui.core.lane_mut();
    let asked = tui
        .ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c, true);
    assert!(tui.ui.read.is_none(), "`ctrl+c` took it down too");
    assert!(!matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
}

// The draw cursor must move past a filed row, else the next adopt redraws
// it above the prompt; mid-run, with no entry yet, it waits with the lane.
#[tokio::test]
async fn a_filed_row_moves_the_cursor_an_adopt_goes_by() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lane().token();
    let lane = tui.core.lane_mut();
    let view = view_at(&mut tui.views, token);
    let drawn = view.surface.scrollback.len();

    // The surface lane is running: the transcript is out with the run.
    tui.ui.say_kept(lane, view, "! the reply came back short");
    assert_eq!(view.surface.scrollback.len(), drawn + 1, "drawn now");
    assert_eq!(view.surface.tail, None, "with nothing filed to move past");
    assert_eq!(lane.held(), 1, "it is waiting with the lane");

    // The run comes home, and the next row can be filed as it is drawn.
    lane.return_session(agent::session::Session::default());
    tui.ui.say_kept(lane, view, "! settled");
    assert_eq!(
        view.surface.tail,
        lane.session()
            .and_then(|s| s.entries().last())
            .map(|e| e.id()),
        "the cursor is past the row just drawn"
    );
}

// The completion list stays up during a run (`/help`/`/model` answer on the
// spot, the rest queue); `esc` dismisses it first, innermost, then the run.
#[test]
fn esc_takes_the_list_first_and_the_run_next() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut ui = test_ui(80, 24);
    ui.commands = std::sync::Arc::new(vec![Command {
        word: "/new".into(),
        args: "",
        help: "a fresh session".into(),
        intent: |_, _| Intent::Builtin(Builtin::New),
        source: Source::Builtin,
    }]);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let esc = || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // An empty line raises no list, so one `esc` stops the run — unless
    // uncommitted, where the same key means `Unsend` instead (a different test).
    view.state.committed = true;
    assert!(ui.menu().is_empty(), "an empty line completes to nothing");
    let intent = ui.key(&lane, &mut view, esc(), true);
    assert!(matches!(intent, Asked::Own(Deed::Interrupt)), "{intent:?}");

    ui.editor.set_line("/ne");
    assert!(
        !ui.menu().is_empty(),
        "the word is worth completing mid-run"
    );

    // First press: the list, and nothing asked of the loop.
    let intent = ui.key(&lane, &mut view, esc(), true);
    assert!(matches!(intent, Asked::Own(Deed::Nothing)), "{intent:?}");
    assert!(ui.menu().is_empty(), "the list went");
    // Second: through where the list was, to the run.
    let intent = ui.key(&lane, &mut view, esc(), true);
    assert!(matches!(intent, Asked::Own(Deed::Interrupt)), "{intent:?}");

    // Typing past the dismissal brings the list back, and Tab still
    // completes mid-run — the half of the menu the run never claimed.
    ui.editor.set_line("/n");
    let tab = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    ui.key(&lane, &mut view, tab, true);
    assert_eq!(
        ui.editor.text(),
        "/new",
        "the half-typed word was completed"
    );
}

// A `ctrl+c` that closed something is not the first of a quit: two quick
// presses to put a slash page away must not leave the app.
#[tokio::test]
async fn the_stop_that_closed_a_command_page_is_not_half_a_quit() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_slash(Listing::say(["/status answered this"]));
    let token = tui.core.lane().token();
    let ctrl_c =
        || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c(), false);
    assert!(tui.ui.read.is_none(), "the first closed it");
    let lane = tui.core.lane_mut();
    let asked = tui
        .ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c(), false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert!(tui.ui.flash.is_some(), "the second only warns");
}

// Esc on the line `esc esc` put there, untouched, takes it back; an edited one
// stays. The `esc` that does it is not the first of another.
#[tokio::test]
async fn esc_takes_back_the_rewind_line_it_was_given() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pi_core::input::commands::Choice;
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.said = vec![Choice {
        name: "1".into(),
        note: "the first question".into(),
    }];
    let token = tui.core.lane().token();
    let press = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let key = |tui: &mut crate::tui::Tui, code| {
        let lane = tui.core.lane_mut();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), press(code), false)
    };

    tui.ui.seed("/rewind ");
    for n in 1..=2 {
        let asked = key(&mut tui, KeyCode::Esc);
        assert!(
            matches!(asked, Asked::Own(Deed::Nothing)),
            "esc {n}: {asked:?}"
        );
        assert!(tui.ui.editor.is_empty(), "esc {n}");
    }

    tui.ui.seed("/rewind ");
    key(&mut tui, KeyCode::Char('f'));
    key(&mut tui, KeyCode::Esc);
    assert_eq!(tui.ui.editor.text(), "/rewind f");
}

// The `esc` that closed a slash page is not the first of `esc esc`: a quick
// second press on the empty line must not open the rewind selector.
#[tokio::test]
async fn the_esc_that_closed_a_command_page_is_not_half_a_rewind() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_slash(Listing::say(["/status answered this"]));
    let token = tui.core.lane().token();
    let esc = || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    for n in 1..=2 {
        let lane = tui.core.lane_mut();
        let asked = tui
            .ui
            .key(lane, view_at(&mut tui.views, token), esc(), false);
        assert!(
            matches!(asked, Asked::Own(Deed::Nothing)),
            "esc {n}: {asked:?}"
        );
    }
}

// A question `choose` put is answered in a press or two: the highlighted
// option or a numbered one goes as if typed; `q` or esc closes it.
#[tokio::test]
async fn a_question_is_answered_by_its_options_or_closed() {
    use pi_core::core::question::{Asker, Question};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lanes[0].token();
    let call = |tui: &mut crate::tui::Tui, id: u64| {
        let from = Asker::Tool {
            name: "choose".into(),
        };
        let options = ["/compact".to_string(), "fix parse.rs".to_string()];
        let mut q = Question::new(from, "tests failed, now what?", &options, false).unwrap();
        q.id = id;
        view_at(&mut tui.views, token).ask(q);
    };
    let press = |tui: &mut crate::tui::Tui, event| {
        let lane = tui.core.lane();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), event, false)
    };
    let waiting = |tui: &mut crate::tui::Tui| view_at(&mut tui.views, token).questions.len();
    let shown = |tui: &mut crate::tui::Tui| -> Vec<String> {
        let view = view_at(&mut tui.views, token);
        let question = view.questions.front().expect("a question up").clone();
        (tui.ui
            .question_lines(
                &question,
                &view.checked,
                !view.writing && tui.ui.line_bare(),
                80,
            )
            .iter())
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));

    call(&mut tui, 2);
    assert_eq!(
        shown(&mut tui),
        [
            "? choose · tests failed, now what? · j/k move · enter send · i write · q close",
            "  1 /compact",
            "  2 fix parse.rs"
        ]
    );

    press(&mut tui, typed('j'));
    let asked = press(&mut tui, key(KeyCode::Enter));
    // Read as the line is: this surface knows no commands, so it is a prompt.
    assert!(
        matches!(&asked, Asked::Core(Intent::Prompt(p)) if p == "fix parse.rs"),
        "{asked:?}"
    );
    assert_eq!(
        tui.ui.took_submit().as_deref(),
        Some("fix parse.rs"),
        "sent as typed"
    );
    assert_eq!(waiting(&mut tui), 0);

    call(&mut tui, 3);
    assert!(matches!(
        press(&mut tui, typed('9')),
        Asked::Own(Deed::Nothing)
    ));
    assert_eq!(waiting(&mut tui), 1, "no ninth option, nothing picked");
    press(&mut tui, typed('1'));
    assert_eq!(tui.ui.took_submit().as_deref(), Some("/compact"));
    assert!(tui.ui.editor.is_empty(), "the digit never reached the line");

    call(&mut tui, 4);
    assert!(matches!(
        press(&mut tui, key(KeyCode::Esc)),
        Asked::Own(Deed::Nothing)
    ));
    assert_eq!(waiting(&mut tui), 0);
    assert!(tui.ui.took_submit().is_none());

    // A stray key neither closes it nor types under it; `q` closes it.
    call(&mut tui, 5);
    for stray in ['x', 'y', ' '] {
        assert!(matches!(
            press(&mut tui, typed(stray)),
            Asked::Own(Deed::Nothing)
        ));
    }
    assert!(tui.ui.editor.is_empty(), "nothing typed under it");
    assert_eq!(waiting(&mut tui), 1, "and still up");
    press(&mut tui, typed('q'));
    assert_eq!(waiting(&mut tui), 0);
    assert!(tui.ui.took_submit().is_none());

    // `i` hands the line back: their own words sent close the question, and
    // a line emptied again returns to it. The caret says which has the keys.
    call(&mut tui, 7);
    let drawn = |tui: &mut crate::tui::Tui| {
        let lane = tui.core.lane();
        tui.ui.flush(lane, view_at(&mut tui.views, token));
        tui.ui.question_holds
    };
    assert!(drawn(&mut tui), "the question holds the keyboard");
    press(&mut tui, typed('i'));
    assert!(!drawn(&mut tui), "the line has it back");
    assert!(
        !shown(&mut tui)[0].contains("send"),
        "no keys offered while writing"
    );
    press(&mut tui, typed('o'));
    press(&mut tui, key(KeyCode::Backspace));
    assert!(
        tui.ui.asking(view_at(&mut tui.views, token)),
        "back at the question"
    );
    press(&mut tui, typed('i'));
    for c in "no".chars() {
        press(&mut tui, typed(c));
    }
    assert_eq!(tui.ui.editor.text(), "no", "letters type while writing");
    let asked = press(&mut tui, key(KeyCode::Enter));
    assert!(
        matches!(&asked, Asked::Core(Intent::Prompt(p)) if p == "no"),
        "{asked:?}"
    );
    assert_eq!(waiting(&mut tui), 0, "their own answer closes it");
    tui.ui.took_submit();

    tui.ui.editor.insert_str("x");
    call(&mut tui, 6);
    press(&mut tui, typed('1'));
    assert_eq!(
        tui.ui.editor.text(),
        "x1",
        "a line being typed keeps its digit"
    );
    assert!(
        !shown(&mut tui)[0].contains("send"),
        "no keys offered that would type"
    );

    // One asker, one question: asking again replaces; a withdrawn one goes.
    let job = |text: &str| {
        let from = Asker::Job {
            id: 3,
            name: "judge #3".into(),
        };
        let mut q = Question::new(from, text, &["/new".into()], false).unwrap();
        q.id = 7;
        q
    };
    let view = view_at(&mut tui.views, token);
    view.ask(job("first?"));
    view.ask(job("second?"));
    assert_eq!(view.questions.len(), 2, "the script's and the job's");
    assert_eq!(view.questions[1].text, "second?", "replaced where it stood");
    assert_eq!(
        view.questions[0].from,
        Asker::Tool {
            name: "choose".into()
        },
        "the front stays in front"
    );
    view.withdraw(7);
    assert_eq!(view.questions.len(), 1);
    assert_eq!(
        view.questions[0].from,
        Asker::Tool {
            name: "choose".into()
        }
    );

    // Asked again with fewer options, the highlight starts over.
    view.questions.clear();
    let three = Question::new(
        Asker::Tool {
            name: "choose".into(),
        },
        "which?",
        &["a".into(), "b".into(), "c".into()],
        false,
    );
    view.ask(three.unwrap());
    view.picked = 2;
    view.ask(
        Question::new(
            Asker::Tool {
                name: "choose".into(),
            },
            "which?",
            &["a".into()],
            false,
        )
        .unwrap(),
    );
    assert_eq!(view.picked, 0);
}

// Where several may be picked, picks are gathered and sent together as one
// prompt, a line each; with none gathered, enter sends the highlighted one.
#[tokio::test]
async fn several_picks_go_together_as_one_prompt() {
    use pi_core::core::question::{Asker, Question};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lanes[0].token();
    let call = |tui: &mut crate::tui::Tui, id: u64, options: &[&str]| {
        let from = Asker::Tool {
            name: "choose".into(),
        };
        let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
        let mut q = Question::new(from, "which first?", &options, true).unwrap();
        q.id = id;
        view_at(&mut tui.views, token).ask(q);
    };
    let press = |tui: &mut crate::tui::Tui, event| {
        let lane = tui.core.lane();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), event, false)
    };
    let shown = |tui: &mut crate::tui::Tui| -> Vec<String> {
        let view = view_at(&mut tui.views, token);
        let question = view.questions.front().expect("a question up").clone();
        (tui.ui
            .question_lines(
                &question,
                &view.checked,
                !view.writing && tui.ui.line_bare(),
                80,
            )
            .iter())
        .map(|r| r.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let enter = || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    call(&mut tui, 1, &["fix parse.rs", "add tests", "update docs"]);
    press(&mut tui, typed(' '));
    press(&mut tui, typed('3'));
    assert_eq!(
        shown(&mut tui),
        [
            "? choose · which first? · j/k move · space mark · enter send · i write · q close",
            "  1 [x] fix parse.rs",
            "  2 [ ] add tests",
            "  3 [x] update docs",
        ]
    );
    assert!(
        tui.ui.took_submit().is_none(),
        "a digit gathers, it does not send"
    );
    let asked = press(&mut tui, enter());
    assert!(
        matches!(&asked, Asked::Core(Intent::Prompt(p)) if p == "fix parse.rs\nupdate docs"),
        "{asked:?}"
    );

    call(&mut tui, 2, &["fix parse.rs", "add tests"]);
    press(&mut tui, typed(' '));
    press(&mut tui, typed(' '));
    assert!(
        view_at(&mut tui.views, token).checked.is_empty(),
        "space twice is out again"
    );
    press(
        &mut tui,
        crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
    );
    press(&mut tui, enter());
    assert_eq!(
        tui.ui.took_submit().as_deref(),
        Some("add tests"),
        "none gathered: the highlighted"
    );
}

// A question its asker waits on is answered back, never sent: the pick, the
// person's own words, or none when closed; and it is never replaced.
#[tokio::test]
async fn an_answer_goes_back_to_an_asker_that_waits() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pi_core::core::question::{Asker, Question};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lanes[0].token();
    let waits = |id: u64| {
        let from = Asker::Tool {
            name: "guard".into(),
        };
        let mut q = Question::new(
            from,
            "run rm -rf build?",
            &["allow".into(), "deny".into()],
            false,
        )
        .unwrap();
        (q.id, q.back) = (id, true);
        q
    };
    let press = |tui: &mut crate::tui::Tui, event| {
        let lane = tui.core.lane();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), event, false)
    };
    let key = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));

    let view = view_at(&mut tui.views, token);
    view.ask(waits(1));
    view.ask(waits(2));
    assert_eq!(view.questions.len(), 2, "one that waits is never replaced");

    press(&mut tui, typed('j'));
    let asked = press(&mut tui, key(KeyCode::Enter));
    assert!(
        matches!(&asked, Asked::Own(d) if *d == Deed::Answer { question: 1, picked: Some("deny".into()) }),
        "{asked:?}"
    );
    assert!(tui.ui.took_submit().is_none(), "nothing went out as typed");

    press(&mut tui, typed('i'));
    for c in "only build/".chars() {
        press(&mut tui, typed(c));
    }
    let asked = press(&mut tui, key(KeyCode::Enter));
    assert!(
        matches!(&asked, Asked::Own(d) if *d == Deed::Answer { question: 2, picked: Some("only build/".into()) }),
        "{asked:?}"
    );
    assert!(
        tui.ui.took_submit().is_none(),
        "their words went back, not out"
    );

    let view = view_at(&mut tui.views, token);
    view.ask(waits(3));
    view.ask(waits(4));
    view.withdraw(3);
    assert_eq!(view.questions.len(), 1);
    let asked = press(&mut tui, typed('q'));
    assert!(
        matches!(&asked, Asked::Own(d) if *d == Deed::Answer { question: 4, picked: None }),
        "{asked:?}"
    );
}
