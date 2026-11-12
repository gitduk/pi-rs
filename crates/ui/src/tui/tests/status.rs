use crate::tui::View;
use crate::tui::screen::plain;

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
    let row = plain(&ui.statusline_lines(&facts("deepseek"), 60).1[0].line);
    assert!(row.starts_with("deepseek"), "{row:?}");
    assert!(row.ends_with("nothing to rewind to"), "{row:?}");
    assert_eq!(row.chars().count(), 60);

    // Too narrow for both: the flash keeps half the row, cut with its `…`.
    let row = plain(
        &ui.statusline_lines(&facts("a-rather-long-model-name"), 30)
            .1[0]
            .line,
    );
    assert!(row.ends_with('…'), "{row:?}");
    assert!(pi_store::text::width(&row) <= 30, "{row:?}");
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
