use crate::render::Paint;
use crate::tui::View;
use crate::tui::mouse::Target;
use crate::tui::row::{Call, Row};
use crate::tui::screen::plain;
use crate::tui::scrollback::{Folds, scrollback_from};
use pi_core::core::lane::Lane;
use pi_store::icons;

use super::harness::*;

// Both `streaming_row` and `stream_fold` draw block ids from one counter;
// sharing a number would make the two blocks indistinguishable to lookup.
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
    let rows = scrollback_from(
        &s,
        &Paint::new(false),
        &mut folds,
        std::path::Path::new("/w/pi"),
    );
    // Nothing between them, so the three are one group.
    let groups: Vec<&Row> = rows.iter().filter(|r| r.is_steps()).collect();
    assert_eq!(groups.len(), 1, "{} rows", rows.len());
    // And the counter moved past them, so a block streamed after the rebuild
    // cannot land on one of these.
    let next = folds.take_id();
    let ids: Vec<u64> = (0..next).filter(|&id| groups[0].holds_block(id)).collect();
    assert_eq!(ids.len(), 3, "two blocks share a number: {ids:?}");
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
    let rows = scrollback_from(
        &s,
        &Paint::new(false),
        &mut folds,
        std::path::Path::new("/w/pi"),
    );
    assert!(!rows.iter().any(Row::is_steps));
}

// Esc unsends the prompt that opened the run. Once a prompt typed mid-run
// joins it, one unsend would hand back that prompt and leave the first sent
// with nothing to answer it, so the chance is spent.
#[test]
fn a_prompt_joining_the_run_spends_the_unsend() {
    use agent::session::{Entry, EntryId};

    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    let committed = |view: &View| {
        matches!(
            view.state.work.as_ref().map(|w| &w.doing),
            Some(crate::tui::view::Doing::Turn { committed: true })
        )
    };
    let commit = |entries| agent::Event::Committed { entries };

    view.state.work = a_turn(false);
    // A note filed mid-run is not the user asking again.
    ui.on_event(
        &mut lane,
        &mut view,
        commit(vec![Entry::Note {
            id: EntryId(1),
            at: 0,
            note: "a note".into(),
        }]),
    );
    assert!(!committed(&view), "a note leaves the prompt unsendable");

    ui.on_event(
        &mut lane,
        &mut view,
        commit(vec![Entry::Ask {
            id: EntryId(2),
            at: 0,
            ask: agent::session::Prompt {
                text: "and now this".into(),
                images: Vec::new(),
                shown: None,
                relayed: None,
            },
        }]),
    );
    assert!(committed(&view));
}

// The pending live line and the committed entry must carry the same body:
// adoption's equality check compares rows born from one source.
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
        agent::Event::ToolQueued {
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
            outcome: agent::Outcome::Failed,
            preview: body.into(),
            took: std::time::Duration::ZERO,
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
                warned: false,
                took: None,
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
        agent::Event::ToolQueued {
            id: "c-long".into(),
            name: "bash".into(),
            args: serde_json::json!({"command":
                "cargo build --release --features wasi-x"}),
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolQueued {
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

    // The tail of the wrapped batch answers; the rows after it do not.
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
}

// The same hand-over as a call starting under a row already there — why
// the row is read per frame rather than settled when the call starts.
#[test]
fn a_batch_in_flight_hands_its_remainder_to_the_row_that_opens() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    read_queued(&mut ui, &mut lane, &mut view, "a", "a.rs");
    read_queued(&mut ui, &mut lane, &mut view, "b", "b.rs");
    ui.flush(&lane, &mut view);

    // No row yet, so the batch is a line of its own: collapsed, the
    // newest with a count for the rest.
    assert_eq!(live_pending_rows(&ui), 1, "one line for two calls");
    assert!(drawn_rows(&view).is_empty(), "nothing has landed yet");

    // The first lands. Its row opens where the batch was drawn, and takes
    // the call still out with it. Each call keeps its own line.
    read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
    ui.flush(&lane, &mut view);
    assert_eq!(
        live_pending_rows(&ui),
        0,
        "the row took it rather than leaving a line under it"
    );
    let rows = drawn_rows(&view);
    assert_eq!(rows.len(), 2, "each call has its own line: {rows:?}");
    assert!(rows[0].ends_with("read a.rs"), "{rows:?}");
    assert!(rows[1].ends_with("read b.rs"), "{rows:?}");
}

// An edit's result is a row of its own, so its line sits where that row
// will land, not in the summary row a plain call leaves behind.
#[test]
fn a_modifying_call_keeps_its_line() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    read_queued(&mut ui, &mut lane, &mut view, "a", "a.rs");
    read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolQueued {
            id: "e".into(),
            name: "edit".into(),
            args: serde_json::json!({"path": "b.rs"}),
        },
    );
    ui.flush(&lane, &mut view);

    assert_eq!(live_pending_rows(&ui), 1, "the edit holds a row of its own");
    let (live, _, _) = ui.live(&view, true, std::time::Instant::now());
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

// A click on a running call's line opens what it has printed: the last
// lines, with a count for the rest. Its end closes the view.
#[test]
fn a_click_opens_what_a_running_call_printed() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolQueued {
            id: "c".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "seq 1 40"}),
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolStart {
            id: "c".into(),
            at: std::time::Instant::now(),
        },
    );
    let printed: String = (1..=40).map(|n| format!("{n}\n")).collect();
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolPrinted {
            id: "c".into(),
            text: printed,
        },
    );
    ui.flush(&lane, &mut view);
    let (closed, _, _) = ui.live(&view, false, std::time::Instant::now());
    assert_eq!(closed.len(), 1, "one line while closed");

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
    let (open, _, _) = ui.live(&view, false, std::time::Instant::now());
    assert!(open.len() > 1, "the printed lines open under it");
    assert!(
        open.iter().any(|l| plain(l).contains("more")),
        "a count for what is left out: {open:?}"
    );
    assert!(
        open.iter().any(|l| plain(l).contains("40")),
        "its newest line: {open:?}"
    );

    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolEnd {
            id: "c".into(),
            name: "bash".into(),
            outcome: agent::Outcome::Done,
            preview: String::new(),
            took: std::time::Duration::ZERO,
        },
    );
    ui.flush(&lane, &mut view);
    assert!(
        !view.state.tools.iter().any(|t| t.open),
        "its end closes the view"
    );
}

// A prompt above the live block leaves a gap row; the first clickable row
// must still name the first call, and a second click closes what the first
// opened.
#[test]
fn a_click_after_a_prompt_opens_and_closes_its_own_call() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    for row in Row::prompt("go", 1, &Paint::new(false)) {
        view.surface.scrollback.push(row);
    }
    for (id, command) in [("c1", "seq 1 3"), ("c2", "seq 4 6")] {
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolQueued {
                id: id.into(),
                name: "bash".into(),
                args: serde_json::json!({"command": command}),
            },
        );
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolStart {
                id: id.into(),
                at: std::time::Instant::now(),
            },
        );
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolPrinted {
                id: id.into(),
                text: command.into(),
            },
        );
    }
    ui.flush(&lane, &mut view);
    let at = ui
        .row_targets
        .iter()
        .position(|t| matches!(t, Target::PendingTool(_)))
        .expect("a call line");
    let row = ui.regions.history.y + at as u16;
    ui.on_mouse_click(&mut view, 0, row);
    assert!(view.state.tools.iter().find(|t| t.id == "c2").unwrap().open);
    ui.on_mouse_click(&mut view, 0, row);
    assert!(!view.state.tools.iter().find(|t| t.id == "c2").unwrap().open);
}

// A failing call is never the row's: the row would name it, count it and
// wear its ✗, then drop all three once its own row landed under it.
#[test]
fn a_failing_call_never_joins_the_row() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    read_queued(&mut ui, &mut lane, &mut view, "a", "a.rs");
    read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
    read_queued(&mut ui, &mut lane, &mut view, "b", "b.rs");
    ui.flush(&lane, &mut view);
    // Out and held: the row draws it.
    assert_eq!(live_pending_rows(&ui), 0);

    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolEnd {
            id: "b".into(),
            name: "read".into(),
            outcome: agent::Outcome::Failed,
            preview: "Error: no such file".into(),
            took: std::time::Duration::ZERO,
        },
    );
    ui.flush(&lane, &mut view);

    // The row is back to what it keeps, and the ✗ holds the line its own
    // row will take.
    assert_eq!(live_pending_rows(&ui), 1, "the ✗ keeps a line here");
    let (live, _, _) = ui.live(&view, true, std::time::Instant::now());
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

// Where a call ran is the call's, not its result's: a failure filed live
// keeps it, as the rebuild does.
#[test]
fn a_failing_call_keeps_where_it_ran() {
    // The rows are read at the screen's width, so the drift check paints the
    // row at the width it is read back at.
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolQueued {
            id: "b".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "ls", "cwd": "sub"}),
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolEnd {
            id: "b".into(),
            name: "bash".into(),
            outcome: agent::Outcome::Failed,
            preview: "no such dir".into(),
            took: std::time::Duration::ZERO,
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::Committed {
            entries: vec![failed_entry(8, "b", "bash", "no such dir")],
        },
    );
    ui.flush(&lane, &mut view);
    let rows = drawn_rows(&view);
    assert!(
        rows.iter()
            .any(|r| r.ends_with(&format!("no such dir{}in sub", icons::PART_SEP))),
        "got: {rows:?}"
    );
}

// The time the live line says must be the one the stored entry gives back,
// or reopening the session redraws the call differently.
#[test]
fn a_call_keeps_how_long_it_ran() {
    let mut ui = test_ui(80, 24);
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolQueued {
            id: "b".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "ls", "cwd": "sub"}),
        },
    );
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::ToolEnd {
            id: "b".into(),
            name: "bash".into(),
            outcome: agent::Outcome::Failed,
            preview: "no such dir".into(),
            took: std::time::Duration::from_millis(5_400),
        },
    );
    let mut entry = failed_entry(8, "b", "bash", "no such dir");
    if let agent::session::Entry::Tool { took, .. } = &mut entry {
        *took = Some(std::time::Duration::from_millis(5_400));
    }
    ui.on_event(
        &mut lane,
        &mut view,
        agent::Event::Committed {
            entries: vec![entry],
        },
    );
    ui.flush(&lane, &mut view);
    let rows = drawn_rows(&view);
    let sep = icons::PART_SEP;
    assert!(
        rows.iter()
            .any(|r| r.ends_with(&format!("no such dir{sep}in sub{sep}5s"))),
        "got: {rows:?}"
    );
}

#[test]
fn a_stopped_tool_call_is_silenced_in_tui() {
    let mut session = agent::session::Session::new();
    session.prompt("run check");
    session.push_assistant(vec![llm::message::AssistantContent::ToolCall(
        llm::message::ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({ "command": "cargo check" }),
        },
    )]);
    session.close_open(agent::session::STOPPED_CALL);
    session.send_prompt("do something else", None::<String>);

    // The rebuild draws nothing of it: the call it would leave is none.
    let stopped_entry = &session.entries()[2];
    let agent::session::Entry::Tool { result, .. } = stopped_entry else {
        panic!("a tool entry, {:?}", stopped_entry)
    };
    assert!(
        crate::tui::scrollback::call_of(&mut Folds::default(), result, None, false, None).is_none(),
        "an interrupted call leaves no row"
    );
    // One that landed leaves one, on the same path.
    let landed = llm::message::ToolResult::text("c2", "bash", "done");
    assert!(
        crate::tui::scrollback::call_of(&mut Folds::default(), &landed, None, false, None)
            .is_some()
    );
}

// The live rows the frame tagged as the calls in flight: what the surface
// drew for them, as the click sees it.
fn live_pending_rows(ui: &crate::tui::Ui) -> usize {
    ui.row_targets
        .iter()
        .filter(|t| matches!(t, Target::PendingTool(_)))
        .count()
}

// A read taken into its batch; `read_landed` ends it. Between them they
// decide what the screen draws.
fn read_queued(ui: &mut crate::tui::Ui, lane: &mut Lane, view: &mut View, call: &str, path: &str) {
    ui.on_event(
        lane,
        view,
        agent::Event::ToolQueued {
            id: call.into(),
            name: "read".into(),
            args: serde_json::json!({"path": path}),
        },
    );
}

fn read_landed(
    ui: &mut crate::tui::Ui,
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
            outcome: agent::Outcome::Done,
            preview: path.into(),
            took: std::time::Duration::ZERO,
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
        warned: false,
        took: None,
    }
}

fn failed_entry(id: u64, call: &str, name: &str, body: &str) -> agent::session::Entry {
    use agent::session::{Entry, EntryId};
    Entry::Tool {
        id: EntryId(id),
        at: 0,
        result: llm::message::ToolResult::error(call, name, body),
        preview: Some(body.into()),
        warned: false,
        took: None,
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

// An open execution's tail stays within its rows, and every line not shown whole
// is counted for the page.
#[test]
fn an_open_commands_tail_counts_what_it_leaves_out() {
    use crate::tui::call::tail_rows;
    let many: Vec<String> = (0..10).map(|_| "word ".repeat(30)).collect();
    let per = tail_rows(&many[..1], 40).1.len();
    let (from, rows) = tail_rows(&many, 40);
    assert!(rows.len() <= 8);
    assert_eq!(rows.len() % per, 0);
    assert_eq!(from + rows.len() / per, many.len());

    // A last line taller than the rows keeps its own last ones.
    let huge = vec!["word ".repeat(200)];
    let (from, rows) = tail_rows(&huge, 40);
    assert_eq!((from, rows.len()), (1, 8));
}

// A change to files keeps a row of its own — an edit's and a command's
// alike: the fold's budget cuts it to a window, and the count under it opens
// the rest on a page.
#[test]
fn a_change_report_keeps_its_own_row() {
    use crate::render::Outcome;
    use crate::tui::call::push_tool_row;
    use crate::tui::scrollback::{Folds, Scrollback};

    let command = Call::new(
        Outcome::Done,
        "bash",
        "README.md +1 -1\n-1 POST /a\n+1 post /a",
        "sed -i 's/POST/post/g' README.md",
        Some("ok"),
    );
    let mut scrollback = Scrollback::default();
    push_tool_row(&mut scrollback, &mut Folds::default(), command);
    assert_eq!(scrollback.len(), 1);
    assert!(
        !scrollback.iter().any(Row::is_steps),
        "a change keeps its rows"
    );

    let bare = Call::new(
        Outcome::Done,
        "bash",
        "sed -i 's/POST/post/g' README.md",
        "sed -i 's/POST/post/g' README.md",
        Some("ok"),
    );
    let mut scrollback = Scrollback::default();
    push_tool_row(&mut scrollback, &mut Folds::default(), bare);
    assert!(
        scrollback.iter().any(Row::is_steps),
        "a bare command is a step"
    );

    let edit = Call::new(Outcome::Done, "edit", "a.rs +1 -1\n-1 x\n+1 y", "", None);
    let mut scrollback = Scrollback::default();
    push_tool_row(&mut scrollback, &mut Folds::default(), edit);
    assert!(
        !scrollback.iter().any(Row::is_steps),
        "and so does an edit's"
    );
}
