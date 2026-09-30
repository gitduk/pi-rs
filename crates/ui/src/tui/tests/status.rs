use crate::tui::View;
use crate::tui::screen::plain;

use super::harness::*;

fn facts(model: &str) -> crate::tui::bar::Facts {
    crate::tui::bar::Facts {
        model: model.into(),
        effort: llm::request::Effort::Off,
        tier: pi_core::args::TierArg::Exec,
        running: false,
        root: std::path::PathBuf::new(),
        worktree: None,
        ctx: None,
        spent: 0.0,
    }
}

// A flash stands at the bar's right end, and the bar keeps its row: the
// model the lane runs is not the thing a keypress's answer should hide.
#[test]
fn a_flash_stands_beside_the_bar_rather_than_over_it() {
    let mut ui = test_ui(60, 24);
    ui.flash("nothing to rewind to");
    let row = plain(&ui.bar_lines(&facts("deepseek"), &Default::default(), 60)[0]);
    assert!(row.starts_with("deepseek"), "{row:?}");
    assert!(row.ends_with("nothing to rewind to"), "{row:?}");
    assert_eq!(row.chars().count(), 60);

    // Too narrow for both: the flash keeps half the row, cut with its `…`.
    let row = plain(&ui.bar_lines(&facts("a-rather-long-model-name"), &Default::default(), 30)[0]);
    assert!(row.ends_with('…'), "{row:?}");
    assert!(
        unicode_width::UnicodeWidthStr::width(row.as_str()) <= 30,
        "{row:?}"
    );
}

// A retry is the run's state, so it waits on the status line; its reason —
// often a whole error body — lands as a row of its own, and the next event
// of any other kind ends the wait.
#[test]
fn a_retry_waits_on_the_status_line_and_leaves_its_reason_in_a_row() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::Retrying {
            attempt: 2,
            delay_ms: 3000,
            reason: "502 Bad Gateway: upstream connect error".into(),
        },
    );
    let (live, _) = ui.live(&lane, &view, false, std::time::Instant::now());
    let status = plain(live.last().expect("a status line"));
    assert!(status.ends_with("retry 2 in 3.00s"), "{status:?}");
    assert_eq!(
        drawn_rows(&view),
        vec!["502 Bad Gateway: upstream connect error".to_string()]
    );

    ui.on_event(&mut lane, &mut view, agent::Event::TextDelta("ok".into()));
    let (live, _) = ui.live(&lane, &view, false, std::time::Instant::now());
    let status = plain(live.last().expect("a status line"));
    assert!(!status.contains("retry"), "{status:?}");
}

// A running line with nothing to say goes, rather than leaving a blank row
// between the stream and the bar.
#[test]
fn a_status_line_with_nothing_to_say_draws_no_row() {
    let mut ui = test_ui(80, 24);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    view.state.started = Some(std::time::Instant::now());
    let (shown, _) = ui.live(&lane, &view, false, std::time::Instant::now());
    assert_eq!(shown.len(), 1, "the clock has something to say");
    ui.status = Vec::new();
    let (gone, _) = ui.live(&lane, &view, false, std::time::Instant::now());
    assert!(gone.is_empty(), "{gone:?}");
}
