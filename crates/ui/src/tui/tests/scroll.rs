use crate::render::Paint;
use crate::tui::row::{Row, Step, test_call as call};
use crate::tui::scrollback::{Folds, Scrollback, ScrollbackRows};
use crate::tui::{View, following_terminal};
use pi_store::icons;
use pi_store::theme::{Color, Theme};

#[test]
fn command_sigils_only_lead_the_first_screen_row() {
    use crate::tui::screen::{Piece, plain};

    let paint = Paint::new(false);
    for sigil in ["!", "!!"] {
        let source = format!(
            "{sigil}xh POST :8000/documents 'Authorization: Bearer example-token'\nnext line also wraps"
        );
        let history = Row::prompt(&source, 0, &paint);
        for width in [12, 24, 80, 12] {
            let pieces: Vec<_> = ScrollbackRows::new(&history, &paint, width).collect();
            let measured: usize = pieces.iter().map(Piece::height).sum();
            let rows: Vec<String> = pieces
                .into_iter()
                .flat_map(Piece::pieces)
                .map(|line| plain(&line))
                .collect();
            let (_, border) = history[0].line(0, &paint, width);
            let border = border.unwrap();
            assert_eq!(measured, rows.len());
            assert!(rows[0].starts_with(&plain(&border)));
            let indent = " ".repeat(border.width());
            assert!(rows.iter().skip(1).all(|row| row.starts_with(&indent)));
            let body: String = rows.iter().map(|row| &row[indent.len()..]).collect();
            assert_eq!(body, source[sigil.len()..].replace('\n', ""));
        }
    }
}

#[test]
fn notice_folding_keeps_persistence_styles_blocks_and_prose_separate() {
    let paint = Paint::new(true);
    let blocks = [
        vec![vec![Row::notice("same")], vec![Row::aside("same")]],
        vec![
            vec![Row::notice("same")],
            vec![Row::notice(ratatui::text::Line::styled(
                "same",
                ratatui::style::Style::default().fg(ratatui::style::Color::Red),
            ))],
        ],
        vec![Row::notice_lines("same\nsame", &paint)],
        vec![vec![Row::notice("")], vec![Row::notice("")]],
        vec![Row::answer("same", &paint), Row::answer("same", &paint)],
    ];
    for blocks in blocks {
        let mut rows = Scrollback::default();
        for block in blocks {
            rows.append(block);
        }
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| !row.line(0, &paint, 80).0.to_string().contains('×'))
        );
    }
}

#[test]
fn banner_height_tracks_wrapping_after_resizing() {
    use crate::tui::screen::{Piece, plain};

    let paint = Paint::new(true);
    let mut resolved = (*pi_core::core::lane::a_resolved("")).clone();
    resolved.instructions = vec!["~/.pi/AGENTS.md".into()];
    resolved.project = vec![
        (
            "tools".into(),
            "addr-watch, four-watch, loom-buy, loom-sell".into(),
        ),
        ("skills".into(), "order-flow, trade-direction".into()),
    ];
    resolved.notes = vec!["startup note".into()];
    let history = vec![Row::banner(&resolved, &paint)];
    let mut at_forty = Vec::new();

    for width in [1, 8, 16, 24, 40, 60, 100, 240, 40] {
        let pieces: Vec<_> = ScrollbackRows::new(&history, &paint, width).collect();
        let measured: usize = pieces.iter().map(Piece::height).sum();
        let rows: Vec<String> = pieces
            .into_iter()
            .flat_map(Piece::pieces)
            .map(|line| plain(&line))
            .collect();
        assert_eq!(measured, rows.len(), "width {width}");
        assert!(rows.iter().all(|line| pi_store::text::width(line) <= width));
        if width == 40 {
            at_forty.push(rows);
        }
    }
    assert_eq!(
        at_forty[0], at_forty[1],
        "resizing back restores the layout"
    );
}

#[test]
fn banner_height_changes_invalidate_measurements() {
    let paint = Paint::new(true);
    let resolved = pi_core::core::lane::a_resolved("");
    let mut row = Row::banner(&resolved, &paint);
    let width = 80;
    let mut heights = Vec::new();
    for height in [40, 8, 40] {
        row.prepare_layout(&paint, width, height);
        let (line, border) = row.line(0, &paint, width);
        let actual = crate::tui::screen::wrap_rows(border.as_ref(), &line, width);
        let measured = row.line_height(0, &paint, width);
        assert_eq!(measured, actual, "history height {height}");
        heights.push(measured);
    }
    assert_eq!(heights[0], heights[2]);
}

#[test]
fn markdown_continuations_align_in_live_and_settled_rows() {
    use crate::tui::screen::{Piece, plain};
    use crate::tui::scrollback::{Folds, Scrollback, ScrollbackRows, body};

    let source =
        "- abcdefghijklmnop\n  - nested words continue\n\n  > quoted words continue\n\n---";
    let paint = Paint::new(true);
    let history: Scrollback = Row::answer(source, &paint).into();
    for width in [5, 12, 28] {
        let settled: Vec<String> = ScrollbackRows::new(&history, &paint, width)
            .flat_map(Piece::pieces)
            .map(|line| plain(&line))
            .collect();
        let live: Vec<String> = body(
            &Folds::default(),
            &Scrollback::default(),
            false,
            source,
            width,
            &paint,
        )
        .iter()
        .map(plain)
        .collect();
        assert_eq!(settled, live, "width {width}");
        assert_eq!(settled.last().unwrap(), &"─".repeat(width));
    }
    let narrow: Vec<String> = ScrollbackRows::new(&history, &paint, 12)
        .flat_map(Piece::pieces)
        .map(|line| plain(&line))
        .collect();
    assert_eq!(narrow[0], "• abcdefghij");
    assert_eq!(narrow[1], "  klmnop");
    assert!(narrow.iter().any(|line| line.starts_with("  ◦ ")));
    assert!(
        narrow
            .iter()
            .filter(
                |line| line.contains("quoted") || line.contains("continue") && line.contains('│')
            )
            .all(|line| line.starts_with("  │ "))
    );
}
// A streaming answer is drawn again only when its text, the width or the
// paint changed: a frame for a spinner tick reuses the last drawing.
#[test]
fn a_streaming_answer_is_drawn_again_only_once_it_changed() {
    let mut stream = crate::tui::view::Stream::default();
    let paint = Paint::new(true);
    let draws = std::cell::Cell::new(0);
    let draw = |stream: &crate::tui::view::Stream, width, paint: &Paint| {
        stream.answer_lines(width, paint, |text: &str| {
            draws.set(draws.get() + 1);
            vec![Line::from(format!("{width} {text}"))]
        })
    };
    stream.push("an answer");
    let first = draw(&stream, 20, &paint);
    assert_eq!(draw(&stream, 20, &paint), first);
    assert_eq!(draws.get(), 1, "nothing changed");
    stream.push(" grows");
    assert_eq!(
        draw(&stream, 20, &paint)[0].to_string(),
        "20 an answer grows"
    );
    let plain = Paint::new(false);
    draw(&stream, 30, &paint);
    draw(&stream, 30, &plain);
    draw(&stream, 30, &plain);
    assert_eq!(draws.get(), 4, "text, width, then paint");
    // The same length again, from a later answer, is not the old one.
    stream.replace("an answer grOws".into());
    assert_eq!(
        draw(&stream, 30, &plain)[0].to_string(),
        "30 an answer grOws"
    );
}

use ratatui::text::Line;

use super::harness::*;

#[test]
fn mouse_scroll_reaches_both_ends_across_tool_and_prose_rows() {
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

    let mut ui = test_ui(60, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    for i in 0..30 {
        view.surface
            .scrollback
            .extend(Row::answer(&format!("explanation {i}"), &ui.paint));
        view.surface.scrollback.push(Row::steps(
            Step::Tool(call("read", &format!("file{i}"), "", "")),
            true,
        ));
    }
    ui.flush(&lane, &mut view);
    let bottom = ui.drawn.clone();
    for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
        for _ in 0..view.surface.scrollback.len() * 2 {
            let event = Event::Mouse(MouseEvent {
                kind,
                column: 2,
                row: ui.regions.history.y,
                modifiers: KeyModifiers::NONE,
            });
            ui.key(&lane, &mut view, event, false);
            ui.flush(&lane, &mut view);
        }
        match kind {
            MouseEventKind::ScrollUp => assert_eq!(ui.drawn[0], "explanation 0"),
            MouseEventKind::ScrollDown => assert_eq!(ui.drawn, bottom),
            _ => unreachable!(),
        }
    }
}

#[test]
fn a_tool_separator_does_not_toggle_the_preceding_group() {
    use crate::tui::mouse::Target;
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    let mut ui = test_ui(60, 14);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback.push(Row::steps(
        Step::Tool(call("read", "a.rs", "", "contents")),
        true,
    ));
    view.surface
        .scrollback
        .extend(Row::answer("explanation", &ui.paint));
    ui.flush(&lane, &mut view);
    let i = ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::Separator(0, 0)))
        .expect("a separator after the group");
    assert!(ui.drawn[i].is_empty());
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        ui.key(
            &lane,
            &mut view,
            Event::Mouse(MouseEvent {
                kind,
                column: 1,
                row: ui.regions.history.y + i as u16,
                modifiers: KeyModifiers::NONE,
            }),
            false,
        );
    }
    assert_eq!(view.surface.scrollback[0].folded(), Some(true));
}

#[test]
fn a_stream_after_tools_keeps_its_scrolled_position_when_it_lands() {
    let mut ui = test_ui(40, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback.push(Row::steps(
        Step::Tool(call("read", "a.rs", "", "contents")),
        true,
    ));
    view.surface
        .stream
        .replace((1..=20).map(|i| format!("paragraph {i}\n\n")).collect());
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 6);
    ui.flush(&lane, &mut view);
    let top = ui.drawn[0].clone();
    assert!(top.contains("paragraph"));
    view.surface.stream.push("paragraph 21\n\n");
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top, "while the stream grows");
    ui.close(&mut view);
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top, "when the separated paragraph lands");
}

// A band the config named itself outlives the terminal's, and a terminal
// that would not say changes nothing. Read on every config adopted.
#[test]
fn a_configured_band_outlives_the_terminals() {
    let bg = Some((13, 17, 23));
    let mut theme = Theme::default();
    let derived = following_terminal(&theme, bg, None);
    assert_ne!(derived.prompt.panel.input, theme.prompt.panel.input);
    assert_eq!(
        following_terminal(&theme, None, None),
        theme,
        "no answer, no lift"
    );

    theme.prompt.panel.said = Color::Rgb(1, 2, 3);
    let mixed = following_terminal(&theme, bg, None);
    assert_eq!(mixed.prompt.panel.said, Color::Rgb(1, 2, 3));
    assert_ne!(mixed.prompt.panel.input, theme.prompt.panel.input);
}

// A row with no lines costs nothing; both walks must agree on where the
// others are — the back walk especially, which starts inside the last row.
#[test]
fn a_window_steps_over_rows_without_lines() {
    let paint = Paint::new(false);
    let mut rows = vec![Row::lines(Vec::new())];
    rows.extend(Row::answer("the answer", &paint));
    rows.push(Row::lines(Vec::new()));

    let forwards: Vec<String> = ScrollbackRows::new(&rows, &paint, 80).map(text).collect();
    assert_eq!(forwards, ["the answer"]);

    let backwards: Vec<String> = ScrollbackRows::new(&rows, &paint, 80)
        .rev()
        .map(text)
        .collect();
    assert_eq!(backwards, ["the answer"], "the back walk too");
}

#[test]
fn an_empty_scrollback_iterates_to_nothing() {
    // Both walks index `rows[0]` before comparing pointers; an empty
    // scrollback panics there if either forgets to guard it.
    let paint = Paint::new(false);
    let rows: Vec<String> = ScrollbackRows::new(&[], &paint, 80).map(text).collect();
    assert!(rows.is_empty());
    let back: Vec<String> = ScrollbackRows::new(&[], &paint, 80)
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
    let all: Vec<String> = ScrollbackRows::new(&rows, &paint, 80).map(text).collect();
    let rows = ScrollbackRows::new(&rows, &paint, 80);
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
    let back: Vec<String> = back.into_iter().rev().map(text).collect();
    assert_eq!(all.len(), 4);
    assert_eq!(front, all[..2], "the front walk meets the back one");
    assert_eq!(back, all[2..]);
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
    t.close_block(&mut scrollback, &Paint::new(false));
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
    t.close_block(&mut scrollback, &Paint::new(false));
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
    t.close_block(&mut scrollback, &Paint::new(false));
    t.join(&mut scrollback, Step::Tool(call("read", "a.rs", "", "")));
    streamed(&mut t, &mut scrollback);
    t.close_block(&mut scrollback, &Paint::new(false));
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
    t.close_block(&mut scrollback, &Paint::new(false));
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
    t.close_block(&mut scrollback, &Paint::new(false));
    let drawn = ScrollbackRows::new(&scrollback, &Paint::new(false), 80).count();
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
    t.close_block(&mut scrollback, &Paint::new(false));
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
    t.close_block(&mut scrollback, &Paint::new(false));

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
    let (shown, scrolled) = (ui.drawn.clone(), view.surface.scroll.by);

    // 25 columns at a 19-column width: one line, two rows.
    view.surface.scrollback.push(Row::notice("x".repeat(25)));
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn, shown, "the window stayed put");
    assert_eq!(view.surface.scroll.by, scrolled + 2, "on both wrapped rows");
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
    let mut scrollback: Scrollback =
        vec![Row::steps(Step::Tool(call("read", "a.rs", "", "")), false)].into();
    t.toggle_current(&mut scrollback);
    assert!(!t.birth_fold(), "the next group is born open");
}

// A page has its own window: scrolling it leaves the transcript's where it
// was, and closing it lands back on the rows the transcript was scrolled to.
#[test]
fn a_page_scrolls_itself_and_leaves_the_transcript_where_it_was() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=30).map(|n| Row::notice(format!("row {n}"))).collect();
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 4);
    ui.flush(&lane, &mut view);
    let (transcript, shown) = (view.surface.scroll, ui.drawn.clone());
    assert_eq!(transcript.by, 4, "the transcript is scrolled up");

    ui.open_page(
        (0..100)
            .map(|i| Line::from(format!("line {i}")).into())
            .collect(),
    );
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 6);
    ui.flush(&lane, &mut view);
    assert_eq!(
        view.surface.scroll, transcript,
        "the page scrolled the transcript's window"
    );

    assert!(ui.close_page(), "the page was up");
    ui.flush(&lane, &mut view);
    assert_eq!(view.surface.scroll, transcript, "still where it was left");
    assert_eq!(ui.drawn, shown, "and the rows are the ones it was showing");
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
    ui.open_answer(pi_store::listing::Listing::say(["/status answered"]));
    ui.flush(&lane, &mut view);
    assert!(dimmed(&ui), "under the panel");
    ui.read = None;
    ui.flush(&lane, &mut view);
    assert!(!dimmed(&ui), "the panel closed");
}

// A found line comes up midway under the page's pinned rule, `n` / `N`
// step round the lines holding it, and esc puts the view back where `/` found it.
#[test]
fn a_search_brings_each_line_holding_it_to_the_middle() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut ui = vim_ui_at(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.open_page(
        (0..100)
            .map(|i| Line::from(format!("line {i}")).into())
            .collect(),
    );
    let press = |ui: &mut crate::tui::Ui, view: &mut View, event| {
        ui.key(&lane, view, event, false);
        ui.flush(&lane, view);
        let rule = pi_store::icons::OVER_RULE;
        assert_eq!(
            ui.screen.test_buffer()[(ui.regions.history.x, ui.regions.history.y)].symbol(),
            rule,
            "the rule stays pinned"
        );
        current_hit(ui).map_or(String::new(), |(_, line)| line)
    };
    let key = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    ui.flush(&lane, &mut view);
    press(&mut ui, &mut view, typed('/'));
    assert_eq!(
        ui.drawn[1].trim_end(),
        "line 0",
        "nothing typed, nothing moved"
    );
    assert_eq!(press(&mut ui, &mut view, typed('4')), "line 4");
    let row = current_hit(&ui).map_or(0, |(row, _)| row as u16);
    let reversed = |ui: &crate::tui::Ui, col| {
        ui.screen.test_buffer()[(col, row)]
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
    assert_eq!(press(&mut ui, &mut view, typed('N')), "line 94");
    assert_eq!(count(&ui).as_deref(), Some("20/20"));
    let bottom = ui.drawn[1].clone();
    press(&mut ui, &mut view, typed('/'));
    assert_eq!(ui.drawn[1], bottom, "a fresh `/` leaves the view");
    press(&mut ui, &mut view, typed('x'));
    // The row the search took comes back, the view where this `/` found it.
    press(&mut ui, &mut view, key(KeyCode::Esc));
    assert_eq!(ui.drawn[1], bottom);
    assert!(ui.read.is_some(), "esc leaves the query, not the page");
}

// A page searches its own lines, not the transcript it was opened
// over, though both hold the query.
#[test]
fn a_command_page_searches_only_its_own_lines() {
    use ratatui::style::Modifier;
    let mut ui = vim_ui_at(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = vec![Row::notice("beta in the transcript")].into();
    let lines = ["alpha", "beta"].into_iter().map(String::from);
    let filler = (0..20).map(|n| format!("filler {n}"));
    ui.open_answer(pi_store::listing::Listing::say(lines.chain(filler)));
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
    assert!(ui.read.is_some());
}

// The query is edited with the line's own keys: `ctrl+w` takes a word,
// and on an empty query takes the `/` too.
#[test]
fn a_search_query_takes_the_line_editing_keys() {
    let mut ui = vim_ui_at(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.open_page(vec![Line::from("head").into()]);
    ui.flush(&lane, &mut view);
    for c in "/one two".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.key(&lane, &mut view, ctrl('w'), false);
    let query = |ui: &crate::tui::Ui| ui.search_prompt().map(|(q, _)| q);
    assert_eq!(query(&ui).as_deref(), Some("/one "));
    ui.key(&lane, &mut view, ctrl('u'), false);
    assert_eq!(query(&ui).as_deref(), Some("/"));
    assert!(ui.read.is_some());
    ui.key(&lane, &mut view, ctrl('w'), false);
    assert_eq!(query(&ui), None, "back past the `/`, as backspace goes");
}

// What is counted is what is drawn: an escape the frame drops holds no
// match, and a line wrapping past the window is stepped row by row.
#[test]
fn a_search_counts_what_is_drawn_and_reaches_rows_below_the_window() {
    let mut ui = vim_ui_at(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let long = (0..200)
        .map(|i| format!("w{i:03}"))
        .collect::<Vec<_>>()
        .join(" ");
    ui.open_page(vec![
        Line::from("head").into(),
        Line::from("\x1b[31mred\x1b[0m").into(),
        Line::from(long).into(),
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
    let mut ui = vim_ui_at(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.open_page(vec![Line::from("head").into()]);
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

// Pasted as the rows it searches were drawn: an escape's tail would never match.
#[test]
fn a_pasted_escape_does_not_reach_the_query() {
    let mut ui = vim_ui_at(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.open_page(vec![Line::from("head").into()]);
    ui.flush(&lane, &mut view);
    ui.key(&lane, &mut view, typed('/'), false);
    let paste = crate::tui::TermEvent::Paste("\x1b[31mone\x1b[0m".into());
    ui.key(&lane, &mut view, paste, false);
    assert_eq!(ui.search_prompt().map(|(q, _)| q).as_deref(), Some("/one"));
}

// ratatui draws the menu, and it prints an escape's tail as text.
#[test]
fn a_menu_entry_shows_no_escape_tail() {
    use ratatui::widgets::Widget;
    let ui = test_ui(40, 12);
    let entry = crate::tui::menu::MenuEntry::Completion(pi_core::input::commands::Candidate {
        show: "\x1b[31mred\x1b[0m".into(),
        line: "red".into(),
        help: "\x1b[1mbold\x1b[0m".into(),
        accept: pi_core::input::commands::Acceptance::Run,
        more: false,
        picked_only: false,
    });
    let table = ui.menu_table(&[entry], 0, 40);
    let area = ratatui::layout::Rect::new(0, 0, 40, 1);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    table.render(area, &mut buf);
    let row: String = (0..40).map(|x| buf[(x, 0)].symbol().to_string()).collect();
    assert_eq!(
        row.trim_end(),
        "red  bold",
        "the band, not a mark, says it is picked"
    );
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
    // Filler first: a hit centred on its jump is not on the view `gg` gives.
    let filler = (0..30).map(|_| "filler".to_string());
    ui.open_page(
        (filler.chain((0..100).map(|i| format!("line {i}"))))
            .map(|l| Line::from(l).into())
            .collect(),
    );
    let press = |ui: &mut crate::tui::Ui, view: &mut View, event| {
        ui.key(&lane, view, event, false);
        ui.flush(&lane, view);
        ui.search_prompt()
            .map(|(_, count)| count.trim().to_string())
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
    view.surface
        .stream
        .replace((1..=20).map(|n| format!("said {n}\n\n")).collect());
    ui.flush(&lane, &mut view);
    ui.scroll_view(&mut view, true, 6);
    ui.flush(&lane, &mut view);
    let top = ui.drawn[0].clone();
    assert!(top.contains("said"), "the top row is the stream's: {top:?}");
    view.surface.stream.push("said 21\n\nsaid 22\n\n");
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top, "as it grew");
    ui.close(&mut view);
    view.surface.scrollback.push(Row::notice("next"));
    ui.flush(&lane, &mut view);
    assert_eq!(ui.drawn[0], top, "as it landed");
}

// A hit in the answer still streaming comes up midway, and stays there as
// the answer grows under it; esc then goes back to the newest rows.
#[test]
fn a_search_hit_in_the_stream_holds_as_it_grows() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=5).map(|n| Row::notice(format!("row {n}"))).collect();
    view.surface
        .stream
        .replace((1..=20).map(|n| format!("said {n}\n\n")).collect());
    ui.flush(&lane, &mut view);
    ui.key(&lane, &mut view, ctrl('s'), false);
    for c in "said 3".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.flush(&lane, &mut view);
    let came = current_hit(&ui).expect("the hit came up");
    assert_eq!(came.1, "said 3");
    assert!(came.0 > 0, "midway, not on top");
    view.surface.stream.push("said 21\n\nsaid 22\n\n");
    ui.flush(&lane, &mut view);
    assert_eq!(current_hit(&ui), Some(came), "and stayed as it grew");
    // `/` found the view on the newest rows: esc goes back to them, grown.
    let esc = crate::tui::TermEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ));
    ui.key(&lane, &mut view, esc, false);
    ui.flush(&lane, &mut view);
    assert!(
        ui.drawn.iter().any(|r| r.trim() == "said 22"),
        "the newest rows"
    );
}

// A hit already on screen at the newest rows holds there too, rather than
// following the bottom off the screen.
#[test]
fn a_search_hit_on_the_newest_rows_holds_as_the_stream_grows() {
    let mut ui = test_ui(30, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback = (1..=5).map(|n| Row::notice(format!("row {n}"))).collect();
    view.surface
        .stream
        .replace((1..=3).map(|n| format!("said {n}\n\n")).collect());
    ui.flush(&lane, &mut view);
    ui.key(&lane, &mut view, ctrl('s'), false);
    for c in "said 2".chars() {
        ui.key(&lane, &mut view, typed(c), false);
    }
    ui.flush(&lane, &mut view);
    let at = |ui: &crate::tui::Ui| ui.drawn.iter().position(|r| r.trim() == "said 2");
    let found = at(&ui);
    assert!(found.is_some(), "on screen");
    view.surface.stream.push(
        &(4..=20)
            .map(|n| format!("said {n}\n\n"))
            .collect::<String>(),
    );
    ui.flush(&lane, &mut view);
    assert_eq!(at(&ui), found, "where it was found");
}

// Code wrapped over rows is taken whole from any of them: as the line holds
// it, not as the rows cut it.
#[test]
fn a_double_click_takes_wrapped_code_whole() {
    let mut ui = test_ui(20, 8);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let answer = Row::answer("run `cargo test --workspace` now", &Paint::new(true));
    view.surface.scrollback = answer.into();
    ui.flush(&lane, &mut view);
    let first = ui.drawn.iter().position(|r| r.starts_with("run")).unwrap();
    let at = crate::tui::select::Point {
        row: first + 1,
        col: 0,
    };
    let (sel, text) = ui.unit_selection(&view, at).expect("code under the click");
    assert_eq!(text, "cargo test --workspace");
    assert_eq!((sel.anchor.row, sel.anchor.col), (first, 4));
    assert_eq!(sel.head.row, first + 1);
    // Plain words around it are no stretch.
    let words = crate::tui::select::Point { row: first, col: 1 };
    assert!(ui.unit_selection(&view, words).is_none());
}

// Selection follows cell content in both the grid and vertical record layouts.
#[test]
fn a_double_click_takes_a_cell_of_a_wide_table() {
    for width in [30, 13] {
        let mut ui = test_ui(width, 40);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        let table = "| name | what it does |\n|---|---|\n| pi | a very long description of it |\n\
                     | 中文 | 宽 x |\n| --- | --- |\n";
        view.surface.scrollback = Row::answer(table, &Paint::new(true)).into();
        ui.flush(&lane, &mut view);
        let pick = |needle: &str| {
            let row = ui.drawn.iter().position(|r| r.contains(needle)).unwrap();
            let byte = ui.drawn[row].find(needle).unwrap();
            let col = pi_store::text::width(&ui.drawn[row][..byte]);
            ui.unit_selection(&view, crate::tui::select::Point { row, col })
                .expect("a cell under the click")
                .1
        };
        assert_eq!(pick("pi"), "pi");
        assert_eq!(pick("中文"), "中文");
        assert_eq!(pick("宽 x"), "宽 x");
        assert_eq!(pick("---"), "---");
        let description = pick("very");
        assert!(
            description.contains("very") && !description.contains('│'),
            "{description}"
        );
        assert!(
            !ui.drawn
                .iter()
                .any(|r| r.contains("needs ") || r.contains("|---|"))
        );
    }
}

// Typing lists every hit, a line holding two listed twice; moving down the
// list moves the view, and Enter keeps it there.
#[test]
fn the_search_lists_its_hits_and_enter_keeps_the_pick() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut ui = vim_ui_at(40, 16);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let lines = ["head", "a cat", "dog", "cat and cat", "bird", "the cat"];
    let filler = (0..30).map(|n| format!("filler {n}"));
    ui.open_page(
        (lines.into_iter().map(String::from).chain(filler))
            .map(|l| Line::from(l).into())
            .collect(),
    );
    let key = |code| crate::tui::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
    let mut press = |ui: &mut crate::tui::Ui, event| {
        ui.key(&lane, &mut view, event, false);
        // The list is laid out on the frame that answers.
        ui.flush(&lane, &mut view);
    };
    press(&mut ui, typed('/'));
    for c in "cat".chars() {
        press(&mut ui, typed(c));
    }
    let listed: Vec<String> = (ui.menu().iter()).map(|m| m.show().to_string()).collect();
    assert_eq!(listed, ["a cat", "cat and cat", "cat and cat", "the cat"]);
    press(&mut ui, key(KeyCode::Down));
    let followed = current_hit(&ui);
    assert_eq!(
        followed.as_ref().map(|(_, line)| line.as_str()),
        Some("cat and cat"),
        "the view followed"
    );
    press(&mut ui, key(KeyCode::Down));
    let count = ui.search_prompt().map(|(_, count)| count);
    assert_eq!(count.as_deref(), Some("  3/4"), "the line's second hit");
    press(&mut ui, key(KeyCode::Enter));
    assert!(ui.menu().is_empty(), "the list went with the typing");
    let stayed = current_hit(&ui).map(|(_, line)| line);
    assert_eq!(
        stayed.as_deref(),
        Some("cat and cat"),
        "and the pick stayed"
    );
}

#[test]
fn the_rewind_list_follows_prompts_without_changing_the_transcript() {
    use agent::session::{Prompt, Session};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use pi_core::input::commands::Choice;

    for rebuilt in [false, true] {
        let mut ui = test_ui(48, 20);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        let mut session = Session::new();
        for n in 0..5 {
            let prompt = if n == 2 {
                "!pwd"
            } else {
                "the same prompt\nwith another line"
            };
            ui.submit(&mut view, prompt);
            let id = if n == 2 {
                session.push_bash(Prompt {
                    text: "the command output".into(),
                    shown: Some(prompt.into()),
                    images: Vec::new(),
                    relayed: None,
                    context: Vec::new(),
                })
            } else {
                session.prompt(prompt)
            };
            view.surface.scrollback.bind_prompt(id);
            let answer = (0..20)
                .map(|i| format!("answer {n}, line {i}\n"))
                .collect::<String>();
            view.surface
                .scrollback
                .extend(Row::answer(&answer, &ui.paint));
            session.push_assistant(vec![llm::message::AssistantContent::Text(
                llm::message::Text { text: answer },
            )]);
        }
        if rebuilt {
            ui.rebuild(&mut view, &session, lane.root());
        }
        let entries = format!("{:?}", session.entries());
        ui.said = session
            .rewind_nodes()
            .iter()
            .map(|node| Choice {
                name: node.id().0.to_string(),
                note: node.show().to_string(),
            })
            .collect();
        let ids: Vec<_> = session.rewind_nodes().iter().map(|n| n.id()).collect();
        ui.open_page(vec![ratatui::text::Line::from("a command's answer").into()]);
        ui.seed("/rewind ");
        ui.flush(&lane, &mut view);
        assert!(ui.page().is_none(), "the transcript is visible");

        let assert_visible =
            |ui: &crate::tui::Ui, view: &View, id| {
                let visible =
                    ui.regions.history.height.saturating_sub(
                        ui.regions.history.intersection(ui.regions.overlay()).height,
                    ) as usize;
                let first = view
                    .surface
                    .scrollback
                    .iter()
                    .position(|row| row.entry() == Some(id))
                    .unwrap();
                assert!(
                    ui.row_targets[..visible]
                        .iter()
                        .any(|target| matches!(target,
                            crate::tui::mouse::Target::Scrollback(idx, 0) if *idx == first
                        )),
                    "the selected prompt is above the list (rebuilt={rebuilt})"
                );
            };
        assert_visible(&ui, &view, ids[4]);
        for id in ids[..4].iter().rev() {
            ui.key(
                &lane,
                &mut view,
                crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                false,
            );
            ui.flush(&lane, &mut view);
            assert_visible(&ui, &view, *id);
        }
        ui.key(
            &lane,
            &mut view,
            crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            false,
        );
        ui.flush(&lane, &mut view);
        assert_visible(&ui, &view, ids[1]);
        ui.editor.set_line("/rewind !pwd");
        ui.flush(&lane, &mut view);
        assert_eq!(ui.menu().len(), 1);
        assert_visible(&ui, &view, ids[2]);
        ui.key(
            &lane,
            &mut view,
            crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            false,
        );
        ui.flush(&lane, &mut view);
        assert!(ui.menu().is_empty());
        assert_eq!(
            format!("{:?}", session.entries()),
            entries,
            "browsing leaves history intact"
        );
    }
}

// A page is read over the whole conversation: the rows the executions held are
// its own while it is up, so a click there lands on the page, not an execution.
#[test]
fn a_page_lies_over_the_commands() {
    use pi_core::driver::background::Invocation;
    let mut ui = test_ui(80, 24);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.executions = vec![background_command(1, Invocation::shell("serve"))];
    ui.flush(&lane, &mut view);
    assert_eq!(ui.regions.executions.height, 1, "the execution has its row");

    ui.open_page(
        (0..60)
            .map(|i| Line::from(format!("line {i}")).into())
            .collect(),
    );
    ui.flush(&lane, &mut view);
    assert_eq!(
        ui.regions.history.bottom(),
        ui.regions.statusline.y,
        "the page covers the commands, gap and empty input up to the statusline"
    );
    let last = ui.regions.history.bottom() - 1;
    assert!(ui.jump_at(last), "the bottom hint jumps to the page's end");
    assert!((0..24).all(|row| ui.command_at(row).is_none()));
    ui.on_mouse_click(&mut view, 0, last);
    ui.flush(&lane, &mut view);
    assert!(!ui.jump_at(last), "no hint remains at the end");
    assert_eq!(
        ui.drawn[(last - ui.regions.history.y) as usize],
        "line 59",
        "the last content row replaces the hint"
    );
}

// A group of `n` calls, each on one line.
fn step_calls(n: usize) -> Row {
    let mut steps = (1..=n).map(|i| Step::Tool(call("read", &format!("file{i}.rs"), "", "")));
    let mut row = Row::steps(steps.next().expect("a group of one"), true);
    for step in steps {
        assert!(row.join(step).is_ok());
    }
    row
}

// The bar's cells, as the colour each is drawn in: lit, then gone grey.
fn bar_cells(row: &Row, i: usize, paint: &Paint, width: usize) -> (usize, usize) {
    let line = row.line(i, paint, width).0;
    let lit = crate::sgr::style_to_ratatui(&paint.theme.heading).fg;
    let dim = crate::sgr::style_to_ratatui(&paint.theme.muted).fg;
    let cell = icons::BAR_CELL.chars().next().expect("a cell");
    let (mut on, mut off) = (0, 0);
    for span in &line.spans {
        let n = span.content.chars().filter(|c| *c == cell).count();
        if span.style.fg == lit {
            on += n;
        } else if span.style.fg == dim {
            off += n;
        }
    }
    (on, off)
}

// Whether any of the row's lines carries a bar cell.
fn carries_bar(row: &Row, paint: &Paint, width: usize) -> bool {
    let text = |i| row.line(i, paint, width).0.to_string();
    (0..row.len()).any(|i| text(i).contains(icons::BAR_CELL))
}

// A group taller than its window shows its newest steps over a bar that
// says where the window sits; every step is still there.
#[test]
fn a_window_shows_its_newest_steps_over_its_bar() {
    let paint = Paint::new(true);
    let width = 80;
    let mut row = step_calls(30);
    // The window is its own height whatever the history holds: steps and a bar.
    row.prepare_layout(&paint, width, 40);
    let lines: Vec<String> = (0..row.len())
        .map(|i| row.line(i, &paint, width).0.to_string())
        .collect();
    let bar = row.len() - 1;
    assert!(lines[bar].contains("30"), "the bar says what it holds");
    assert_eq!(
        bar_cells(&row, bar, &paint, width),
        (10, 0),
        "at the bottom"
    );
    assert!(
        lines[bar - 1].contains("file30.rs"),
        "the newest step sits on the bar: {lines:?}"
    );
    // The window keeps its own rows whatever the history holds: a room of
    // eight fills with its steps and the bar, not with seven and nothing.
    let mut taller = step_calls(30);
    let room = 8;
    taller.prepare_layout(&paint, width, room);
    assert_eq!(
        taller.len(),
        room,
        "the bar's row comes out of the history's, not the window's"
    );

    // Scrolled to the top, every cell has gone grey.
    for _ in 0..64 {
        if !row.scroll_window(true, 1) {
            break;
        }
    }
    assert_eq!(bar_cells(&row, bar, &paint, width), (0, 10), "at the top");

    // One step up from the bottom greys the last cell alone.
    for _ in 0..64 {
        if !row.scroll_window(false, 1) {
            break;
        }
    }
    assert!(row.scroll_window(true, 1));
    assert_eq!(bar_cells(&row, bar, &paint, width), (9, 1));

    // A group that fits the window has no bar under it.
    let mut short = step_calls(5);
    short.prepare_layout(&paint, width, 40);
    assert_eq!(short.len(), 5);
    assert!(!carries_bar(&short, &paint, width));
}

// The wheel turns the window while it can move and says so, so the frame
// knows when the turn belongs to the transcript again.
#[test]
fn a_window_scrolls_by_lines_and_stops_at_its_ends() {
    let paint = Paint::new(false);
    let width = 80;
    let mut row = step_calls(30);
    row.prepare_layout(&paint, width, 40);

    let last = row.len() - 2;
    let bottom = row.line(last, &paint, width).0.to_string();
    assert!(row.scroll_window(true, 1), "the wheel turns it");
    assert_ne!(row.line(last, &paint, width).0.to_string(), bottom);

    for _ in 0..64 {
        if !row.scroll_window(true, 1) {
            break;
        }
    }
    assert!(
        row.line(0, &paint, width)
            .0
            .to_string()
            .contains("file1.rs")
    );
    assert!(
        !row.scroll_window(true, 1),
        "at the top: the transcript's turn"
    );

    for _ in 0..64 {
        if !row.scroll_window(false, 1) {
            break;
        }
    }
    assert!(
        row.line(last, &paint, width)
            .0
            .to_string()
            .contains("file30.rs")
    );
    assert!(!row.scroll_window(false, 1));
}

// A step whose lines the window holds opens in place, from its own line.
#[test]
fn a_step_that_fits_opens_in_the_window() {
    let paint = Paint::new(false);
    let width = 80;
    let mut row = step_calls(6);
    let ran = call("bash", "cat a", "cat a", "one\ntwo\n");
    assert!(row.join(Step::Tool(ran)).is_ok());
    row.prepare_layout(&paint, width, 40);
    let head = row.len() - 1;
    assert!(
        row.toggle_at(head, &paint, width, usize::MAX),
        "it fits, so it opens there"
    );
    let lines: Vec<String> = (0..row.len())
        .map(|i| row.line(i, &paint, width).0.to_string())
        .collect();
    let head = lines
        .iter()
        .position(|line| line.contains("cat a"))
        .expect("the opened call keeps its line");
    assert!(lines[head + 1].is_empty(), "a blank under it: {lines:?}");
    assert!(
        lines[head + 2].contains("one"),
        "its output sits under the blank: {lines:?}"
    );
}

// A step taller than the window stays closed and is read on a page.
#[test]
fn a_step_too_tall_for_the_window_opens_on_a_page() {
    let paint = Paint::new(false);
    let width = 80;
    let body: String = (1..=15).map(|i| format!("line {i}\n")).collect();
    let big = call("bash", "cat big", "cat big", &body);
    let mut row = step_calls(1);
    assert!(row.join(Step::Tool(big)).is_ok());
    row.prepare_layout(&paint, width, 40);
    let head = (0..row.len())
        .find(|&i| row.line(i, &paint, width).0.to_string().contains("cat big"))
        .expect("the call keeps its line");

    assert!(
        !row.toggle_at(head, &paint, width, usize::MAX),
        "the window cannot hold it"
    );
    let page = row
        .page_at(head, &paint, width, usize::MAX)
        .expect("the page reads it");
    assert_eq!(page.len(), 17, "its own line, a blank, and what it printed");
    assert!(
        row.line(head, &paint, width)
            .0
            .to_string()
            .contains("cat big"),
        "it stays closed"
    );
}

// A background turn's text that outgrows the rows a fold may take is read on
// a page: the row never cuts itself short for a count to click through.
#[test]
fn a_relayed_turn_too_tall_for_its_rows_opens_on_a_page() {
    let paint = Paint::new(false);
    let width = 80;
    let long: String = (1..=30).map(|i| format!("line {i}\n\n")).collect();
    let mut row = Row::relayed("automatic input", &long, 0, &paint);
    row.prepare_layout(&paint, width, 20);
    assert!(
        !row.toggle_at(0, &paint, width, usize::MAX),
        "it does not open in place"
    );
    assert_eq!(row.len(), 1, "it stays one line");
    let page = row
        .page_at(0, &paint, width, usize::MAX)
        .expect("the page reads it");
    assert!(
        page.len() > 10,
        "the whole of it, past the rows a fold takes"
    );

    // One the rows hold opens in place, with no count at its end.
    let short: String = (1..=4).map(|i| format!("line {i}\n\n")).collect();
    let mut row = Row::relayed("automatic input", &short, 0, &paint);
    row.prepare_layout(&paint, width, 20);
    assert!(
        row.toggle_at(0, &paint, width, usize::MAX),
        "it fits, so it opens"
    );
    assert!(row.len() > 1, "its text reads under its own line");
    assert!(
        !row.has_more(row.len() - 1, width),
        "no count stands between the row and its text"
    );
}

// A turn opened to be read loses the aside's grey: its own line reads as the
// message it holds, on the row and on its page.
#[test]
fn an_opened_turn_reads_its_own_line_plain() {
    let paint = Paint::new(true);
    let text: String = (1..=3).map(|i| format!("line {i}\n\n")).collect();
    let mut row = Row::relayed("automatic input", &text, 0, &paint);
    row.prepare_layout(&paint, 80, 40);
    let muted = crate::sgr::style_to_ratatui(&paint.theme.muted);
    let plain = crate::sgr::style_to_ratatui(&paint.theme.input);
    assert_eq!(row.line(0, &paint, 80).0.spans[0].style, muted, "shut");
    assert!(row.toggle_at(0, &paint, 80, usize::MAX));
    assert_eq!(row.line(0, &paint, 80).0.spans[0].style, plain, "opened");
    assert_eq!(
        row.line(1, &paint, 80).0.to_string(),
        "",
        "a blank under it"
    );
    let page = row.opening(0, &paint).expect("a page");
    assert_eq!(page[1].line.to_string(), "", "a blank under it there too");
    assert_eq!(page[0].line.spans[0].style, plain, "read on its page");
}

// Under a page the transcript stands back: the wheel turns what is shown,
// not the window row the pointer happens to lie over.
#[test]
fn the_wheel_under_a_page_leaves_the_window_alone() {
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

    let mut ui = test_ui(60, 16);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback.push(step_calls(30));
    ui.flush(&lane, &mut view);
    let window_before = view.surface.scrollback[0]
        .line(0, &ui.paint, 60)
        .0
        .to_string();

    // A page short enough to leave transcript rows above it, where the
    // pointer sits.
    ui.open_page(
        (0..3)
            .map(|i| crate::tui::browse::PageLine::from(Line::from(format!("page {i}"))))
            .collect(),
    );
    ui.flush(&lane, &mut view);
    let event = Event::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 2,
        row: ui.regions.history.y,
        modifiers: KeyModifiers::NONE,
    });
    ui.key(&lane, &mut view, event, false);
    ui.flush(&lane, &mut view);

    assert_eq!(
        view.surface.scrollback[0]
            .line(0, &ui.paint, 60)
            .0
            .to_string(),
        window_before,
        "the window under it stayed put"
    );
}

// What is already open closes on a click, even once the rows it may take
// have shrunk: the page is for what cannot open, not for what is open.
#[test]
fn an_open_turn_that_no_longer_fits_still_closes() {
    let paint = Paint::new(false);
    let width = 80;
    let text: String = (1..=6).map(|i| format!("line {i}\n\n")).collect();
    let mut row = Row::relayed("automatic input", &text, 0, &paint);
    row.prepare_layout(&paint, width, 40);
    assert!(
        row.toggle_at(0, &paint, width, usize::MAX),
        "a tall enough window opens it"
    );
    row.prepare_layout(&paint, width, 8);
    assert!(
        row.page_at(0, &paint, width, usize::MAX).is_none(),
        "an open row is not read on a page"
    );
    assert!(
        row.toggle_at(0, &paint, width, usize::MAX),
        "the click closes it"
    );
    assert_eq!(row.len(), 1);
}

// The wheel turns a group window by its bar alone: every other row of the
// group belongs to the transcript.
#[test]
fn the_wheel_turns_a_window_by_its_bar_alone() {
    use crate::tui::mouse::Target;
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

    let mut ui = test_ui(60, 16);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    for i in 0..20 {
        view.surface
            .scrollback
            .extend(Row::answer(&format!("line {i}"), &ui.paint));
    }
    view.surface.scrollback.push(step_calls(30));
    let group = view.surface.scrollback.len() - 1;
    ui.flush(&lane, &mut view);
    // The screen row of the group's bar, or of one of its steps.
    let at = |ui: &crate::tui::Ui, view: &View, bar: bool| {
        (0..ui.regions.history.height)
            .map(|d| ui.regions.history.y + d)
            .find(|&r| {
                matches!(
                    ui.target_at(r),
                    Some(Target::Scrollback(idx, line))
                        if idx == group && view.surface.scrollback[group].is_bar(line) == bar
                )
            })
            .expect("the group takes some rows")
    };
    // Point at the cell, then turn the wheel there: a bar takes the wheel
    // only where the reader pointed.
    let wheel = |lane: &pi_core::core::lane::Lane,
                 ui: &mut crate::tui::Ui,
                 view: &mut View,
                 col: u16,
                 row: u16,
                 up: bool| {
        ui.on_mouse_move(view, col, row);
        let kind = match up {
            true => MouseEventKind::ScrollUp,
            false => MouseEventKind::ScrollDown,
        };
        let event = Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        });
        ui.key(lane, view, event, false);
        ui.flush(lane, view);
    };
    let head = |view: &View| {
        view.surface.scrollback[group]
            .line(0, &Paint::new(false), 60)
            .0
            .to_string()
    };

    // The bar's cells and count are the handle; the blank rest of that row
    // scrolls the transcript like any other.
    let before = head(&view);
    let bar = at(&ui, &view, true);
    wheel(&lane, &mut ui, &mut view, 2, bar, true);
    assert_ne!(head(&view), before, "the bar turns the window");

    let window = head(&view);
    let bar = at(&ui, &view, true);
    wheel(&lane, &mut ui, &mut view, 40, bar, true);
    assert_eq!(
        head(&view),
        window,
        "the blank rest of the bar scrolls history"
    );

    // Back to the newest rows, where the group shows again.
    for _ in 0..64 {
        let top = ui.regions.history.y;
        wheel(&lane, &mut ui, &mut view, 40, top, false);
    }

    // At the window's ends the bar keeps the wheel: history stays put.
    for _ in 0..64 {
        let bar = at(&ui, &view, true);
        wheel(&lane, &mut ui, &mut view, 2, bar, true);
    }
    let top = ui.drawn.clone();
    let bar = at(&ui, &view, true);
    wheel(&lane, &mut ui, &mut view, 2, bar, true);
    assert_eq!(ui.drawn, top, "at the top the history does not move");
    for _ in 0..64 {
        let bar = at(&ui, &view, true);
        wheel(&lane, &mut ui, &mut view, 2, bar, false);
    }
    let bottom = ui.drawn.clone();
    let bar = at(&ui, &view, true);
    wheel(&lane, &mut ui, &mut view, 2, bar, false);
    assert_eq!(ui.drawn, bottom, "at the bottom too");
}

// The window is its own height whatever the history holds, and what fits it
// exactly has no bar under it.
#[test]
fn a_window_that_fits_exactly_carries_no_bar() {
    let paint = Paint::new(false);
    let width = 80;
    let mut exact = step_calls(7);
    exact.prepare_layout(&paint, width, 40);
    assert_eq!(exact.len(), 7, "seven steps fill the window");
    assert!(!carries_bar(&exact, &paint, width));

    // A frame shorter than the window gives the window its rows alone.
    let mut short = step_calls(10);
    short.prepare_layout(&paint, width, 6);
    assert_eq!(short.len(), 6, "the frame decides when it is shorter");
}

// A bar that scrolls under a still pointer belongs to history: only a bar the
// reader pointed at takes the wheel.
#[test]
fn a_bar_under_a_still_pointer_stays_with_history() {
    use crate::tui::mouse::Target;
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};

    let mut ui = test_ui(60, 16);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback.push(step_calls(30));
    for i in 0..20 {
        view.surface
            .scrollback
            .extend(Row::answer(&format!("line {i}"), &ui.paint));
    }
    view.surface.scrollback.push(step_calls(30));
    ui.flush(&lane, &mut view);

    // The wheel turns where the pointer was put; a scroll moves content under
    // it without pointing at anything.
    let turn = |lane: &pi_core::core::lane::Lane,
                ui: &mut crate::tui::Ui,
                view: &mut View,
                col: u16,
                row: u16,
                up: bool| {
        let kind = match up {
            true => MouseEventKind::ScrollUp,
            false => MouseEventKind::ScrollDown,
        };
        let event = Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        });
        ui.key(lane, view, event, false);
        ui.flush(lane, view);
    };

    // Park the pointer, then scroll the older group's bar under it.
    let park = ui.regions.history.y + 4;
    ui.on_mouse_move(&mut view, 40, park);
    let mut landed = false;
    for _ in 0..64 {
        let bar = matches!(ui.target_at(park), Some(Target::Scrollback(0, line))
            if view.surface.scrollback[0].is_bar(line));
        if bar {
            landed = true;
            break;
        }
        turn(&lane, &mut ui, &mut view, 40, park, true);
    }
    assert!(landed, "the older window's bar reached the pointer");

    let window = view.surface.scrollback[0]
        .line(0, &Paint::new(false), 60)
        .0
        .to_string();
    turn(&lane, &mut ui, &mut view, 2, park, true);
    assert_eq!(
        view.surface.scrollback[0]
            .line(0, &Paint::new(false), 60)
            .0
            .to_string(),
        window,
        "a bar it did not point at leaves the window alone"
    );
}

// A turn whose own line wraps once opened is measured as it reads opened:
// the cut would spend those rows, so the budget is spent on them too.
#[test]
fn an_opened_line_that_wraps_counts_against_the_budget() {
    let paint = Paint::new(false);
    let width = 40;
    let label = format!("automatic input {}", "of a long run ".repeat(4));
    let mut row = Row::relayed(&label, "one line\n", 0, &paint);
    // Budget 3: shut, the line is one row and the turn would open; opened,
    // the line wraps to two and the turn no longer fits.
    row.prepare_layout(&paint, width, 6);
    assert!(
        row.page_at(0, &paint, width, usize::MAX).is_some(),
        "the wrapped line is counted"
    );
}

// A question the window left behind heads it; one it is scrolled past stays
// where it is, since a heading over older rows would name the wrong turn.
#[test]
fn a_question_the_window_left_behind_pins_to_the_top_row() {
    let mut ui = test_ui(40, 8);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let paint = Paint::new(false);
    let mut rows = Row::prompt("first question", 0, &paint);
    rows.extend((1..=10).map(|n| Row::notice(format!("row {n}"))));
    rows.extend(Row::prompt("second question", 1, &paint));
    rows.extend((11..=20).map(|n| Row::notice(format!("row {n}"))));
    view.surface.scrollback = rows.into();
    ui.flush(&lane, &mut view);
    let height = ui.regions.history.height as usize;
    assert!(
        ui.drawn[0].contains("second question"),
        "{:?}",
        &ui.drawn[..height]
    );
    assert_eq!(
        ui.drawn[height - 1].trim_end(),
        "row 20",
        "the pin takes a row above, not the newest one"
    );
    assert!(
        ui.top_place().is_some(),
        "the pin names a row still: a `/` search starts from the top of the view"
    );

    // Ten rows up from the newest puts the question on the window's last row:
    // below what it shows, not above it.
    view.surface.scroll.by = 10;
    ui.flush(&lane, &mut view);
    assert!(
        ui.drawn[..height]
            .iter()
            .any(|row| row.contains("second question")),
        "still in view below: {:?}",
        &ui.drawn[..height]
    );
    assert!(
        !ui.drawn[0].contains("question"),
        "the older rows keep the top: {:?}",
        &ui.drawn[..height]
    );
}

// A question pinned over the top row is not where the window opens: scrolling
// from the newest rows must not anchor on it and jump back up the transcript.
#[test]
fn a_pinned_question_leaves_the_newest_rows_where_they_are() {
    let mut ui = vim_ui_at(30, 12);
    ui.vim = Some(pi_store::keys::Mode::Normal);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.surface.scrollback.append(Row::prompt(
        "a question that wraps because it is long",
        0,
        &ui.paint,
    ));
    for n in 1..=40 {
        view.surface.scrollback.extend(Row::answer(
            &format!("answer {n} with text long enough to wrap once"),
            &ui.paint,
        ));
    }
    ui.flush(&lane, &mut view);
    let bottom = ui.drawn.clone();
    assert_eq!(view.surface.scroll, crate::tui::view::Scroll::newest());
    for key in ['J', 'J', 'j'] {
        ui.key(&lane, &mut view, typed(key), false);
        ui.flush(&lane, &mut view);
        assert_eq!(
            view.surface.scroll,
            crate::tui::view::Scroll::newest(),
            "{key} on the newest rows leaves them newest"
        );
        assert_eq!(ui.drawn, bottom, "{key} drew another window");
    }
    ui.key(&lane, &mut view, typed('K'), false);
    ui.flush(&lane, &mut view);
    assert!(!view.surface.scroll.newest_rows(), "K scrolls up");
    ui.key(&lane, &mut view, typed('J'), false);
    ui.flush(&lane, &mut view);
    assert_eq!(view.surface.scroll, crate::tui::view::Scroll::newest());
    assert_eq!(ui.drawn, bottom, "and J takes it back down");
}
