use crate::tui::ui::Focus;
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
    assert!(matches!(
        Deed::To(agent::session::EntryId(1)).fate(),
        Fate::Refused(_)
    ));
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
    tui.settle(crate::tui::job::Done {
        token,
        kind: crate::tui::job::Kind::Turn,
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

    let crate::tui::Wake::Do(Asked::Own(deed)) = tui.admit(asked, crate::tui::Origin::Typed, None)
    else {
        panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
    };
    tui.carry(deed).await;
    assert!(
        matches!(tui.ui.focus, Focus::Rewind(_)),
        "the selector opened on what the user said"
    );
    assert!(tui.ui.flash.is_none(), "and nothing was refused");
}

// A panicked job leaves the lane idle with nothing in it (`NO_TRANSCRIPT`).
// `esc esc` still arrives since `fate` only guards a run, not this state.
#[tokio::test]
async fn the_rewind_of_a_lane_with_no_transcript_says_so() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());

    // Nothing came home, and nothing was saved to read back: the session has
    // never been written.
    tui.settle(crate::tui::job::Done {
        token: tui.core.lanes[0].token(),
        kind: crate::tui::job::Kind::Turn,
        ran: None,
    })
    .await;
    tui.ui.flash = None;
    assert!(
        tui.core.lane().session().is_none(),
        "the transcript is gone"
    );

    let crate::tui::Wake::Do(Asked::Own(deed)) =
        tui.admit(Asked::Own(Deed::Rewind), crate::tui::Origin::Typed, None)
    else {
        panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
    };
    tui.carry(deed).await;

    assert!(
        !matches!(tui.ui.focus, Focus::Rewind(_)),
        "there is nothing to go back to"
    );
}

// `/new`, `/worktree` to an open checkout, and `/loop` (which only arms a
// lane) all answer empty; a dismiss-for-nothing overlay is worse than silence.
#[tokio::test]
async fn an_empty_reply_is_not_opened() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::default());
    assert!(!matches!(tui.ui.focus, Focus::Reply(_)));
}

// Esc goes to the reply, not past it: reaching the run would stop a turn
// the user meant to leave alone.
#[tokio::test]
async fn esc_closes_the_reply_rather_than_reaching_the_run() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::say(["/help answered this"]));

    let token = tui.core.lane().token();
    let esc = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let asked = tui
        .ui
        .key(tui.core.lane(), view_at(&mut tui.views, token), esc, false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert!(
        !matches!(tui.ui.focus, Focus::Reply(_)),
        "the esc closed it"
    );
}

// A command's answer is a dismissed reply, so echoing it strands the line
// above nothing; a prompt is echoed since the turn streams under it.
#[tokio::test]
async fn a_command_is_not_echoed_and_a_prompt_is() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    // The real table, or `/keys` is a word the door does not know and the
    // line is read as a prompt — which is a different test.
    let table = std::sync::Arc::new(pi_core::input::commands::commands(&[], &mut Vec::new()));
    tui.core.commands = table.clone();
    tui.ui.commands = table;
    let token = tui.core.lane().token();
    let rows = |tui: &mut crate::tui::Tui| view_at(&mut tui.views, token).surface.scrollback.len();
    let enter = || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    let before = rows(&mut tui);
    tui.ui.editor.set_line("/keys");
    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), enter(), false);
    assert_eq!(rows(&mut tui), before, "a command leaves no row");

    let before = rows(&mut tui);
    tui.ui.editor.set_line("what changed?");
    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), enter(), false);
    assert_eq!(
        rows(&mut tui),
        before + 1,
        "a prompt is one, answered under"
    );
}

// A reply has the keyboard: one place takes keys at a time, so a stray
// letter neither types into the hidden line nor takes the reply down.
#[tokio::test]
async fn a_reply_has_the_keyboard_until_closed() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::say(["/status answered this"]));
    let token = tui.core.lane().token();
    let press = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));

    for code in [KeyCode::Down, KeyCode::Char('j'), KeyCode::Char('x')] {
        let lane = tui.core.lane_mut();
        let asked = tui
            .ui
            .key(lane, view_at(&mut tui.views, token), press(code), false);
        assert!(
            matches!(tui.ui.focus, Focus::Reply(_)),
            "{code:?} is the reply's"
        );
        assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    }
    assert!(tui.ui.editor.is_empty(), "nothing typed behind it");

    let lane = tui.core.lane_mut();
    tui.ui.key(
        lane,
        view_at(&mut tui.views, token),
        press(KeyCode::Char('q')),
        false,
    );
    assert!(!matches!(tui.ui.focus, Focus::Reply(_)), "`q` closes it");
}

// Enter closes a reply without sending the line behind it; `ctrl+c` closes
// it and, with a run in flight, still stops the run.
#[tokio::test]
async fn enter_and_the_stop_close_a_reply() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.editor.set_line("/help");
    tui.ui.open_reply(Listing::say(["/status answered this"]));

    let token = tui.core.lane().token();
    let enter = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), enter, false);
    assert!(!matches!(tui.ui.focus, Focus::Reply(_)), "Enter closed it");
    assert!(!tui.ui.took_submit(), "and sent nothing");
    assert_eq!(tui.ui.editor.text(), "/help", "the line waits");

    tui.ui.open_reply(Listing::say(["/status answered this"]));
    let ctrl_c =
        crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let lane = tui.core.lane_mut();
    let asked = tui
        .ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c, true);
    assert!(
        !matches!(tui.ui.focus, Focus::Reply(_)),
        "`ctrl+c` took it down too"
    );
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
    tui.ui
        .file_screen(lane, view, "! the reply came back short");
    assert_eq!(view.surface.scrollback.len(), drawn + 1, "drawn now");
    assert_eq!(view.surface.tail, None, "with nothing filed to move past");
    assert_eq!(lane.held_screens().len(), 1, "it is waiting with the lane");

    // The run comes home, and the next row can be filed as it is drawn.
    lane.return_session(agent::session::Session::default());
    tui.ui.file_screen(lane, view, "! settled");
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
// presses to put a reply away must not leave the app.
#[tokio::test]
async fn the_stop_that_closed_a_reply_is_not_half_a_quit() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::say(["/status answered this"]));
    let token = tui.core.lane().token();
    let ctrl_c =
        || crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c(), false);
    assert!(matches!(tui.ui.focus, Focus::Editor), "the first closed it");
    let lane = tui.core.lane_mut();
    let asked = tui
        .ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c(), false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert!(tui.ui.flash.is_some(), "the second only warns");
}

// The rewind selector has the keyboard too: a letter neither types nor
// closes it, and the `esc` that closes it is not the first of another.
#[tokio::test]
async fn the_rewind_selector_has_the_keyboard() {
    use crate::tui::menu::MenuEntry;
    use agent::session::EntryId;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_rewind(vec![MenuEntry::Message {
        id: EntryId(1),
        show: "the first question".into(),
    }]);
    let token = tui.core.lane().token();
    let press = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));

    let lane = tui.core.lane_mut();
    tui.ui.key(
        lane,
        view_at(&mut tui.views, token),
        press(KeyCode::Char('x')),
        false,
    );
    assert!(
        matches!(tui.ui.focus, Focus::Rewind(_)),
        "a letter is swallowed"
    );
    assert!(tui.ui.editor.is_empty());

    for (n, expect_open) in [(1, false), (2, false)] {
        let lane = tui.core.lane_mut();
        let asked = tui.ui.key(
            lane,
            view_at(&mut tui.views, token),
            press(KeyCode::Esc),
            false,
        );
        assert!(
            matches!(asked, Asked::Own(Deed::Nothing)),
            "esc {n}: {asked:?}"
        );
        assert_eq!(matches!(tui.ui.focus, Focus::Rewind(_)), expect_open);
    }
}

// The `esc` that closed a reply is not the first of `esc esc`: a quick
// second press on the empty line must not open the rewind selector.
#[tokio::test]
async fn the_esc_that_closed_a_reply_is_not_half_a_rewind() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::say(["/status answered this"]));
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
