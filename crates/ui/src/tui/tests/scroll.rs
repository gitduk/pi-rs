use crate::render::Paint;
use crate::tui::row::{FoldedTool, Row, Step};
use crate::tui::scrollback::{Folds, Scrollback, ScrollbackRows};
use crate::tui::{View, following_terminal};
use pi_store::theme::{Color, Theme};
use ratatui::text::Line;

use super::harness::*;

// Rows are painted once per width; a stale cache would leak the narrow
// frame's clipping into the wide one, visible only below the head row.
#[test]
fn every_row_of_a_result_is_repainted_when_the_window_changes() {
    let paint = Paint::new(false);
    let long = "x".repeat(200);
    let rows = [Row::result(true, "edit", format!("head\n+12 {long}"))];

    let narrow: Vec<String> = ScrollbackRows::new(&rows, &paint, 40, |_| true)
        .map(text)
        .collect();
    let wide: Vec<String> = ScrollbackRows::new(&rows, &paint, 160, |_| true)
        .map(text)
        .collect();
    // And back again: widening must not be the only direction that repaints.
    let again: Vec<String> = ScrollbackRows::new(&rows, &paint, 40, |_| true)
        .map(text)
        .collect();

    assert_eq!(narrow.len(), 2, "head plus the one diff row");
    assert!(
        wide[1].len() > narrow[1].len(),
        "{} vs {}",
        wide[1],
        narrow[1]
    );
    assert_eq!(narrow, again, "the narrow frame came back different");
}

// A band the config named itself outlives the terminal's, and a terminal
// that would not say changes nothing. Read on every config adopted.
#[test]
fn a_configured_band_outlives_the_terminals() {
    let bg = Some((13, 17, 23));
    let mut theme = Theme::default();
    let derived = following_terminal(&theme, bg);
    assert_ne!(derived.prompt.panel.input, theme.prompt.panel.input);
    assert_eq!(
        following_terminal(&theme, None),
        theme,
        "no answer, no lift"
    );

    theme.prompt.panel.said = Color::Rgb(1, 2, 3);
    let mixed = following_terminal(&theme, bg);
    assert_eq!(mixed.prompt.panel.said, Color::Rgb(1, 2, 3));
    assert_ne!(mixed.prompt.panel.input, theme.prompt.panel.input);
}

// A skipped row costs only its count; both walks must agree on where kept
// rows are — the back walk especially, which starts inside the last row.
#[test]
fn a_filtered_window_reads_the_rows_it_keeps_and_skips_the_rest() {
    let paint = Paint::new(false);
    let mut rows = vec![Row::notice("a command printed this")];
    rows.extend(Row::answer("the answer", &paint));
    rows.push(Row::notice("and then a warning"));

    let keep = |row: &Row| row.is_conversation();
    let forwards: Vec<String> = ScrollbackRows::new(&rows, &paint, 80, keep)
        .map(text)
        .collect();
    assert_eq!(forwards, ["the answer"]);

    let backwards: Vec<String> = ScrollbackRows::new(&rows, &paint, 80, keep)
        .rev()
        .map(text)
        .collect();
    assert_eq!(backwards, ["the answer"], "the back walk too");

    // Nothing kept at all leaves both walks with nothing, which is how a
    // browse of a screen holding no conversation looks.
    let none: Vec<String> = ScrollbackRows::new(&rows, &paint, 80, |_| false)
        .map(text)
        .collect();
    assert!(none.is_empty());
}

#[test]
fn an_empty_scrollback_iterates_to_nothing() {
    // Both walks index `rows[0]` before comparing pointers; an empty
    // scrollback panics there if either forgets to guard it.
    let paint = Paint::new(false);
    let rows: Vec<String> = ScrollbackRows::new(&[], &paint, 80, |_| true)
        .map(text)
        .collect();
    assert!(rows.is_empty());
    let back: Vec<String> = ScrollbackRows::new(&[], &paint, 80, |_| true)
        .rev()
        .map(text)
        .collect();
    assert!(back.is_empty(), "the back walk too");
}

#[test]
fn scrollback_rows_walk_from_both_ends() {
    let rows = vec![
        Row::notice("a".to_string()),
        block(1, 2, false),
        Row::notice("d".to_string()),
    ];
    let paint = Paint::new(false);
    let rows = ScrollbackRows::new(&rows, &paint, 80, |_| true);
    let (front, back): (Vec<_>, Vec<_>) = {
        let mut f = Vec::new();
        let mut b = Vec::new();
        let mut it = rows;
        loop {
            match (it.next(), it.next_back()) {
                (Some(x), Some(y)) => {
                    f.push(x);
                    b.push(y);
                }
                (Some(x), None) => f.push(x),
                (None, Some(y)) => b.push(y),
                (None, None) => break,
            }
        }
        (f, b)
    };
    let front: Vec<String> = front.into_iter().map(text).collect();
    let back: Vec<String> = back.into_iter().map(text).collect();
    assert_eq!(front, vec!["a", "line 1"]);
    assert_eq!(back, vec!["d", "line 2"]);
}

// A block streamed in: started, and one line landed in its group.
fn streamed(t: &mut Folds, scrollback: &mut Scrollback) {
    t.start(scrollback);
    let id = t.streaming.expect("started");
    t.streaming_row(scrollback)
        .expect("the group start made")
        .push_line(id, Line::from("thought"));
}

#[test]
fn toggling_moves_the_last_group_and_nothing_else() {
    // `ctrl+t` flips the group that is last now, and only it: the group
    // pushed out of last by the new one folds back to the switch.
    let mut t = Folds::default();
    let mut scrollback = vec![
        Row::thinking(9, vec![Line::from("old")], false),
        Row::notice("an answer"),
    ]
    .into();
    t.start(&mut scrollback);
    t.toggle_current(&mut scrollback);
    assert!(t.folded);
    assert_eq!(scrollback[0].folded(), Some(true));
    assert_eq!(scrollback[2].folded(), Some(false));
}

#[test]
fn a_finished_group_keeps_its_fold_until_the_next_question() {
    // An unfold survives the answer — a finished group is still last —
    // and folds back to the switch the moment a new input is submitted.
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    t.toggle_current(&mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(false));
    t.close_block(&mut scrollback);
    // Still last until the next question is asked.
    assert_eq!(scrollback[0].folded(), Some(false));
    t.fold_previous(&mut scrollback);
    // The submitted question pushes it out of last: it folds to the switch.
    assert_eq!(scrollback[0].folded(), Some(true));
    assert!(!t.birth_fold());
}

#[test]
fn a_finished_group_follows_a_global_unfold() {
    // The fold follows the switch both ways: a screen the global key
    // opened keeps its group open once the next question takes over.
    let mut t = Folds {
        folded: false,
        ..Default::default()
    };
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    t.close_block(&mut scrollback);
    t.fold_previous(&mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(false));
}

#[test]
fn a_block_after_a_call_joins_its_group() {
    // The run's working between two answers is one group: a call and the
    // blocks on either side of it fold behind one line.
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    t.close_block(&mut scrollback);
    t.join(
        &mut scrollback,
        Step::Tool(FoldedTool::new("read", "a.rs", "", "")),
    );
    streamed(&mut t, &mut scrollback);
    t.close_block(&mut scrollback);
    assert_eq!(scrollback.len(), 1);
    assert!(scrollback[0].holds_block(1) && scrollback[0].holds_block(2));
}

#[test]
fn a_block_after_an_answer_starts_a_group_and_inherits_the_flip() {
    // Past an answer the next block is a new group and new last: the one
    // before folds back, and the new one is born the way `ctrl+t` left it.
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    t.toggle_current(&mut scrollback);
    t.close_block(&mut scrollback);
    scrollback.push(Row::notice("an answer"));
    streamed(&mut t, &mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(true));
    assert_eq!(scrollback[2].folded(), Some(false));
}

#[test]
fn a_block_that_ends_without_a_line_leaves_nothing() {
    // `ctrl+t` before the first line flips the group `start` made, and the
    // flip outlives it: a block with no line takes its lone group with it.
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    t.start(&mut scrollback);
    t.toggle_current(&mut scrollback);
    assert!(t.folded, "the switch itself is not touched");
    assert_eq!(scrollback[0].folded(), Some(false));
    t.close_block(&mut scrollback);
    let drawn = ScrollbackRows::new(&scrollback, &Paint::new(false), 80, |_| true).count();
    assert_eq!(drawn, 0, "nothing drawn");
    assert!(!scrollback.iter().any(Row::is_steps), "no group left");
    assert!(!t.birth_fold());
}

#[test]
fn the_live_text_follows_the_streaming_group() {
    // The live region reads the group's own state, not the switch: a group
    // the user unfolded streams even though the switch still says folded.
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    t.start(&mut scrollback);
    assert!(t.holds(true, &scrollback));
    scrollback[0].set_folded(false);
    assert!(!t.holds(true, &scrollback));
}

#[test]
fn a_global_flip_takes_the_current_group_with_it() {
    // Everything else unfolded, the current group folded on its own; the
    // global key still folds it too, since the fold is where the rest go.
    let mut t = Folds {
        folded: false,
        ..Default::default()
    };
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(true));
    t.flip_all(&mut scrollback);
    assert!(t.folded);
    assert_eq!(scrollback[0].folded(), Some(true));
}

#[test]
fn flipping_every_group_moves_the_switch_with_them() {
    // The global key folds every group, current one included, and moves
    // the switch with them: rows and switch never disagree.
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    t.toggle_current(&mut scrollback); // unfold the current group on its own
    t.close_block(&mut scrollback);
    t.flip_all(&mut scrollback); // global fold
    assert!(!t.folded);
    assert!(scrollback.iter().all(|e| e.folded() == Some(false)));
    // The switch moved with them, so the next group is born unfolded.
    assert!(!t.birth_fold());
    // And a second global press folds the whole screen back.
    t.flip_all(&mut scrollback);
    assert!(t.folded);
    assert!(scrollback.iter().all(|e| e.folded() == Some(true)));
}

#[test]
fn a_flip_applies_to_each_new_last_group_until_flipped_back() {
    // `ctrl+t` controls whichever group is last: each new one is born
    // unfolded, and the one it displaces folds back to the switch.
    let mut t = Folds::default();

    // Startup: the key names a group that does not exist yet.
    t.toggle_current(&mut []);
    assert!(!t.birth_fold());

    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(false));
    t.close_block(&mut scrollback);

    // An answer ends the group; the next one is the new last, born
    // unfolded, and the first folds back to the switch.
    scrollback.push(Row::notice("an answer"));
    streamed(&mut t, &mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(true));
    assert_eq!(scrollback[2].folded(), Some(false));
}

// A click moves a group without the key knowing; the key then flips what
// the group shows, rather than a remembered value it no longer matches.
#[test]
fn the_key_flips_what_a_click_left() {
    let mut t = Folds::default();
    let mut scrollback = Scrollback::default();
    streamed(&mut t, &mut scrollback);
    assert!(scrollback[0].toggle_expand(), "a click unfolds it");
    t.toggle_current(&mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(true));
}

#[test]
fn a_scrolled_up_view_holds_until_the_user_scrolls_back() {
    // Scrolled two rows up from ten rows of history, rows 5-8 stay put
    // as output arrives below; only the user's own scroll moves them.
    let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
    let (room, mut scroll, mut top) = (4usize, 0usize, None);
    scroll = scroll.saturating_add(2);
    let first = frame(&content, room, &mut scroll, &mut top);
    assert_eq!(first, vec!["5", "6", "7", "8"]);
    for n in 11..=15 {
        content.push(n.to_string());
        assert_eq!(
            frame(&content, room, &mut scroll, &mut top),
            first,
            "row {n} arriving moved the scrolled-up window"
        );
    }
    // As `scroll_view`: by rows from where the last frame stood.
    (scroll, top) = (scroll.saturating_sub(1), None);
    assert_eq!(
        frame(&content, room, &mut scroll, &mut top),
        vec!["6", "7", "8", "9"]
    );
}

#[test]
fn rows_gone_below_the_window_leave_the_view_put() {
    // Rows removed below the window shrink the tail; the window stays put.
    let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
    let (room, mut scroll, mut top) = (4usize, 0usize, None);
    scroll = scroll.saturating_add(2);
    assert_eq!(
        frame(&content, room, &mut scroll, &mut top),
        vec!["5", "6", "7", "8"]
    );
    for n in 11..=15 {
        content.push(n.to_string());
        frame(&content, room, &mut scroll, &mut top);
    }
    content.truncate(10);
    assert_eq!(
        frame(&content, room, &mut scroll, &mut top),
        vec!["5", "6", "7", "8"]
    );
}

// A row that wraps counts for the rows it takes, not the line it is:
// both of its rows go under the window, or the window drifts.
#[test]
fn a_wrapped_row_landing_below_moves_the_scroll_by_its_rows() {
    let mut ui = test_ui(20, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=12).map(|n| Row::notice(format!("row {n}"))).collect();
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 2);
    ui.flush(&lane, &mut view);
    let (shown, scrolled) = (ui.drawn.clone(), view.surface.scroll);

    // 25 columns at a 19-column width: one line, two rows.
    view.surface.scrollback.push(Row::notice("x".repeat(25)));
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn, shown, "the window stayed put");
    assert_eq!(view.surface.scroll, scrolled + 2, "on both wrapped rows");
}

// Groups folding above the window take their rows with them; the line at
// the top stays at the top.
#[test]
fn folding_above_the_window_leaves_its_top_line_put() {
    let mut ui = test_ui(30, 8);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = vec![block(1, 6, false)].into();
    view.surface
        .scrollback
        .extend((1..=12).map(|n| Row::notice(format!("row {n}"))));
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 6);
    ui.flush(&lane, &mut view);
    let top = ui.drawn[0].clone();
    assert!(top.contains("row"), "{top:?}");
    view.surface.folds.flip_all(&mut view.surface.scrollback);
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top);
}

// A group can stop being unfoldable while unfolded — a call it held failed
// and left — so the key reads it as shown, folded, and opens the next one.
#[test]
fn the_key_reads_a_group_with_nothing_to_unfold_as_folded() {
    let mut t = Folds::default();
    let mut scrollback: Scrollback = vec![Row::steps(
        Step::Tool(FoldedTool::new("read", "a.rs", "", "")),
        false,
    )]
    .into();
    t.toggle_current(&mut scrollback);
    assert!(!t.birth_fold(), "the next group is born open");
}

// A panel that holds the keys sets the transcript back, so the two do not
// read as one; closed, the transcript is itself again.
#[test]
fn a_panel_dims_the_transcript_under_it() {
    use ratatui::style::Modifier;
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = vec![Row::notice("in the transcript")].into();
    let dimmed = |ui: &crate::tui::Ui| {
        ui.screen.test_buffer()[(0, ui.regions.history.y)]
            .modifier
            .contains(Modifier::DIM)
    };
    ui.flush(&lane, &mut view);
    assert!(!dimmed(&ui));
    ui.open_command(pi_store::listing::Listing::say(["/status answered"]));
    ui.flush(&lane, &mut view);
    assert!(dimmed(&ui), "under the panel");
    ui.focus = crate::tui::ui::Focus::Editor;
    ui.flush(&lane, &mut view);
    assert!(!dimmed(&ui), "the panel closed");
}

// A found line comes up to the top of the preview, `n` / `N` step round
// the lines holding it, and esc puts the view back where `/` found it.
#[test]
fn a_search_brings_each_line_holding_it_to_the_top() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.preview(
        (0..100)
            .map(|i| (Line::from(format!("line {i}")), None))
            .collect(),
    );
    let press = |ui: &mut crate::tui::Ui, view: &mut View, event| {
        ui.key(&lane, view, event, false);
        ui.flush(&lane, view);
        ui.drawn[0].trim_end().to_string()
    };
    let key = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    ui.flush(&lane, &mut view);
    assert_eq!(press(&mut ui, &mut view, typed('/')), "line 0");
    assert_eq!(press(&mut ui, &mut view, typed('4')), "line 4");
    let reversed = |ui: &crate::tui::Ui, col| {
        ui.screen.test_buffer()[(col, 0)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    };
    assert!(reversed(&ui, 5) && !reversed(&ui, 4), "the 4 alone");
    assert_eq!(press(&mut ui, &mut view, typed('2')), "line 42");
    assert_eq!(press(&mut ui, &mut view, key(KeyCode::Backspace)), "line 4");
    // Which hit of how many: twenty 4s, line 44 holding two.
    let count = |ui: &crate::tui::Ui| ui.search_prompt().map(|(_, c)| c.trim().to_string());
    assert_eq!(press(&mut ui, &mut view, key(KeyCode::Enter)), "line 4");
    assert_eq!(count(&ui).as_deref(), Some("1/20"));
    assert_eq!(press(&mut ui, &mut view, typed('n')), "line 14");
    assert_eq!(count(&ui).as_deref(), Some("2/20"));
    assert_eq!(press(&mut ui, &mut view, typed('N')), "line 4");
    // Past the first, round to the last, which the bottom of the page holds.
    let bottom = press(&mut ui, &mut view, typed('N'));
    assert!(ui.drawn.iter().any(|r| r.starts_with("line 94")));
    assert_eq!(count(&ui).as_deref(), Some("20/20"));
    assert_eq!(press(&mut ui, &mut view, typed('/')), bottom);
    press(&mut ui, &mut view, typed('x'));
    // The row the search took comes back, and the page is still at its end.
    press(&mut ui, &mut view, key(KeyCode::Esc));
    assert!(ui.drawn.iter().any(|r| r.starts_with("line 99")));
    assert!(ui.reading(), "esc leaves the query, not the page");
}

// A command page searches its own lines, not the transcript it was typed
// over, though both hold the query.
#[test]
fn a_command_page_searches_only_its_own_lines() {
    use ratatui::style::Modifier;
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = vec![Row::notice("beta in the transcript")].into();
    ui.open_command(pi_store::listing::Listing::say(["alpha", "beta"]));
    ui.flush(&lane, &mut view);
    for c in "/beta".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.flush(&lane, &mut view);
    // The cell `beta` starts on, on the row whose text is `line`.
    let marked = |line: &str| {
        let row = ui.drawn.iter().position(|r| r.trim() == line).unwrap();
        let col = ui.drawn[row].find("beta").unwrap();
        ui.screen.test_buffer()[(col as u16, ui.regions.history.y + row as u16)]
            .modifier
            .contains(Modifier::REVERSED)
    };
    assert!(marked("beta"), "the page's own");
    assert!(!marked("beta in the transcript"), "the transcript over it");
}

// The query is edited with the line's own keys: `ctrl+w` takes a word.
#[test]
fn a_search_query_takes_the_line_editing_keys() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.preview(vec![(Line::from("head"), None)]);
    ui.flush(&lane, &mut view);
    for c in "/one two".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.key(&lane, &mut view, ctrl('w'), false);
    let query = |ui: &crate::tui::Ui| ui.search_prompt().map(|(q, _)| q);
    assert_eq!(query(&ui).as_deref(), Some("/one "));
    ui.key(&lane, &mut view, ctrl('u'), false);
    assert_eq!(query(&ui).as_deref(), Some("/"));
    assert!(ui.reading());
}

// What is counted is what is drawn: an escape the frame drops holds no
// match, and a line wrapping past the window is stepped row by row.
#[test]
fn a_search_counts_what_is_drawn_and_reaches_rows_below_the_window() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let long = (0..200)
        .map(|i| format!("w{i:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    ui.preview(vec![
        (Line::from("head"), None),
        (Line::from("\x1b[31mred\x1b[0m"), None),
        (Line::from(long), None),
    ]);
    ui.flush(&lane, &mut view);
    let count = |ui: &crate::tui::Ui| ui.search_prompt().map(|(_, c)| c.trim().to_string());
    for c in "/31m".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.flush(&lane, &mut view);
    assert_eq!(
        count(&ui).as_deref(),
        Some("0/0"),
        "the escape is not drawn"
    );
    ui.key(&lane, &mut view, ctrl('u'), false);
    for c in "w199".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.flush(&lane, &mut view);
    assert_eq!(count(&ui).as_deref(), Some("1/1"));
    assert!(
        ui.drawn.iter().any(|r| r.contains("w199")),
        "the row holding it is on screen, though its line starts above"
    );
}

// A paste while the query is typed goes on the query.
#[test]
fn a_paste_goes_on_the_query() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.preview(vec![(Line::from("head"), None)]);
    ui.flush(&lane, &mut view);
    ui.key(&lane, &mut view, typed('/'), false);
    ui.key(
        &lane,
        &mut view,
        crate::tui::TermEvent::Paste("one\ntwo".into()),
        false,
    );
    assert_eq!(ui.search_prompt().map(|(q, _)| q).as_deref(), Some("/one"));
}

// A view moved off the hit steps from where it is: `gg n` is the first
// hit, `G N` the last, not the ones beside the hit left behind.
#[test]
fn a_step_after_moving_the_view_goes_from_the_view() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    // `gg` is a pair, which only vim keys give the time to type.
    let mut ui = vim_ui();
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.preview(
        (0..100)
            .map(|i| (Line::from(format!("line {i}")), None))
            .collect(),
    );
    // The count as the one frame after the key drew it, not as kept.
    let press = |ui: &mut crate::tui::Ui, view: &mut View, event| {
        ui.key(&lane, view, event, false);
        ui.flush(&lane, view);
        let buf = ui.screen.test_buffer();
        let bottom = buf.area.height - 1;
        let row: String = (0..buf.area.width)
            .map(|x| buf[(x, bottom)].symbol())
            .collect();
        row.split_whitespace().nth(1).map(str::to_string)
    };
    ui.flush(&lane, &mut view);
    press(&mut ui, &mut view, typed('/'));
    press(&mut ui, &mut view, typed('4'));
    let enter = crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    press(&mut ui, &mut view, enter);
    // From the first hit itself: `gg n` is that hit again, not the one after.
    press(&mut ui, &mut view, typed('G'));
    press(&mut ui, &mut view, typed('g'));
    press(&mut ui, &mut view, typed('g'));
    assert_eq!(
        press(&mut ui, &mut view, typed('n')).as_deref(),
        Some("1/20")
    );
    for _ in 0..6 {
        press(&mut ui, &mut view, typed('n'));
    }
    assert_eq!(
        press(&mut ui, &mut view, typed('n')).as_deref(),
        Some("8/20")
    );
    press(&mut ui, &mut view, typed('g'));
    press(&mut ui, &mut view, typed('g'));
    assert_eq!(
        press(&mut ui, &mut view, typed('n')).as_deref(),
        Some("1/20")
    );
    // Unmoved since, `n` goes on from the hit as before.
    assert_eq!(
        press(&mut ui, &mut view, typed('n')).as_deref(),
        Some("2/20")
    );
    press(&mut ui, &mut view, typed('G'));
    assert_eq!(
        press(&mut ui, &mut view, typed('N')).as_deref(),
        Some("20/20")
    );
}

// A fold opened by a click grows under the line clicked, which stays on
// the row it was on, even on the newest rows.
#[test]
fn a_clicked_fold_opens_under_the_line_clicked() {
    use crate::tui::mouse::Target;
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut ui = test_ui(30, 24);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=20).map(|n| Row::notice(format!("row {n}"))).collect();
    view.surface.scrollback.push(block(1, 6, true));
    view.surface.scrollback.push(Row::notice("after"));
    ui.flush(&lane, &mut view);
    let at = |ui: &crate::tui::Ui| {
        ui.row_targets
            .iter()
            .position(|t| matches!(t, Target::Scrollback(20, 0)))
            .expect("the fold drawn")
    };
    let (before, top) = (at(&ui), ui.drawn[0].clone());
    let row = ui.regions.history.y + before as u16;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        let event = Event::Mouse(MouseEvent {
            kind,
            column: 2,
            row,
            modifiers: KeyModifiers::NONE,
        });
        ui.key(&lane, &mut view, event, false);
    }
    ui.flush(&lane, &mut view);
    assert_eq!(
        view.surface.scrollback[20].folded(),
        Some(false),
        "it opened"
    );
    assert_eq!((at(&ui), &ui.drawn[0]), (before, &top));
}

// A scroll by key is taken from the top row last drawn: what lands before
// the next frame leaves it where the key put it.
#[test]
fn rows_landing_after_a_scroll_key_leave_where_it_went() {
    let (_dir, lane) = a_running_lane();
    let scrolled = |landing: usize| {
        let mut ui = test_ui(30, 12);
        let mut view = View::default();
        view.surface.scrollback = (1..=30).map(|n| Row::notice(format!("row {n}"))).collect();
        ui.flush(&lane, &mut view);
        ui.scroll_view(&mut view, true, 3);
        for n in 0..landing {
            view.surface
                .scrollback
                .push(Row::notice(format!("new {n}")));
        }
        ui.flush(&lane, &mut view);
        ui.drawn.clone()
    };
    assert_eq!(scrolled(4), scrolled(0));
}

// A top row in the answer still streaming stays put as the answer grows,
// and when it lands among the settled rows with more coming under it.
#[test]
fn a_top_row_in_the_stream_holds_as_it_grows_and_lands() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=5).map(|n| Row::notice(format!("row {n}"))).collect();
    view.surface.stream.text = (1..=20).map(|n| format!("said {n}\n\n")).collect();
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 6);
    ui.flush(&lane, &mut view);
    let top = ui.drawn[0].clone();
    assert!(top.contains("said"), "the top row is the stream's: {top:?}");
    view.surface.stream.text.push_str("said 21\n\nsaid 22\n\n");
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top, "as it grew");
    ui.close(&mut view);
    view.surface.scrollback.push(Row::notice("next"));
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top, "as it landed");
}
