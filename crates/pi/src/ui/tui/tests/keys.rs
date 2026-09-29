use crate::input::commands::{Command, Source};
use crate::input::{Builtin, Fate};
use crate::store::listing::Listing;
use crate::ui::tui::{Asked, Deed, Intent, View, view_at};

use super::harness::*;

// Both scrollback producers draw block ids from one counter. They used
// not to: a rebuilt block was always `0`, which held only while nothing
// looked one up — and `streaming_row` and `stream_fold` both do, taking the
// last match, so two blocks sharing a number is two blocks the lookup
// cannot tell apart.
#[test]
fn a_deed_says_whether_a_run_in_flight_allows_it() {
    // Only the two that rewrite the transcript care: the run in flight is
    // writing it. The rest are the screen's own and go through whenever
    // they are asked for.
    assert!(matches!(Deed::Rewind.fate(), Fate::Refused(_)));
    assert!(matches!(
        Deed::To(agent::session::EntryId(1)).fate(),
        Fate::Refused(_)
    ));
    for deed in [Deed::Nothing, Deed::External, Deed::Interrupt, Deed::Unsend] {
        assert!(matches!(deed.fate(), Fate::Now), "{deed:?} should proceed");
    }
}

// The other half of that answer, and the one the deed cannot give itself:
// `fate` is about a run in flight, so a lane with none admits the rewind
// whatever the deed says. Consulting it first refused every rewind there
// was — with the wording for a run that is not there.
#[tokio::test]
async fn an_idle_lane_opens_the_rewind_selector() {
    use agent::session::Session;

    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    assert!(
        matches!(
            tui.admit(Asked::Own(Deed::Rewind), crate::ui::tui::Origin::Typed),
            crate::ui::tui::Wake::Nothing
        ),
        "the run in flight refuses it"
    );
    assert!(tui.ui.flash.is_some(), "and says why");

    // `esc` stopped the run and its job came home: the same lane, idle.
    let mut session = Session::new();
    session.prompt("the first question");
    tui.ui.flash = None;
    let token = tui.core.lanes[0].token();
    tui.settle(crate::ui::tui::job::Done {
        token,
        kind: crate::ui::tui::job::Kind::Turn,
        ran: Some((session, Ok(llm::stream::Usage::default()))),
    })
    .await;

    // `esc esc` on the empty line, as the user presses it: the key has to
    // arrive at the deed before the deed can be admitted.
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let esc = || crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
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

    let crate::ui::tui::Wake::Do(Asked::Own(deed)) =
        tui.admit(asked, crate::ui::tui::Origin::Typed)
    else {
        panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
    };
    tui.carry(deed).await;
    assert!(
        !tui.ui.rewind.is_empty(),
        "the selector opened on what the user said"
    );
    assert!(tui.ui.flash.is_none(), "and nothing was refused");
}

// A job that panicked and could not read its transcript back leaves the lane
// idle with nothing in it — the state `NO_TRANSCRIPT` names, and one the run
// in flight is not there to explain. `esc esc` still arrives, because `fate`
// only guards a run, so the answer has to be a thing said rather than a panic.
#[tokio::test]
async fn the_rewind_of_a_lane_with_no_transcript_says_so() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());

    // Nothing came home, and nothing was saved to read back: the session has
    // never been written.
    tui.settle(crate::ui::tui::job::Done {
        token: tui.core.lanes[0].token(),
        kind: crate::ui::tui::job::Kind::Turn,
        ran: None,
    })
    .await;
    tui.ui.flash = None;
    assert!(
        tui.core.lane().session().is_none(),
        "the transcript is gone"
    );

    let crate::ui::tui::Wake::Do(Asked::Own(deed)) =
        tui.admit(Asked::Own(Deed::Rewind), crate::ui::tui::Origin::Typed)
    else {
        panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
    };
    tui.carry(deed).await;

    assert!(tui.ui.rewind.is_empty(), "there is nothing to go back to");
}

// A command that answered with nothing opens nothing: `/new`, `/worktree`
// to a checkout already open and the `/loop` that only arms a lane all come
// through here empty, and an overlay the user has to dismiss for nothing is
// worse than silence.
#[tokio::test]
async fn an_empty_reply_is_not_opened() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::default());
    assert!(tui.ui.reply.is_none());
}

// A reply is closed the way the menu's other lists are, and the esc goes
// to it rather than past it: an esc that reached the run would stop a turn
// the user meant to leave alone.
#[tokio::test]
async fn esc_closes_the_reply_rather_than_reaching_the_run() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::say(["/help answered this"]));

    let token = tui.core.lane().token();
    let esc = crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let asked = tui
        .ui
        .key(tui.core.lane(), view_at(&mut tui.views, token), esc, false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert!(tui.ui.reply.is_none(), "the esc closed it");
}

// A command is not echoed onto the screen: its answer is the reply over the
// menu, which is dismissed rather than kept, so the line would be left
// above an answer that never comes. A line that opens a turn still is —
// the turn streams its rows under it, and a line nobody can see they sent
// is one they send twice.
#[tokio::test]
async fn a_command_is_not_echoed_and_a_prompt_is() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    // The real table, or `/keys` is a word the door does not know and the
    // line is read as a prompt — which is a different test.
    let table = std::sync::Arc::new(crate::input::commands::commands(&[], &mut Vec::new()));
    tui.core.commands = table.clone();
    tui.ui.commands = table;
    let token = tui.core.lane().token();
    let rows =
        |tui: &mut crate::ui::tui::Tui| view_at(&mut tui.views, token).surface.scrollback.len();
    let enter =
        || crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

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

// A reply is an answer over the menu, not a mode over the keyboard: it
// reads the menu's own keys and nothing else, so a letter typed over it
// takes it down on the way past and lands in the editor. A letter it read
// for itself would be a letter missing from the line being typed.
#[tokio::test]
async fn a_reply_reads_the_menus_keys_and_yields_everything_else() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.open_reply(Listing::say(["/status answered this"]));
    let token = tui.core.lane().token();

    // The menu's own: the window moves and the reply stays up.
    let down = crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), down, false);
    assert!(tui.ui.reply.is_some(), "a menu key is the reply's");

    // Not the menu's: a letter meant for the next line comes down here, and
    // is left for the caller to read as what it was typed as.
    let j = crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    let lane = tui.core.lane_mut();
    let asked = tui.ui.key(lane, view_at(&mut tui.views, token), j, false);
    assert!(tui.ui.reply.is_none(), "the letter took it down");
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
}

// Enter is not the reply's to swallow: the line is sent, and the answer the
// reply was showing belongs to the line before it. The same for `ctrl+c`,
// which is the stop-the-run key whatever is drawn over the editor — a
// refusal opens a reply while a turn is in flight, and a run that cannot be
// stopped because a refusal is on screen is worse than the refusal.
#[tokio::test]
async fn the_line_and_the_stop_are_not_the_replys_to_take() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.ui.editor.set_line("/help");
    tui.ui.open_reply(Listing::say(["/status answered this"]));

    let token = tui.core.lane().token();
    let enter = crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let lane = tui.core.lane_mut();
    tui.ui.key(
        lane,
        view_at(&mut tui.views, token),
        enter,
        /* running */ false,
    );
    assert!(tui.ui.reply.is_none(), "the line took it down");
    assert!(tui.ui.took_submit(), "and the line it was written for went");
    assert!(tui.ui.editor.is_empty(), "taken off the line");

    tui.ui.open_reply(Listing::say(["/status answered this"]));
    let ctrl_c =
        crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let lane = tui.core.lane_mut();
    tui.ui
        .key(lane, view_at(&mut tui.views, token), ctrl_c, true);
    assert!(tui.ui.reply.is_none(), "`ctrl+c` took it down too");
}

// A row filed for the screen is drawn as it is filed, so the cursor that
// says what this surface has drawn has to move past the entry. Left where
// it was, the next turn's adopt draws the row a second time, above the
// prompt it is answering. And while a run holds the transcript there is no
// entry to move past: the row waits with the lane instead.
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

// The completion list stays up during a run — `/help` and `/model` answer
// on the spot then, and the rest queue as what they are. `esc` is the one
// key it costs, and it costs it for a press: innermost first, so the list
// goes and the next `esc` reaches the run.
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
    let esc = || crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    // Nothing typed raises no list, so the common way to stop a run is one
    // press, as the running row says it is. Committed, or an empty line
    // would mean `Unsend` — a different answer to the same key, and not
    // the one this is about.
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
    let tab = crate::ui::tui::TermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    ui.key(&lane, &mut view, tab, true);
    assert_eq!(
        ui.editor.text(),
        "/new",
        "the half-typed word was completed"
    );
}
