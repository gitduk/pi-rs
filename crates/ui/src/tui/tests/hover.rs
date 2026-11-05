use crate::tui::View;
use crate::tui::row::Row;
use ratatui::style::Modifier;
use ratatui::text::Line;

use super::harness::*;

fn bold_at(ui: &crate::tui::Ui, col: u16, row: u16) -> bool {
    ui.screen.test_buffer()[(col, row)]
        .modifier
        .contains(Modifier::BOLD)
}

fn text_at(ui: &crate::tui::Ui, row: u16) -> String {
    let buf = ui.screen.test_buffer();
    (0..buf.area.width)
        .map(|col| buf[(col, row)].symbol())
        .collect()
}

fn one_line_row(i: u64) -> Row {
    Row::thinking(i, vec![Line::from(format!("thought {i}"))], false)
}

// Output landing moves the rows under a still pointer: the frame painted
// after it must bold the row under the pointer, not the one it left.
#[test]
fn a_row_landing_under_a_still_pointer_paints_the_hover_where_the_pointer_is() {
    let mut ui = test_ui(40, 12);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    for i in 0..12 {
        view.surface.scrollback.push(one_line_row(i));
    }
    ui.flush(&lane, &mut view);
    let (col, row) = (4, ui.regions.history.y + 3);
    ui.on_mouse_move(&mut view, col, row);
    ui.flush(&lane, &mut view);
    ui.flush(&lane, &mut view);
    assert!(bold_at(&ui, col, row), "the hover lands: {:?}", ui.drawn[3]);

    // A row lands, so every row above the live text is a line further up.
    for i in 0..5 {
        view.surface.scrollback.push(one_line_row(99 + i));
        ui.flush(&lane, &mut view);
        // A frame of the new layout shows its own hover, never a stale one.
        assert!(
            !text_at(&ui, row).contains("thought") || bold_at(&ui, col, row),
            "painted a row without its hover: {:?}",
            text_at(&ui, row)
        );
        if std::mem::take(&mut ui.redraw) {
            ui.flush(&lane, &mut view);
        }
        assert!(
            bold_at(&ui, col, row),
            "the hover settles on the pointer row"
        );
        assert!(
            text_at(&ui, row).contains("thought"),
            "the pointer row still holds text: {:?}",
            text_at(&ui, row)
        );
    }
}
