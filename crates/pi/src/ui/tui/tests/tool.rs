use crate::core::lane::Lane;
use crate::store::icons;
use crate::ui::render::Paint;
use crate::ui::tui::View;
use crate::ui::tui::mouse::Target;
use crate::ui::tui::row::Row;
use crate::ui::tui::screen::plain;
use crate::ui::tui::scrollback::{Folds, scrollback_from};

use super::harness::*;

#[test]
fn rebuilt_reasoning_blocks_get_ids_of_their_own() {
    use agent::session::Session;
    use llm::message::{AssistantContent, Reasoning, ReasoningContent};

    let mut s = Session::new();
    s.prompt("go");
    for n in 0..3 {
        s.push_assistant(vec![AssistantContent::Reasoning(Reasoning {
            id: None,
            content: vec![ReasoningContent::Text {
                text: format!("thought {n}"),
                signature: None,
            }],
            by: None,
        })]);
    }

    let mut folds = Folds::default();
    let rows = scrollback_from(&s, &Paint::new(false), &mut folds);
    let ids: Vec<u64> = rows.iter().filter_map(Row::block).collect();
    assert_eq!(ids.len(), 3, "{} rows, {ids:?}", rows.len());
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 3, "two blocks share a number: {ids:?}");
    // And the counter moved, so a block streamed after the rebuild cannot
    // land on one of these.
    assert!(!ids.contains(&folds.take_id()), "{ids:?}");
}

#[test]
fn rebuilt_empty_reasoning_blocks_are_ignored() {
    use agent::session::Session;
    use llm::message::{AssistantContent, Reasoning, ReasoningContent};

    let mut s = Session::new();
    s.prompt("go");
    s.push_assistant(vec![AssistantContent::Reasoning(Reasoning {
        id: None,
        content: vec![ReasoningContent::Text {
            text: "".into(),
            signature: None,
        }],
        by: None,
    })]);

    let mut folds = Folds::default();
    let rows = scrollback_from(&s, &Paint::new(false), &mut folds);
    let ids: Vec<u64> = rows.iter().filter_map(Row::block).collect();
    assert_eq!(ids.len(), 0);
}

// A multi-line error body reaches the pending live line and the committed
// entry alike, so adoption's equality check compares two rows born from
// one source — and a dropped preview would panic right here.
#[test]
fn an_errored_tool_adopts_its_full_body() {
    use agent::session::{Entry, EntryId};

    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    let body = "edit refused:\nline one\nline two";

    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolStart {
            id: "c1".into(),
            name: "edit".into(),
            args: serde_json::json!({}),
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolEnd {
            id: "c1".into(),
            name: "edit".into(),
            is_error: true,
            preview: body.into(),
        },
    );
    // What the loop files for the call: error content, and the preview
    // the event showed riding along (see `run_calls`'s error arm).
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::Committed {
            entries: vec![Entry::Tool {
                id: EntryId(7),
                at: 0,
                result: llm::message::ToolResult::error("c1", "edit", body),
                preview: Some(body.into()),
            }],
        },
    );

    // The pending spinner retired, the ✗ row filed once.
    assert!(view.state.tools.is_empty());
    let after = view.surface.scrollback.len();
    assert_eq!(after, 1, "one adopted row, got {after}");
}

// A wrapped pending row is tagged on every screen row it takes: the count
// is fitted rows, not lines. A long command at 40 columns folds in two.
#[test]
fn a_wrapped_pending_batch_tags_every_row_it_takes() {
    let mut ui = test_ui(40, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();

    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolStart {
            id: "c-long".into(),
            name: "bash".into(),
            args: serde_json::json!({"command":
                "cargo build --release --features wasi-x"}),
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolStart {
            id: "c-read".into(),
            name: "read".into(),
            args: serde_json::json!({}),
        },
    );
    ui.flush(&lane, &mut view);

    // Collapsed, the batch is one row; open it so every call holds a
    // row, then the long one wraps.
    ui.key(
        &lane,
        &mut view,
        mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            0,
        ),
        false,
    );
    ui.flush(&lane, &mut view);

    let tagged = live_pending_rows(&ui);
    assert_eq!(
        tagged, 3,
        "both rows of the wrapped one, and the row after it: {tagged}"
    );

    // The tail of the wrapped batch answers — the row the count used to
    // miss — and the status line after the batch does not.
    ui.key(
        &lane,
        &mut view,
        mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            2,
        ),
        false,
    );
    assert!(!ui.live_tools_shown, "the wrapped batch's tail answers");
    ui.flush(&lane, &mut view);
    ui.key(
        &lane,
        &mut view,
        mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            0,
        ),
        false,
    );
    assert!(ui.live_tools_shown);
    ui.flush(&lane, &mut view);
    let last = ui.row_targets.len() - 1;
    ui.key(
        &lane,
        &mut view,
        mouse_event(
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            last as u16,
        ),
        false,
    );
    assert!(ui.live_tools_shown, "the status line is no click target");
}

// Two calls out at once hold a line each until one lands. The row that
// opens then takes the other: the same hand-over as a call that starts
// under a row already there, and the reason the row is read per frame
// rather than settled when the call starts.
#[test]
fn a_batch_in_flight_hands_its_remainder_to_the_row_that_opens() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
    read_started(&mut ui, &mut lane, &mut view, "b", "b.rs");
    ui.flush(&lane, &mut view);

    // No row yet, so the batch is a line of its own: collapsed, the
    // newest with a count for the rest.
    assert_eq!(live_pending_rows(&ui), 1, "one line for two calls");
    assert!(drawn_rows(&view).is_empty(), "nothing has landed yet");

    // The first lands. Its row opens where the batch was drawn, and takes
    // the call still out with it — no second line, and none to jump up.
    read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
    ui.flush(&lane, &mut view);
    assert_eq!(
        live_pending_rows(&ui),
        0,
        "the row took it rather than leaving a line under it"
    );
    let rows = drawn_rows(&view);
    assert_eq!(rows.len(), 1, "one row, not two: {rows:?}");
    assert!(
        rows[0].ends_with(&format!("read b.rs {}{}", icons::ELLIPSIS, 2)),
        "got: {}",
        rows[0]
    );
}

// A call that will not fold keeps its line in the live block: an edit's
// result is a row of its own, so its line sits where that row will land
// rather than in the summary row a check is about to leave behind.
#[test]
fn a_modifying_call_keeps_its_line() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
    read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolStart {
            id: "e".into(),
            name: "edit".into(),
            args: serde_json::json!({"path": "b.rs"}),
        },
    );
    ui.flush(&lane, &mut view);

    assert_eq!(live_pending_rows(&ui), 1, "the edit holds a row of its own");
    let (live, _) = ui.live(&lane, &view, true);
    assert!(
        plain(&live[0]).contains("edit b.rs"),
        "and that row is the edit's: {:?}",
        plain(&live[0])
    );
    assert_eq!(
        drawn_rows(&view),
        vec![format!("{} read a.rs", icons::DONE_MARK)]
    );
}

// A call that lands badly is never the row's: the row would name it, count
// it and wear its ✗, then drop all three when its own row landed under it
// — the move this change exists to remove, left standing on the one path
// where a call does not fold in.
#[test]
fn a_failing_call_never_joins_the_row() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
    read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
    read_started(&mut ui, &mut lane, &mut view, "b", "b.rs");
    ui.flush(&lane, &mut view);
    // Out and held: the row draws it.
    assert_eq!(live_pending_rows(&ui), 0);

    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolEnd {
            id: "b".into(),
            name: "read".into(),
            is_error: true,
            preview: "Error: no such file".into(),
        },
    );
    ui.flush(&lane, &mut view);

    // The row is back to what it keeps, and the ✗ holds the line its own
    // row will take.
    assert_eq!(live_pending_rows(&ui), 1, "the ✗ keeps a line here");
    let (live, _) = ui.live(&lane, &view, true);
    assert_eq!(
        plain(&live[0]),
        format!("{} read b.rs", icons::FAIL_MARK),
        "its mark leads, with no frame left animating a call that is over"
    );
    assert_eq!(
        drawn_rows(&view),
        vec![format!("{} read a.rs", icons::DONE_MARK)],
        "and the row never counted it"
    );

    // Filing it leaves the row where it was and puts the ✗ where its line
    // was: nothing moved.
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::Committed {
            entries: vec![failed_entry(8, "b", "read", "Error: no such file")],
        },
    );
    ui.flush(&lane, &mut view);
    let rows = drawn_rows(&view);
    assert_eq!(rows.len(), 2, "the row and the ✗ under it: {rows:?}");
    assert_eq!(rows[0], format!("{} read a.rs", icons::DONE_MARK));
    assert!(
        rows[1].contains(&format!("{} read Error: no such file", icons::FAIL_MARK)),
        "got: {}",
        rows[1]
    );
}

#[test]
fn a_stopped_tool_call_is_silenced_in_tui() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let tui = surface(dir.path());
    let mut session = agent::session::Session::new();
    session.prompt("run check");
    session.push_assistant(vec![llm::message::AssistantContent::ToolCall(
        llm::message::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "cargo check" }),
        },
    )]);
    session.send_prompt("do something else", None::<String>);

    // The rebuild filter drops the repaired stopped tool entry.
    let stopped_entry = &session.entries()[2];
    assert!(crate::ui::tui::scrollback::f_entry(stopped_entry, &tui.ui.paint).is_none());
}

// The live region follows the lane's turn, not the clock beside it. One
// field answering both meant every ending path had to put the clock back
// or leave a spinner running over a lane that had finished.
#[test]
fn the_live_region_ends_with_the_turn_and_not_with_the_clock() {
    let ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    view.state.started = Some(std::time::Instant::now());
    assert!(
        ui.live(&lane, &view, false)
            .0
            .iter()
            .any(|r| icons::SPINNER_FRAMES.iter().any(|f| plain(r).contains(f))),
        "a running lane draws the status line"
    );

    lane.finish();
    assert!(
        !ui.live(&lane, &view, false)
            .0
            .iter()
            .any(|r| icons::SPINNER_FRAMES.iter().any(|f| plain(r).contains(f))),
        "the clock is still set; the turn is what says the run is over"
    );
}

// The live rows the frame tagged as the calls in flight: what the surface
// drew for them, as the click sees it.
fn live_pending_rows(ui: &crate::ui::tui::Ui) -> usize {
    ui.row_targets
        .iter()
        .filter(|t| matches!(t, Target::PendingTools))
        .count()
}

// A read's start, and the result the loop files under it: the two events
// that decide what the screen draws.
fn read_started(
    ui: &mut crate::ui::tui::Ui,
    lane: &mut Lane,
    view: &mut View,
    call: &str,
    path: &str,
) {
    ui.on_event(
        lane,
        view,
        agent::Event::ToolStart {
            id: call.into(),
            name: "read".into(),
            args: serde_json::json!({"path": path}),
        },
    );
}

fn read_landed(
    ui: &mut crate::ui::tui::Ui,
    lane: &mut Lane,
    view: &mut View,
    id: u64,
    call: &str,
    path: &str,
) {
    ui.on_event(
        lane,
        view,
        agent::Event::ToolEnd {
            id: call.into(),
            name: "read".into(),
            is_error: false,
            preview: path.into(),
        },
    );
    ui.on_event(
        lane,
        view,
        agent::Event::Committed {
            entries: vec![tool_entry(id, call, "read", path)],
        },
    );
}

fn tool_entry(id: u64, call: &str, name: &str, preview: &str) -> agent::session::Entry {
    use agent::session::{Entry, EntryId};
    Entry::Tool {
        id: EntryId(id),
        at: 0,
        result: llm::message::ToolResult::text(call, name, format!("the body of {call}")),
        preview: Some(preview.into()),
    }
}

fn failed_entry(id: u64, call: &str, name: &str, body: &str) -> agent::session::Entry {
    use agent::session::{Entry, EntryId};
    Entry::Tool {
        id: EntryId(id),
        at: 0,
        result: llm::message::ToolResult::error(call, name, body),
        preview: Some(body.into()),
    }
}

// A mouse event on the given screen row, at a column over any row's text.
fn mouse_event(kind: crossterm::event::MouseEventKind, row: u16) -> crossterm::event::Event {
    crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column: 5,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })
}
