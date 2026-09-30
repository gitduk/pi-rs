use crate::tui::row::Row;
use crate::tui::screen::{self};
use crate::tui::{Asked, Deed, Intent, View, view_at};
use pi_core::input::Builtin;
use pi_core::store::keys::Mode;

use super::harness::*;

// The scratch file lives in shared /tmp and carries whatever the user
// was about to say, so it must not be readable by anyone else.
#[test]
fn the_scratch_file_is_private() {
    let path = crate::tui::term::scratch_file("hello").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
    }
    std::fs::remove_file(&path).unwrap();
}

// The @ completion rides the same menu: Tab lands the path, Enter lands
// it and stays — the prompt is not sent until the user says so.
#[test]
fn an_at_token_completes_by_tab_and_stays_on_enter() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code: KeyCode| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let mut ui = test_ui(80, 24);
    let (dir, lane) = a_running_lane();
    let mut view = View::default();
    std::fs::write(dir.path().join("at_probe.rs"), "").unwrap();
    std::fs::write(dir.path().join("at_probe2.rs"), "").unwrap();
    std::fs::create_dir(dir.path().join("probe_dir")).unwrap();
    ui.at_root = dir.path().to_path_buf();

    ui.editor.set_line("look at @at_pro");
    let intent = ui.key(&lane, &mut view, key(KeyCode::Tab), false);
    assert_eq!(ui.editor.text(), "look at @at_probe.rs ");
    assert!(matches!(intent, Asked::Own(Deed::Nothing)));

    // A directory keeps completing: no space, so the walk can descend.
    ui.editor.set_line("in @probe_d");
    ui.key(&lane, &mut view, key(KeyCode::Tab), false);
    assert_eq!(ui.editor.text(), "in @probe_dir/");

    // Enter with the list open applies the path instead of sending.
    ui.editor.set_line("look at @at_pro");
    let intent = ui.key(&lane, &mut view, key(KeyCode::Enter), false);
    assert_eq!(ui.editor.text(), "look at @at_probe.rs ");
    assert!(matches!(intent, Asked::Own(Deed::Nothing)));

    // A changed query re-anchors the highlight on the best row: the
    // stale index sat on the worse of the two matches above.
    ui.editor.set_line("look at @at_probe");
    ui.picked = Some(0);
    ui.key(&lane, &mut view, key(KeyCode::Tab), false);
    assert_eq!(ui.editor.text(), "look at @at_probe.rs ");
}

// The @ walk follows the checkout in front: a lane on another worktree
// completes its own files, not the first lane's.
#[test]
fn an_at_token_completes_against_the_lane_in_front() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code: KeyCode| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let was = tempfile::tempdir().unwrap();
    let now = tempfile::tempdir().unwrap();
    std::fs::write(now.path().join("marker.rs"), "").unwrap();
    let mut tui = surface(was.path());
    switch_to(&mut tui, running_lane(now.path()));

    assert_eq!(tui.ui.at_root, now.path());
    tui.ui.editor.set_line("see @mar");
    let token = tui.core.lane().token();
    tui.ui.key(
        tui.core.lane(),
        view_at(&mut tui.views, token),
        key(KeyCode::Tab),
        false,
    );
    assert_eq!(tui.ui.editor.text(), "see @marker.rs ");
}

// The sequence is read where unbound characters are typed, and its first
// half is a real `j` on a real line until the `k` arrives — nothing is
// held pending, so the screen is never a guess.
#[test]
fn jk_leaves_insert_and_takes_its_first_half_back_off_the_line() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();

    ui.key(&lane, &mut view, typed('j'), false);
    assert_eq!(ui.editor.text(), "j", "a lone j is a j");
    assert_eq!(mode(&ui), Some(Mode::Insert));

    ui.key(&lane, &mut view, typed('k'), false);
    assert_eq!(ui.editor.text(), "", "the j goes with the mode change");
    assert_eq!(mode(&ui), Some(Mode::Normal));
}

// Outside the window the two characters are just two characters. Without
// this, a `j` typed minutes ago would still be armed.
#[test]
fn a_j_left_behind_does_not_arm_a_later_k() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();

    ui.key(&lane, &mut view, typed('j'), false);
    let stale = std::time::Instant::now() - std::time::Duration::from_secs(1);
    ui.vim.as_mut().unwrap().last = Some(('j', stale));
    ui.key(&lane, &mut view, typed('k'), false);

    assert_eq!(ui.editor.text(), "jk");
    assert_eq!(mode(&ui), Some(Mode::Insert));
}

// A command between the halves breaks the sequence: `j`, a keystroke that
// means something, then `k` is two commands and a `j`, not a mode change.
#[test]
fn a_bound_key_between_the_halves_breaks_the_sequence() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();

    ui.key(&lane, &mut view, typed('j'), false);
    let left = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    ui.key(&lane, &mut view, left, false);
    ui.key(&lane, &mut view, typed('k'), false);

    assert_eq!(ui.editor.text(), "kj", "the caret had moved before the k");
    assert_eq!(mode(&ui), Some(Mode::Insert));
}

// Normal has to refuse the keys it does not bind. Without this the mode
// is a costume: `z` would still type a `z` and only the bound keys would
// behave, which is worse than no mode at all.
#[test]
fn an_unbound_character_types_nothing_in_normal() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("hello");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('z'), false);
    assert_eq!(ui.editor.text(), "hello");

    // And the keys it does bind still command.
    ui.key(&lane, &mut view, typed('0'), false);
    ui.key(&lane, &mut view, typed('x'), false);
    assert_eq!(ui.editor.text(), "ello");
}

// The way back, and what `a` does that `i` does not.
#[test]
fn i_and_a_return_to_insert_on_either_side_of_the_caret() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("ab");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('0'), false);
    ui.key(&lane, &mut view, typed('i'), false);
    assert_eq!(mode(&ui), Some(Mode::Insert));
    ui.key(&lane, &mut view, typed('Z'), false);
    assert_eq!(ui.editor.text(), "Zab", "i types where the caret is");

    ui.vim.as_mut().unwrap().mode = Mode::Normal;
    ui.key(&lane, &mut view, typed('0'), false);
    ui.key(&lane, &mut view, typed('a'), false);
    ui.key(&lane, &mut view, typed('Y'), false);
    assert_eq!(ui.editor.text(), "ZYab", "a types past it");
}

// `x` and `D` delete and stay in Normal; these delete the same ranges and
// leave. The landing is the whole difference, so it is asserted twice.
#[test]
fn s_and_c_delete_their_range_and_land_in_insert() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("abcd");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('0'), false);
    ui.key(&lane, &mut view, typed('s'), false);
    assert_eq!(ui.editor.text(), "bcd");
    assert_eq!(mode(&ui), Some(Mode::Insert));
    ui.key(&lane, &mut view, typed('Z'), false);
    assert_eq!(ui.editor.text(), "Zbcd", "s types where the character was");

    ui.vim.as_mut().unwrap().mode = Mode::Normal;
    ui.key(&lane, &mut view, typed('0'), false);
    let right = crate::tui::TermEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Right,
        crossterm::event::KeyModifiers::NONE,
    ));
    ui.key(&lane, &mut view, right, false);
    ui.key(&lane, &mut view, typed('C'), false);
    assert_eq!(ui.editor.text(), "Z");
    assert_eq!(mode(&ui), Some(Mode::Insert));
    ui.key(&lane, &mut view, typed('Y'), false);
    assert_eq!(ui.editor.text(), "ZY", "C leaves the caret where it cut");
}

// `S` and `cc` clear the line and land in Insert.
#[test]
fn s_and_cc_change_the_line_without_removing_it() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("ab\ncd\nef");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.editor.buffer_start();
    ui.editor.down();
    ui.key(&lane, &mut view, typed('S'), false);
    assert_eq!(mode(&ui), Some(Mode::Insert));
    assert_eq!(ui.editor.text(), "ab\n\nef");
    ui.key(&lane, &mut view, typed('Z'), false);
    assert_eq!(ui.editor.text(), "ab\nZ\nef");

    ui.vim.as_mut().unwrap().mode = Mode::Normal;
    ui.editor.down();
    ui.key(&lane, &mut view, typed('c'), false);
    ui.key(&lane, &mut view, typed('c'), false);
    assert_eq!(mode(&ui), Some(Mode::Insert));
    assert_eq!(ui.editor.text(), "ab\nZ\n");
    ui.key(&lane, &mut view, typed('Y'), false);
    assert_eq!(ui.editor.text(), "ab\nZ\nY");
}

// `o` and `O` open a line on either side of the caret's and land in
// Insert, which is where the typing goes.
#[test]
fn o_and_o_open_a_line_and_land_in_insert() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("ab\ncd");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('O'), false);
    assert_eq!(mode(&ui), Some(Mode::Insert));
    ui.key(&lane, &mut view, typed('Z'), false);
    assert_eq!(
        ui.editor.text(),
        "ab\nZ\ncd",
        "O typed on the line it opened"
    );

    ui.editor.set_line("ab\ncd");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;
    ui.key(&lane, &mut view, typed('o'), false);
    ui.key(&lane, &mut view, typed('Z'), false);
    assert_eq!(
        ui.editor.text(),
        "ab\ncd\nZ",
        "o typed under the caret's line"
    );
}

// `dd` takes the line and stays. The first `d` waits, the second fires.
#[test]
fn dd_takes_the_whole_line_and_stays_in_normal() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("ab\ncd\nef");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('d'), false);
    assert_eq!(ui.editor.text(), "ab\ncd\nef", "the first d waits");

    ui.key(&lane, &mut view, typed('d'), false);
    assert_eq!(ui.editor.text(), "ab\ncd");
    assert_eq!(mode(&ui), Some(Mode::Normal));

    ui.key(&lane, &mut view, typed('d'), false);
    ui.key(&lane, &mut view, typed('d'), false);
    assert_eq!(ui.editor.text(), "ab");

    ui.key(&lane, &mut view, typed('d'), false);
    ui.key(&lane, &mut view, typed('z'), false);
    ui.key(&lane, &mut view, typed('d'), false);
    assert_eq!(
        ui.editor.text(),
        "ab",
        "an intervening key cancels the sequence"
    );
}

// `gg` and `G` run to the buffer's ends, and `^` to the first non-blank.
#[test]
fn gg_g_and_caret_walk_the_lines() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.editor.set_line("  ab\ncd");
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('g'), false);
    ui.key(&lane, &mut view, typed('g'), false);
    assert_eq!(ui.editor.cursor(), 0);
    assert_eq!(mode(&ui), Some(Mode::Normal));

    ui.key(&lane, &mut view, typed('^'), false);
    assert_eq!(ui.editor.cursor(), 2, "past the indent");

    ui.key(&lane, &mut view, typed('G'), false);
    assert_eq!(ui.editor.cursor(), ui.editor.text().len());
}

// `gg` and `G` take the history's ends while the line is empty and the
// buffer's once anything is typed.
#[test]
fn an_empty_line_sends_gg_and_g_to_the_history_ends() {
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=60).map(|n| Row::notice(format!("row {n}"))).collect();
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.key(&lane, &mut view, typed('g'), false);
    ui.key(&lane, &mut view, typed('g'), false);
    ui.flush(&lane, &mut view);
    let top = view.surface.scroll;
    assert!(top > 0, "gg went back through the history: {top}");

    ui.key(&lane, &mut view, typed('K'), false);
    ui.flush(&lane, &mut view);
    assert_eq!(view.surface.scroll, top, "gg is as far back as it goes");

    ui.key(&lane, &mut view, typed('G'), false);
    ui.flush(&lane, &mut view);
    assert_eq!(view.surface.scroll, 0, "G came back to the newest rows");

    // A line to command: the keys stay on it, and the history stays where
    // the user scrolled it to.
    ui.key(&lane, &mut view, typed('K'), false);
    ui.flush(&lane, &mut view);
    let up = view.surface.scroll;
    ui.editor.set_line("ab\ncd");
    ui.key(&lane, &mut view, typed('G'), false);
    assert_eq!(ui.editor.cursor(), 5, "G runs to the text's end");
    ui.key(&lane, &mut view, typed('g'), false);
    ui.key(&lane, &mut view, typed('g'), false);
    ui.flush(&lane, &mut view);
    assert_eq!(ui.editor.cursor(), 0, "and gg to its start");
    assert_eq!(view.surface.scroll, up, "the history stayed where it was");
}

// `ctrl+l` weighs the line: with text to lose one press clears it, with
// nothing there the session is what a press would replace, so it takes
// two. The press that cleared the line arms nothing toward the second.
#[test]
fn ctrl_l_clears_the_line_and_twice_starts_a_session() {
    let mut ui = test_ui(80, 24);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();

    ui.editor.set_line("half-typed");
    let asked = ui.key(&lane, &mut view, ctrl('l'), false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert!(ui.editor.is_empty(), "the line went on the one press");

    // Pressed again while it is still empty: the clearing press left the
    // double-tap unarmed, so this one only arms it.
    let asked = ui.key(&lane, &mut view, ctrl('l'), false);
    assert!(
        matches!(asked, Asked::Own(Deed::Nothing)),
        "the press that cleared the line started nothing: {asked:?}"
    );
}

#[test]
fn an_empty_line_takes_two_presses_for_a_new_session() {
    let mut ui = test_ui(80, 24);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();

    let asked = ui.key(&lane, &mut view, ctrl('l'), false);
    assert!(
        matches!(asked, Asked::Own(Deed::Nothing)),
        "the first press only arms it: {asked:?}"
    );

    let asked = ui.key(&lane, &mut view, ctrl('l'), false);
    assert!(
        matches!(asked, Asked::Core(Intent::Builtin(Builtin::New))),
        "the second starts the session: {asked:?}"
    );
}

// `ctrl+c` gave up clearing the line: it stops the run, and the second
// press inside the window is the one that leaves.
#[test]
fn ctrl_c_stops_the_run_and_leaves_the_line() {
    let mut ui = test_ui(80, 24);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();

    ui.editor.set_line("half-typed");
    let asked = ui.key(&lane, &mut view, ctrl('c'), false);
    assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    assert_eq!(
        ui.editor.text(),
        "half-typed",
        "the line is not its to clear"
    );
}

// `v` on an empty line opens the conversation view, and only there: with
// something on the line it is a letter vim made into a key of its own.
#[test]
fn v_opens_the_conversation_view_on_an_empty_line() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let esc = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.vim.as_mut().unwrap().mode = Mode::Normal;
    view.surface.scrollback = (1..=40)
        .flat_map(|n| Row::answer(&format!("answer {n}"), &ui.paint))
        .collect();

    // A line with something on it: `v` is Ignore, and nothing opens.
    ui.editor.set_line("half-typed");
    ui.key(&lane, &mut view, typed('v'), false);
    assert!(!ui.browsing, "a line to command keeps its v");
    assert_eq!(ui.editor.text(), "half-typed");

    ui.editor.clear();
    ui.key(&lane, &mut view, typed('v'), false);
    assert!(ui.browsing);

    // The keys in here are its own: `x` would delete a character in
    // Normal, and `k` scrolls back rather than reaching for history.
    ui.key(&lane, &mut view, typed('x'), false);
    assert_eq!(ui.editor.text(), "", "nothing types into a hidden line");
    ui.key(&lane, &mut view, typed('k'), false);
    assert!(view.surface.scroll > 0, "k walked the conversation back");

    // The window keys are the empty line's own: `K`/`J` half a screen,
    // `G` and `gg` the conversation's two ends.
    let (walked, half) = (view.surface.scroll, ui.half_scroll_step());
    ui.key(&lane, &mut view, typed('K'), false);
    assert_eq!(
        view.surface.scroll,
        walked + half,
        "K is half a window back"
    );
    ui.key(&lane, &mut view, typed('J'), false);
    assert_eq!(view.surface.scroll, walked, "and J undoes it");

    // A lone `g` is half a pair: it moves nothing until the other half
    // arrives.
    ui.key(&lane, &mut view, typed('g'), false);
    assert_eq!(view.surface.scroll, walked, "one g moves nothing");
    ui.key(&lane, &mut view, typed('g'), false);
    assert_eq!(view.surface.scroll, screen::TOP, "gg is the top");
    ui.key(&lane, &mut view, typed('G'), false);
    assert_eq!(view.surface.scroll, 0, "G is the newest rows");

    // A key that is not the second `g` ends the pair: the `k` here scrolls
    // its own row and leaves the `g` before it a first half, no pair.
    ui.key(&lane, &mut view, typed('g'), false);
    ui.key(&lane, &mut view, typed('k'), false);
    ui.key(&lane, &mut view, typed('g'), false);
    assert_eq!(view.surface.scroll, 1, "a k between them is not a gg");

    // The half left armed in here goes with the mode: a `g` on each side
    // of it is not a pair.
    ui.key(&lane, &mut view, typed('g'), false);
    ui.key(&lane, &mut view, esc, false);
    assert!(!ui.browsing);
    assert_eq!(view.surface.scroll, 0, "and leaves at the newest rows");
    ui.key(&lane, &mut view, typed('g'), false);
    assert_eq!(
        view.surface.scroll, 0,
        "a g made after the mode is not its pair"
    );
}

// Turning the keys off is the one thing that moves the mode without a
// keystroke — otherwise switching back on would land in Normal with
// nothing having asked to go there.
#[test]
fn turning_the_keys_off_drops_the_mode_rather_than_parking_it() {
    let mut ui = vim_ui();
    ui.vim.as_mut().unwrap().mode = Mode::Normal;

    ui.set_vim(&pi_core::store::config::Vim {
        enabled: false,
        ..Default::default()
    });
    assert!(ui.vim.is_none());

    ui.set_vim(&pi_core::store::config::Vim {
        enabled: true,
        ..Default::default()
    });
    assert_eq!(mode(&ui), Some(Mode::Insert));
}
