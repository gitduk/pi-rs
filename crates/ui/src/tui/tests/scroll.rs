use crate::render::Paint;
use crate::tui::row::{FoldedTool, Row, Step};
use crate::tui::scrollback::{Folds, ScrollbackRows};
use crate::tui::{View, following_terminal};
use pi_core::store::theme::{Color, Theme};
use ratatui::text::Line;

use super::harness::*;

// The rows of one result are painted once per width and handed out one at
// a time, so a stale cache would show the narrow frame's clipping in the
// wide one — and only below the head row, where the single-row case
// cannot see it.
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

// What browse mode is built on: a row the caller does not want is passed
// over by its count alone, and the two walks have to agree about where the
// rows it does want are — the back walk especially, which starts inside
// the last row there is.
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
    // Both walks index `rows[0]` before comparing their pointers, so an
    // empty scrollback panicked. The back walk kept doing it after the
    // front was fixed, and `screen::window` is the one that walks back.
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
fn streamed(t: &mut Folds, scrollback: &mut Vec<Row>) {
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
    ];
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
    let mut scrollback = Vec::new();
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
    let mut scrollback = Vec::new();
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
    let mut scrollback = Vec::new();
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
    // Past an answer the next block is a new group and the new last: the
    // one before folds back to the switch, and the new one is born the way
    // `ctrl+t` left the last group.
    let mut t = Folds::default();
    let mut scrollback = Vec::new();
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
    let mut scrollback = Vec::new();
    t.start(&mut scrollback);
    t.toggle_current(&mut scrollback);
    assert!(t.folded, "the switch itself is not touched");
    assert_eq!(scrollback[0].folded(), Some(false));
    t.close_block(&mut scrollback);
    assert!(scrollback.is_empty());
    assert!(!t.birth_fold());
}

#[test]
fn the_live_text_follows_the_streaming_group() {
    // The live region reads the group's own state, not the last value: a
    // group the user unfolded streams its lines even though the switch
    // still says folded.
    let mut t = Folds::default();
    let mut scrollback = Vec::new();
    t.start(&mut scrollback);
    assert!(t.holds(true, &scrollback));
    scrollback[0].set_folded(false);
    assert!(!t.holds(true, &scrollback));
}

#[test]
fn a_global_flip_takes_the_current_group_with_it() {
    // The case that named the key: everything else unfolded, the current
    // group folded on its own. The global key folds the whole screen — the
    // current group keeps its fold, because the fold is where the rest are
    // going.
    let mut t = Folds {
        folded: false,
        ..Default::default()
    };
    let mut scrollback = Vec::new();
    streamed(&mut t, &mut scrollback);
    assert_eq!(scrollback[0].folded(), Some(true));
    t.flip_all(&mut scrollback);
    assert!(t.folded);
    assert_eq!(scrollback[0].folded(), Some(true));
}

#[test]
fn flipping_every_group_moves_the_switch_with_them() {
    // The global key folds or unfolds every group, the current one
    // included, and moves the switch with them: rows and switch never
    // disagree, so the screen always folds back to a single state.
    let mut t = Folds::default();
    let mut scrollback = Vec::new();
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
    // `ctrl+t` controls the last group, whatever it is: the first one is
    // born unfolded, and each new group that takes over as last is born
    // unfolded too, while the one it displaces folds back to the switch.
    let mut t = Folds::default();

    // Startup: the key names a group that does not exist yet.
    t.toggle_current(&mut []);
    assert!(!t.birth_fold());

    let mut scrollback = Vec::new();
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
    let mut scrollback = Vec::new();
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
    let (room, mut scroll, mut last_total) = (4usize, 0usize, None);
    scroll = scroll.saturating_add(2);
    let first = frame(&content, room, &mut scroll, &mut last_total);
    assert_eq!(first, vec!["5", "6", "7", "8"]);
    for n in 11..=15 {
        content.push(n.to_string());
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            first,
            "row {n} arriving moved the scrolled-up window"
        );
    }
    scroll = scroll.saturating_sub(1);
    assert_eq!(
        frame(&content, room, &mut scroll, &mut last_total),
        vec!["6", "7", "8", "9"]
    );
}

#[test]
fn rows_gone_below_the_window_leave_the_view_put() {
    // Rows removed below the window shrink the tail; the negative delta
    // is absorbed like a positive one and the window stays put.
    let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
    let (room, mut scroll, mut last_total) = (4usize, 0usize, None);
    scroll = scroll.saturating_add(2);
    assert_eq!(
        frame(&content, room, &mut scroll, &mut last_total),
        vec!["5", "6", "7", "8"]
    );
    for n in 11..=15 {
        content.push(n.to_string());
        frame(&content, room, &mut scroll, &mut last_total);
    }
    content.truncate(10);
    assert_eq!(
        frame(&content, room, &mut scroll, &mut last_total),
        vec!["5", "6", "7", "8"]
    );
}

// A row that wraps counts for the rows it takes, not the line it is:
// both of its rows fold into the scroll, or the window drifts.
#[test]
fn a_wrapped_row_landing_below_moves_the_scroll_by_its_rows() {
    let mut ui = test_ui(20, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=12).map(|n| Row::notice(format!("row {n}"))).collect();
    ui.flush(&lane, &mut view);

    // Scroll up, and base the measurement on this frame's layout.
    view.surface.scroll = 2;
    view.surface.counted = None;
    ui.flush(&lane, &mut view);
    let rebased = view.surface.scroll;
    assert_eq!(rebased, 2);

    // 25 columns at a 19-column width: one line, two rows.
    view.surface.scrollback.push(Row::notice("x".repeat(25)));
    ui.flush(&lane, &mut view);
    assert_eq!(
        view.surface.scroll,
        rebased + 2,
        "both wrapped rows folded into the scroll"
    );
}

// A group can stop being unfoldable while unfolded — a call it held failed
// and left — so the key reads it as shown, folded, and opens the next one.
#[test]
fn the_key_reads_a_group_with_nothing_to_unfold_as_folded() {
    let mut t = Folds::default();
    let mut scrollback = vec![Row::steps(
        Step::Tool(FoldedTool::new("read", "a.rs", "", "")),
        false,
    )];
    t.toggle_current(&mut scrollback);
    assert!(!t.birth_fold(), "the next group is born open");
}
