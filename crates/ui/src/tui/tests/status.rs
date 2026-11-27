use crate::tui::View;
use crate::tui::screen::plain;
use pi_store::statusline::Field;
use std::time::{Duration, Instant};

use super::harness::*;

fn facts(model: &str) -> crate::tui::statusline::Facts {
    crate::tui::statusline::Facts::from_snapshot(pi_core::core::meter::Snapshot {
        model_name: model.into(),
        ..Default::default()
    })
}

// A flash stands at the bar's right end, and the bar keeps its row: the
// model the lane runs is not the thing a keypress's answer should hide.
#[test]
fn a_flash_stands_beside_the_bar_rather_than_over_it() {
    let mut ui = test_ui(60, 24);
    ui.flash("nothing to rewind to");
    let row = plain(
        &ui.statusline_lines(&facts("deepseek"), 60, std::path::Path::new("/nowhere"))
            .1[0]
            .line,
    );
    assert!(row.starts_with("deepseek"), "{row:?}");
    assert!(row.ends_with("nothing to rewind to"), "{row:?}");
    assert_eq!(row.chars().count(), 60);

    // Too narrow for both: the flash keeps half the row, cut with its `…`.
    let row = plain(
        &ui.statusline_lines(
            &facts("a-rather-long-model-name"),
            30,
            std::path::Path::new("/nowhere"),
        )
        .1[0]
            .line,
    );
    assert!(row.ends_with('…'), "{row:?}");
    assert!(pi_store::text::width(&row) <= 30, "{row:?}");
}

// The time a run took outlives it: idle keeps the clock it stopped on rather
// than dropping the `elapsed` part from the row.
#[test]
fn an_ended_run_keeps_its_time() {
    let dir = tempfile::tempdir().expect("a checkout");
    let lane = running_lane(dir.path());
    let mut view = View::default();
    view.state.work = a_turn(false);
    view.state.work.as_mut().expect("a running turn").since =
        Instant::now() - Duration::from_secs(2);

    let facts = crate::tui::statusline::Facts::of(&lane, &view, None, Vec::new(), &[], Vec::new());
    assert_eq!(facts.value(Field::State).as_deref(), Some("working"));
    assert!(facts.value(Field::Elapsed).is_some(), "timed while working");

    view.state.settle(crate::tui::view::Ending::Ok);
    let facts = crate::tui::statusline::Facts::of(&lane, &view, None, Vec::new(), &[], Vec::new());
    assert_eq!(facts.value(Field::State).as_deref(), Some("idle"));
    assert!(
        facts.value(Field::Elapsed).is_some(),
        "the ended run's time stayed on the row"
    );
}

// A lone line said again folds into the row before it; a block, a QR's
// rows say, keeps every row even where two of its lines match.
#[test]
fn only_a_lone_line_folds_into_the_one_before() {
    let ui = test_ui(80, 24);
    let mut view = View::default();
    ui.say(&mut view, "reloaded");
    ui.say(&mut view, "reloaded");
    assert_eq!(drawn_rows(&view).len(), 1);

    let mut view = View::default();
    ui.say(&mut view, "██  ██\n██  ██\n\n\n██  ██");
    assert_eq!(drawn_rows(&view).len(), 5);
}

// A script's SGR runs colour their own text and add up: each run builds on the
// one before, a bare reset returns to the item's style, and anything that is
// not SGR is dropped.
#[test]
fn script_colours_add_up() {
    use crate::tui::statusline::{sgr_spans, spoken_line};
    use ratatui::style::{Color, Modifier, Style};
    let base = Style::new().fg(Color::Indexed(7));
    let spans = sgr_spans("\x1b[1mbold\x1b[31mred\x1b[0mplain", base, true);
    let shown: Vec<(&str, Style)> = spans
        .iter()
        .map(|s| (s.content.as_ref(), s.style))
        .collect();
    assert_eq!(shown.len(), 3, "{shown:?}");
    assert_eq!(shown[0].1, base.add_modifier(Modifier::BOLD));
    assert_eq!(shown[1].1, base.fg(Color::Red).add_modifier(Modifier::BOLD));
    assert_eq!(shown[2].1, base);
    // An OSC's payload is not text.
    let spans = sgr_spans("\x1b]0;t\x07ok", base, true);
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].content, "ok");
    assert_eq!(spoken_line("\x1b[31m\x1b[0m "), None);
}
