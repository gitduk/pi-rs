use crate::core::lane::Lane;
use crate::input::commands::Choice;
use crate::input::{Builtin, Drive};
use crate::store::icons;
use crate::store::keys::Mode;
use crate::store::listing::Listing;
use crate::store::status::Segment;
use crate::ui::tui::screen::plain;
use crate::ui::tui::{Asked, Deed, Intent, Origin, Row, View, view_at};
use agent::Event;

use super::harness::*;

// Recall belongs to the checkout, like the transcripts and the completion
// lists. One file for the whole machine put the lines typed in one project
// under `k` in another — a leak as much as a nuisance.
#[tokio::test]
async fn recall_follows_the_checkout() {
    let first = tempfile::tempdir().expect("a checkout");
    let second = tempfile::tempdir().expect("another checkout");
    let mut tui = surface(first.path());

    let store = tui.core.store.clone();
    let file = |ws: &std::path::Path, line: &str| {
        let path = store.history_path(ws);
        std::fs::create_dir_all(path.parent().expect("a bucket")).expect("the bucket");
        std::fs::write(path, crate::ui::tui::editor::encode(&[line.to_string()])).expect("written");
    };
    file(first.path(), "what the first was asked");
    file(second.path(), "what the second was asked");

    // `on_test_screen` skips the startup seed `Tui::new` does, so stand in
    // for it. What is under test is that a switch replaces this, and that
    // it does not reach for the bucket of the checkout being left.
    tui.ui
        .editor
        .seed_history(vec!["what the first was asked".to_string()]);
    switch_to(&mut tui, running_lane(second.path()));

    let landed = tui.ui.editor.history();
    assert_eq!(
        landed.first().map(String::as_str),
        Some("what the second was asked"),
        "the lane in front is what k recalls"
    );
    assert_eq!(landed.len(), 1, "replaced, not appended: {landed:?}");
}

// A flash belongs to the lane it answered. Carried across a switch it
// names the wrong checkout, and it does it on the row the bar uses to say
// which checkout is in front.
#[tokio::test]
async fn a_flash_does_not_follow_the_surface_to_another_lane() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());

    tui.ui.flash("nothing running to stop");
    switch_to(&mut tui, running_lane(dir.path()));
    assert!(tui.ui.flash.is_none(), "the flash was left behind");
}

// A rebuilt lane has already been drawn, whatever its row counts say.
// `rebuild` clears the banner along with the rest — `/resume` and a
// rewind both do it — so a switch back must not read that as a lane
// never drawn and lay a fresh opening block over the transcript.
#[tokio::test]
async fn switching_back_to_a_rebuilt_lane_keeps_its_transcript() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    let lane = running_lane(dir.path());
    let mut view = crate::ui::tui::View::opening(&[], &tui.ui.paint);
    // As `rebuild` leaves it: the conversation, and no banner.
    view.surface.scrollback = vec![Row::notice("what was said before")];
    view.surface.opened = 0;
    // The screen the lane comes back with, keyed by the lane it was built
    // for: a lane no longer carries its own.
    tui.views.insert(lane.token(), view);
    switch_to(&mut tui, lane);

    // By content, not by count: the banner this would lay over it is one
    // row too, so a length check cannot tell them apart.
    let token = tui.core.lanes[1].token();
    let rows = lane_rows(&mut tui, token);
    assert!(
        rows.iter().any(|r| r.contains("what was said before")),
        "the transcript survives the switch: {rows:?}"
    );
}

// A lane out of front keeps what its run posts in its own view as it comes,
// so the screen that moves to it finds it there, not a backlog to replay.
#[tokio::test]
async fn what_a_lane_out_of_front_posted_is_in_its_view_already() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    let lane = running_lane(dir.path());
    let token = lane.token();
    let sent = lane.sender().clone();
    tui.core.lanes.push(lane);

    sent.send(Event::Warning("said behind you".into()))
        .expect("the lane is listening");
    tui.serve_lanes().await;
    assert!(
        lane_rows(&mut tui, token)
            .iter()
            .any(|r| r.contains("said behind you")),
        "a lane out of front did not take what its run posted"
    );

    // The screen moves to it, the way a checkout switch does: still there once.
    tui.core.current = 1;
    tui.reconcile(0);
    let rows = lane_rows(&mut tui, token);
    assert_eq!(
        rows.iter()
            .filter(|r| r.contains("said behind you"))
            .count(),
        1,
        "what the lane posted is on its screen once: {rows:?}"
    );
}

// The bug this guards: `/compact` used to settle without ever putting the
// lane back to `Idle`, and a lane left `Running` queues every later
// prompt into a queue that only drains once it is not running — so the
// checkout was wedged for good. Every kind has to come back idle.
#[tokio::test]
async fn every_kind_of_job_leaves_its_lane_idle() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let kinds = || {
        vec![
            ("turn", crate::ui::tui::job::Kind::Turn),
            (
                "bash",
                crate::ui::tui::job::Kind::Bash {
                    lines: vec!["out".into()],
                },
            ),
            ("compact", crate::ui::tui::job::Kind::Compact(None)),
            (
                "compact with a report",
                crate::ui::tui::job::Kind::Compact(Some((
                    agent::Report::default(),
                    llm::stream::Usage::default(),
                ))),
            ),
        ]
    };
    for (what, kind) in kinds() {
        let mut tui = surface(dir.path());
        tui.settle(crate::ui::tui::job::Done {
            token: tui.core.lanes[0].token(),
            kind,
            ran: Some((
                agent::session::Session::default(),
                Ok(llm::stream::Usage::default()),
            )),
        })
        .await;
        assert!(
            !tui.core.lanes[0].is_running(),
            "a {what} left its lane running"
        );
    }
    // And the same when the job panicked and brought no transcript home.
    for (what, kind) in kinds() {
        let mut tui = surface(dir.path());
        tui.settle(crate::ui::tui::job::Done {
            token: tui.core.lanes[0].token(),
            kind,
            ran: None,
        })
        .await;
        assert!(
            !tui.core.lanes[0].is_running(),
            "a panicked {what} left its lane running"
        );
    }
}

// A `!` that panicked brings no cursor home. Reading that as "nothing is
// drawn yet" laid the recovered transcript over the screen a second time.
#[tokio::test]
async fn a_panicked_bang_does_not_lay_its_transcript_down_again() {
    use agent::session::{Prompt, Session};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let mut s = Session::new();
    s.push_bash(Prompt {
        text: "Ran `ls`\nfile".into(),
        image: None,
        shown: Some("!ls".into()),
    });
    tui.core.lane_mut().return_session(s);
    let token = tui.core.lane().token();
    // The rows as the screen has them, and the archive as it was saved.
    {
        let session = tui.core.lane().session().expect("the transcript");
        tui.ui.rebuild(view_at(&mut tui.views, token), session);
    }
    tui.core.save_lane(0).expect("saved");

    // The job took the transcript and panicked with it, the way `start_bash`
    // lends one out. Nothing comes back but the archive on disk.
    let _ = tui.core.lane_mut().take_session();

    // The job panicked: no lines, and no transcript came home.
    tui.settle(crate::ui::tui::job::Done {
        token,
        kind: crate::ui::tui::job::Kind::Bash { lines: Vec::new() },
        ran: None,
    })
    .await;
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    tui.start_turn("the next question".into(), None, &tx);

    let rows = lane_rows(&mut tui, token);
    assert_eq!(
        rows.iter().filter(|r| *r == "! ls").count(),
        1,
        "the recovered transcript is not laid down twice: {rows:?}"
    );
}

// A stopped command or turn does not inject a cancelled note to the model.
#[tokio::test]
async fn a_stopped_run_does_not_tell_the_model_a_request_was_cancelled() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let stopped = |kind: crate::ui::tui::job::Kind| {
        let dir = dir.path().to_path_buf();
        async move {
            let mut tui = surface(&dir);
            let mut session = agent::session::Session::new();
            session.prompt("the task the user actually asked for");
            tui.settle(crate::ui::tui::job::Done {
                token: tui.core.lanes[0].token(),
                kind,
                ran: Some((session, Err(agent::AgentError::Cancelled))),
            })
            .await;
            let mut back = tui.core.lanes[0]
                .take_session()
                .expect("the transcript back");
            back.send_prompt(String::from("now something else"), None::<String>);
            format!("{:?}", back.entries())
        }
    };

    let after_bash = stopped(crate::ui::tui::job::Kind::Bash {
        lines: vec!["some output".into()],
    })
    .await;
    assert!(
        !after_bash.contains("stopped the previous run"),
        "a stopped `!` adds no note: {after_bash}"
    );

    let after_turn = stopped(crate::ui::tui::job::Kind::Turn).await;
    assert!(
        !after_turn.contains("stopped the previous run"),
        "a stopped turn adds no note: {after_turn}"
    );
}

// An interrupted turn never states its own word, so the spend the view
// was already showing is what lands in the totals — the next run's base
// carries it rather than stepping back to what the session had before.
#[tokio::test]
async fn an_interrupted_turn_keeps_its_spend_in_the_session_totals() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    let mut session = agent::session::Session::new();
    session.prompt("the task the user actually asked for");

    tui.core.lanes[0].note(&agent::Event::TurnStart { turn: 1 });
    tui.core.lanes[0].note(&agent::Event::Usage(llm::stream::Usage {
        input: 100,
        output: 20,
        ..Default::default()
    }));

    tui.settle(crate::ui::tui::job::Done {
        token: tui.core.lanes[0].token(),
        kind: crate::ui::tui::job::Kind::Turn,
        ran: Some((session, Err(agent::AgentError::Cancelled))),
    })
    .await;

    assert_eq!(tui.core.lanes[0].totals().usage.input, 100);
    assert_eq!(tui.core.lanes[0].totals().usage.output, 20);
}

// The same for a lane out of sight: its meter hears the run as it goes, so a
// stop that lands before anyone looks still charges what the run spent.
#[tokio::test]
async fn an_interrupted_turn_out_of_sight_keeps_its_spend_too() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    let lane = running_lane(dir.path());
    let token = lane.token();
    let sent = lane.sender().clone();
    tui.core.lanes.push(lane);
    let mut session = agent::session::Session::new();
    session.prompt("the task behind you");

    for event in [
        agent::Event::TurnStart { turn: 1 },
        agent::Event::Usage(llm::stream::Usage {
            input: 100,
            output: 20,
            ..Default::default()
        }),
    ] {
        sent.send(event).expect("the lane is listening");
    }
    tui.settle(crate::ui::tui::job::Done {
        token,
        kind: crate::ui::tui::job::Kind::Turn,
        ran: Some((session, Err(agent::AgentError::Cancelled))),
    })
    .await;

    assert_eq!(tui.core.lanes[1].totals().usage.input, 100);
    assert_eq!(tui.core.lanes[1].totals().usage.output, 20);
}

// A flash is transient: it is not part of the transcript, and it is
// dropped by the clock, not by whoever set it — who is long gone by then.
#[test]
fn a_flash_is_transient_and_stays_out_of_the_transcript() {
    let mut ui = test_ui(40, 8);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    let before = view.surface.scrollback.len();

    ui.flash("the only checkout there is");
    ui.flush(&lane, &mut view);
    assert!(ui.flash.is_some());
    assert_eq!(
        view.surface.scrollback.len(),
        before,
        "a flash is not part of the transcript"
    );

    // Backdated past the window: the next frame is the one that drops it,
    // which is what an idle screen relies on.
    let (text, _) = ui.flash.take().expect("a flash is up");
    ui.flash = Some((
        text,
        crate::ui::tui::Instant::now()
            .checked_sub(crate::ui::tui::FLASH)
            .expect("a clock"),
    ));
    ui.flush(&lane, &mut view);
    assert!(ui.flash.is_none(), "the expired flash was dropped");
}

// The bar's row is the bar's whether or not it is saying anything: a row
// that came and went would take the transcript above it along, and the
// newest line would sit a row lower for the second a flash is up.
#[test]
fn the_bar_keeps_its_row_when_a_flash_comes_and_goes() {
    let mut ui = test_ui(40, 8);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.tabs = vec![tab(crate::ui::tui::Mark::Front, "main")];

    ui.flush(&lane, &mut view);
    let quiet = ui.regions.history.height;
    assert_eq!(ui.regions.bar.height, 1, "the bar is a row of its own");

    ui.flash("the only checkout there is");
    ui.flush(&lane, &mut view);
    assert_eq!(
        ui.regions.history.height, quiet,
        "a flash moves nothing above it"
    );
}

// The bar carries the whole ring, the lanes' own and the disk's alike: a
// checkout no lane has open is on it as soon as the list has been read, so
// what `H`/`L` would reach is read off the bar before either is pressed.
#[test]
fn the_bar_lists_the_checkouts_no_lane_has_open() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    tui.core.lanes[0].set_worktree(Some("pi-rs".into()));
    tui.ui
        .lists
        .worktrees
        .set(vec![
            Choice {
                name: "pi-rs".into(),
                note: String::new(),
            },
            Choice {
                name: "fix-mem".into(),
                note: String::new(),
            },
            Choice {
                name: "fw-rm".into(),
                note: String::new(),
            },
        ])
        .ok();
    tui.refresh_tabs();

    assert_eq!(bar(&tui.ui, 39), "pi-rs · ○ fix-mem · ○ fw-rm");
    // And the order it lists them in is the order a step walks.
    assert_eq!(
        tui.ui.step_checkout(&tui.core.lanes[0], true).as_deref(),
        Some("fix-mem")
    );
}

// The Normal `L` walks the checkouts in a ring forward; `H` walks it
// back. Every checkout on disk is in it, not only the open ones
// — the main one first, because that is the order `core::worktree::list`
// reports and a lane in it carries no name.
#[test]
fn stepping_the_checkouts_walks_the_ring_and_wraps_both_ways() {
    let ring = |at: Option<&str>, forward: bool| {
        let ui = test_ui(80, 24);
        let trees = ["pi-rs", "f1", "f2"]
            .iter()
            .map(|n| Choice {
                name: n.to_string(),
                note: String::new(),
            })
            .collect();
        ui.lists.worktrees.set(trees).ok();
        let (_dir, mut lane) = a_running_lane();
        lane.set_worktree(at.map(str::to_string));
        ui.step_checkout(&lane, forward)
    };
    // The main checkout is the one a lane names as None.
    assert_eq!(ring(None, true).as_deref(), Some("f1"));
    assert_eq!(ring(Some("f1"), true).as_deref(), Some("f2"));
    // And round the end, back to the main one.
    assert_eq!(ring(Some("f2"), true).as_deref(), Some("pi-rs"));
    // The other way round, from the main one.
    assert_eq!(ring(None, false).as_deref(), Some("f2"));
    assert_eq!(ring(Some("f1"), false).as_deref(), Some("pi-rs"));
    assert_eq!(ring(Some("f2"), false).as_deref(), Some("f1"));
}

// The ring agrees with the tabs the bar shows: a step lands on the next
// checkout in the order they were opened, not the order git reports
// them, with the ones not open yet after the open ones.
#[test]
fn the_ring_walks_the_tabs_order_not_gits() {
    let mut ui = test_ui(80, 24);
    let trees = ["pi-rs", "fw-rm", "fix-input", "fix-mem"]
        .iter()
        .map(|n| Choice {
            name: n.to_string(),
            note: String::new(),
        })
        .collect();
    ui.lists.worktrees.set(trees).ok();
    // fix-input was created before fix-mem, but the user opened fix-mem
    // first — the bar's order, which a step from fw-rm must follow.
    ui.tabs = vec![
        crate::ui::tui::Tab {
            mark: crate::ui::tui::Mark::Plain,
            name: "pi-rs".into(),
        },
        crate::ui::tui::Tab {
            mark: crate::ui::tui::Mark::Plain,
            name: "fw-rm".into(),
        },
        crate::ui::tui::Tab {
            mark: crate::ui::tui::Mark::Front,
            name: "fix-mem".into(),
        },
        crate::ui::tui::Tab {
            mark: crate::ui::tui::Mark::Plain,
            name: "fix-input".into(),
        },
    ];
    let (_dir, mut lane) = a_running_lane();
    lane.set_worktree(Some("fw-rm".into()));

    assert_eq!(ui.step_checkout(&lane, true).as_deref(), Some("fix-mem"));
    lane.set_worktree(Some("fix-mem".into()));
    assert_eq!(ui.step_checkout(&lane, true).as_deref(), Some("fix-input"));
    lane.set_worktree(Some("fix-input".into()));
    assert_eq!(ui.step_checkout(&lane, true).as_deref(), Some("pi-rs"));
    // And the other way, still on the bar's order.
    lane.set_worktree(Some("fix-mem".into()));
    assert_eq!(ui.step_checkout(&lane, false).as_deref(), Some("fw-rm"));
}

// A checkout deleted from the shell leaves its lane a dead end; the loop
// drops idle ones so the bar's tab and the step ring stop pretending it
// is there, and the lane in front keeps its place.
#[test]
fn a_lane_whose_checkout_vanished_is_dropped_and_current_follows() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    // A stale ring entry would let a later step offer — and re-create —
    // the checkout that just went; dropping the lane drops the cache.
    tui.ui
        .lists
        .worktrees
        .set(vec![Choice {
            name: "fix-mem".into(),
            note: String::new(),
        }])
        .ok();
    let gone = vanished_lane("fix-mem");
    tui.core.lanes.push(gone);
    assert_eq!(tui.core.lanes.len(), 2);

    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 1);
    assert_eq!(tui.core.current, 0);
    assert!(
        tui.ui.flash.is_none(),
        "the lane leaving the bar is the whole notice"
    );
    assert!(
        tui.ui.lists.worktrees().is_empty(),
        "the stale ring entry went too"
    );

    // A vanished lane before the one in front shifts its index down.
    let mut tui = surface(dir.path());
    let earlier = vanished_lane("fix-old");
    tui.core.lanes.insert(0, earlier);
    tui.core.current = 1;
    tui.core.lanes[1].finish();
    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 1);
    assert_eq!(tui.core.current, 0, "the front lane follows its index");
}

// A vanished lane that sits before a running one waits: removing it
// would shift the index the running lane's end reports back by.
#[test]
fn a_vanished_lane_before_a_running_one_waits_for_it() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let gone = vanished_lane("fix-mem");
    tui.core.lanes.push(gone);
    let (_run_dir, running) = a_running_lane();
    tui.core.lanes.push(running);
    assert_eq!(tui.core.lanes.len(), 3);

    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 3, "the run's lane must not move");

    // The run over, the same pass now reaches the vanished lane.
    tui.core.lanes[2].finish();
    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 2);
}

// The bar answers what a lane has finished, not what it is doing: a lane
// working out of sight wears its plain name and no frame, and only a run
// that ended wears a mark.
#[test]
fn only_a_finished_lane_wears_a_mark_in_the_bar() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let (_run_dir, mut behind) = a_running_lane();
    behind.set_worktree(Some("fix-mem".into()));
    tui.core.lanes.push(behind);
    tui.refresh_tabs();

    let marks = |tui: &crate::ui::tui::Tui| tui.ui.tabs.iter().map(|t| t.mark).collect::<Vec<_>>();
    assert_eq!(
        marks(&tui),
        vec![crate::ui::tui::Mark::Front, crate::ui::tui::Mark::Plain],
        "a lane working out of sight is just a lane"
    );
    let bar = plain(&tui.ui.lane_bar("", 80).expect("two lanes keep a bar"));
    assert!(
        !icons::SPINNER_FRAMES.iter().any(|f| bar.contains(f)),
        "and the bar does not animate it: {bar}"
    );

    tui.core.lanes[1].end(true, false);
    tui.refresh_tabs();
    assert_eq!(
        marks(&tui),
        vec![crate::ui::tui::Mark::Front, crate::ui::tui::Mark::Done]
    );

    tui.core.lanes[1].end(false, false);
    tui.refresh_tabs();
    assert_eq!(
        marks(&tui),
        vec![crate::ui::tui::Mark::Front, crate::ui::tui::Mark::Failed]
    );
}

// A loop between rounds is not the lane finishing: its next round is work
// this lane still has, so `Done` is not its mark.
#[test]
fn a_lane_under_a_loop_has_not_finished() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let (_run_dir, mut behind) = a_running_lane();
    behind.set_worktree(Some("fix-mem".into()));
    behind.end(true, false);
    let token = behind.token();
    let started = tui
        .drivers
        .command(Drive::Loop(Some("go".into())), token, behind.ctx());
    assert!(matches!(started, crate::driver::Said::Nothing));
    tui.core.lanes.push(behind);
    tui.refresh_tabs();
    assert_eq!(
        tui.ui.tabs[1].mark,
        crate::ui::tui::Mark::Plain,
        "the loop is going on"
    );

    // The loop is over, and that is the lane the colour is for.
    tui.drivers
        .command(Drive::Loop(None), token, tui.core.lanes[1].ctx());
    tui.refresh_tabs();
    assert_eq!(tui.ui.tabs[1].mark, crate::ui::tui::Mark::Done);
}

// Nowhere to go is said, not walked to: one checkout has no next.
#[test]
fn a_lone_checkout_has_no_next() {
    let ui = test_ui(80, 24);
    ui.lists
        .worktrees
        .set(vec![Choice {
            name: "pi-rs".into(),
            note: String::new(),
        }])
        .ok();
    let (_dir, lane) = a_running_lane();
    assert_eq!(ui.step_checkout(&lane, true), None);
    assert_eq!(ui.step_checkout(&lane, false), None);
}

// The half-typed line belongs to the lane it was typed at. A switch
// parks it on that lane — the editor is the surface's, and Enter on the
// checkout just landed on must not file another lane's draft into its
// session — and it comes back to the editor when the lane does.
#[tokio::test]
async fn a_draft_is_parked_on_the_lane_it_was_typed_at_and_comes_back() {
    let first = tempfile::tempdir().expect("a checkout");
    let second = tempfile::tempdir().expect("another checkout");
    let mut tui = surface(first.path());
    tui.ui.editor.set_line("half a thought meant for this lane");

    let was = tui.core.current;
    switch_to(&mut tui, running_lane(second.path()));

    assert_eq!(
        view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
        "half a thought meant for this lane",
        "the draft is parked on the lane that was left"
    );
    assert_eq!(
        tui.ui.editor.text(),
        "",
        "the checkout in front starts its own clean line"
    );

    tui.core.current = was;
    tui.reconcile(was + 1);
    assert_eq!(
        tui.ui.editor.text(),
        "half a thought meant for this lane",
        "the draft is back in the editor with its lane"
    );
    assert_eq!(
        view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
        "",
        "the parked draft was taken up, not left behind"
    );
}

// Up browsing recall puts the composed line aside and shows a recalled
// one; switching then must park the composed line, not the recall.
#[tokio::test]
async fn a_switch_mid_recall_keeps_the_line_being_typed() {
    let first = tempfile::tempdir().expect("a checkout");
    let second = tempfile::tempdir().expect("another checkout");
    let mut tui = surface(first.path());
    tui.ui
        .editor
        .seed_history(vec!["an older prompt".to_string()]);
    tui.ui.editor.set_line("half a thought meant for this lane");
    tui.ui.editor.up();
    assert_eq!(tui.ui.editor.text(), "an older prompt");

    let was = tui.core.current;
    switch_to(&mut tui, running_lane(second.path()));

    assert_eq!(
        view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
        "half a thought meant for this lane",
        "the composing line is parked, not the recalled one"
    );
}

// Normal mode: `L` is the next checkout and `H` the previous one, and the
// lowercase pair is left to the caret. `J`/`K` take the window in the
// same hand, half a screen at a time.
#[test]
fn normal_capitals_step_the_checkouts_and_the_window() {
    let mut ui = vim_ui();
    let trees = ["pi-rs", "f1", "f2"]
        .iter()
        .map(|n| Choice {
            name: n.to_string(),
            note: String::new(),
        })
        .collect();
    ui.lists.worktrees.set(trees).ok();
    ui.vim.as_mut().unwrap().mode = Mode::Normal;
    let (_dir, mut lane) = a_running_lane();
    let mut view = View::default();

    let next = ui.key(&lane, &mut view, typed('L'), false);
    assert!(
        matches!(&next, Asked::Core(Intent::Builtin(Builtin::Worktree(name))) if name == "f1"),
        "{next:?}"
    );
    lane.set_worktree(Some("f1".into()));
    let prev = ui.key(&lane, &mut view, typed('H'), false);
    assert!(
        matches!(&prev, Asked::Core(Intent::Builtin(Builtin::Worktree(name))) if name == "pi-rs"),
        "{prev:?}"
    );

    // The lowercase pair no longer leaves the lane it is typed in.
    for lower in ['h', 'l'] {
        let intent = ui.key(&lane, &mut view, typed(lower), false);
        assert!(
            matches!(intent, Asked::Own(Deed::Nothing)),
            "`{lower}`: {intent:?}"
        );
    }

    // And the window moves without the caret: scrolled up by J, back by K.
    view.surface.scroll = 0;
    ui.key(&lane, &mut view, typed('K'), false);
    let up = view.surface.scroll;
    assert!(up > 0, "K went back through the window: {up}");
    ui.key(&lane, &mut view, typed('J'), false);
    assert!(
        view.surface.scroll < up,
        "J came forward again: {}",
        view.surface.scroll
    );
}

// Whichever door the config comes in by — an edit claimed for the
// session, or the session value written to the file — it has to land, or
// the line never moves.
#[tokio::test]
async fn a_settings_change_to_the_status_segments_reaches_the_surface() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    assert!(!tui.ui.live.contains(&Segment::Model));

    // The session door: an edit claims the value for this run.
    let said = tui
        .core
        .edit("status.live", r#"["model"]"#)
        .expect("the edit lands");
    tui.land_lines(Listing::say(said));
    assert_eq!(tui.ui.live, vec![Segment::Model]);

    // The file door: the session value goes to the file the config is
    // read from, so the surface's own `--config` is pointed at a temp
    // one. The list it writes is another, to tell the two apart.
    let file = dir.path().join("settings.toml");
    std::fs::write(&file, "").expect("an empty settings file");
    tui.core.pinned.config = Some(file.display().to_string());
    tui.core
        .settings
        .claim("status.done", "[\"cost\"]")
        .expect("a valid claim");
    let said = tui
        .core
        .write_to_file("status.done")
        .expect("the write lands");
    tui.land_lines(Listing::say(said));
    assert_eq!(tui.ui.done, vec![Segment::Cost]);
}

// A reload reaches everything the config installed, not only the values the
// lanes read directly. The compactor is the one object here: it holds the
// summarizer's own connection, so a `summarize_model` that changed has to be
// dialled again rather than left as the one the run started with.
#[test]
fn a_reload_installs_a_new_compactor() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let file = dir.path().join("settings.toml");
    std::fs::write(
        &file,
        "base_url = \"http://127.0.0.1:1/v1\"\nformat = \"openai\"\nsummarize_model = \"cheap\"\n",
    )
    .expect("a settings file");
    tui.core.pinned.config = Some(file.display().to_string());

    let before = tui.core.lane().agent().compactor.clone();
    let said = tui.core.reload();

    assert!(
        !said.iter().any(|s| s.starts_with("nothing reloaded")),
        "{said:?}"
    );
    let after = tui.core.lane().agent().compactor.clone();
    assert!(
        !std::sync::Arc::ptr_eq(&before, &after),
        "a reload installs a compactor built from what the file now says"
    );
}

// A reload follows the running model's entry: a changed one moves the lane
// onto it; a broken one keeps the old wire, says so, and the rest still lands.
#[test]
fn a_reload_follows_the_running_models_entry_or_says_why_not() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let file = dir.path().join("settings.toml");
    std::fs::write(
        &file,
        "base_url = \"http://127.0.0.1:1/v1\"\nformat = \"openai\"\n",
    )
    .expect("a settings file");
    tui.core.pinned.config = Some(file.display().to_string());
    tui.core.pinned.context = Some(12_345);

    let said = tui.core.reload();
    assert!(
        !said.iter().any(|s| s.starts_with("nothing reloaded")),
        "{said:?}"
    );
    assert!(!said.iter().any(|s| s.starts_with("assuming")), "{said:?}");
    assert_eq!(tui.core.lane().agent().spec().context_window, 12_345);

    std::fs::write(&file, "effort = \"high\"\n").expect("a file with no endpoint");
    let said = tui.core.reload();
    assert!(
        said.iter().any(|s| s.contains("not re-dialled")),
        "{said:?}"
    );
    assert_eq!(tui.core.lane().agent().spec().context_window, 12_345);
    assert_eq!(
        tui.core.lane().agent().brief.effort,
        llm::request::Effort::High,
        "the rest of the file still lands"
    );
}

// Leaving with a run in flight cancels it and waits for the transcript to
// come home, rather than writing down the state from before the run started.
#[tokio::test]
async fn leaving_with_a_run_in_flight_takes_the_transcript_back() {
    use agent::session::Session;
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lane().token();
    tui.core
        .lane_mut()
        .begin(tokio_util::sync::CancellationToken::new(), None);

    let mut carried = Session::new();
    carried.prompt("go");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tx.send(crate::ui::tui::job::Done {
        token,
        kind: crate::ui::tui::job::Kind::Turn,
        ran: Some((carried, Ok(llm::stream::Usage::default()))),
    })
    .expect("the job reports");

    tui.settle_all(&mut rx).await;

    assert!(!tui.core.lane().is_running(), "the lane is idle again");
    assert_eq!(
        tui.core.lane().session().map(|s| s.is_empty()),
        Some(false),
        "the transcript came home"
    );
}

// The rows one lane's screen shows, the way a frame reads them.
fn lane_rows(tui: &mut crate::ui::tui::Tui, token: u64) -> Vec<String> {
    drawn_rows(view_at(&mut tui.views, token))
}

// An idle worktree lane whose checkout has been deleted from disk —
// exactly what `drop_vanished_lanes` exists to find.
fn vanished_lane(name: &str) -> Lane {
    let (_dir, mut lane) = a_running_lane();
    lane.finish();
    lane.set_worktree(Some(name.into()));
    std::fs::remove_dir_all(lane.root()).expect("the checkout goes");
    lane
}

// A lane whose transcript is gone — a job that panicked and could not read its
// own back — cannot resume what it is running: answering "no switch" leaves the
// user waiting for a screen that never comes back, with nothing said. The
// refusal is an answer like any other.
#[test]
fn resuming_an_own_id_with_no_transcript_says_so() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let id = tui.core.lane().id().to_string();
    assert!(
        tui.core.lane().session().is_none(),
        "there is nothing to switch to"
    );

    let step = tui.core.dispatch(Intent::Builtin(Builtin::Resume(id)));

    assert!(
        matches!(step, crate::input::Step::Handled(_)),
        "the refusal is a listing, not a swap: {step:?}"
    );
}

// `fate()` says `Now` only for what may run while a run has the transcript.
// This is that claim, kept for the intents whose whole effect is this call: each
// is dispatched against a lane holding a transcript, and none may read it away
// or hand one back. `/reload`, `/worktree` and `/wechat` are `Now` too, and are
// exercised where their effects are.
#[test]
fn a_now_intent_runs_with_the_transcript_a_run_has() {
    let dir = tempfile::tempdir().expect("a checkout");
    let (lane_dir, running) = a_running_lane();
    let mut tui = surface(dir.path());
    switch_to(&mut tui, running);

    // A transcript to be kept: the claim is about what these leave behind, and a
    // lane born without one could not tell the two answers apart.
    let mut session = agent::session::Session::new();
    session.prompt("the first question");
    tui.core.lane_mut().return_session(session);

    for intent in [
        Intent::Builtin(Builtin::Help),
        Intent::Builtin(Builtin::Keys),
        Intent::Builtin(Builtin::Status),
        Intent::Builtin(Builtin::Name("n".into())),
        Intent::Builtin(Builtin::Model(String::new())),
        Intent::Builtin(Builtin::Resume(String::new())),
        Intent::Builtin(Builtin::Settings("x".into())),
    ] {
        assert!(
            matches!(intent.fate(), crate::input::Fate::Now),
            "{intent:?} is no longer `Now`"
        );
        let _ = tui.core.dispatch(intent);
        assert!(
            tui.core.lane().session().is_some(),
            "a `Now` intent may not take the transcript, or hand one back"
        );
    }
    drop(lane_dir);
}

// A line from the phone that has to wait keeps where it came from, or the
// turn it finally starts answers the terminal and the phone hears nothing.
#[tokio::test]
async fn a_queued_line_remembers_the_channel_it_came_from() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lane().token();

    tui.admit(
        Asked::Core(Intent::Prompt("from the phone".into())),
        Origin::Channel("wechat"),
    );
    tui.admit(
        Asked::Core(Intent::Prompt("typed here".into())),
        Origin::Typed,
    );

    let from: Vec<_> = view_at(&mut tui.views, token)
        .queued
        .iter()
        .map(|q| q.origin)
        .collect();
    assert_eq!(from, [Origin::Channel("wechat"), Origin::Typed]);
}

// A line the phone steered in that the run ended before hearing comes back
// bare from the run; it has to come back as the phone's, or its answer stays
// on the terminal.
#[tokio::test]
async fn a_steered_line_the_run_never_heard_comes_back_as_its_channels() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lanes[0].token();
    let steer = agent::Steer::default();
    tui.core.lanes[0].begin(
        tokio_util::sync::CancellationToken::new(),
        Some(steer.clone()),
    );

    tui.admit(
        Asked::Core(Intent::Prompt("heard".into())),
        Origin::Channel("wechat"),
    );
    // The run looked once: that line was heard, the rest were not.
    steer.take();
    // The same words from both sides: told apart by order, never by text.
    tui.admit(Asked::Core(Intent::Prompt("same".into())), Origin::Typed);
    tui.admit(
        Asked::Core(Intent::Prompt("same".into())),
        Origin::Channel("wechat"),
    );
    tui.settle(crate::ui::tui::job::Done {
        token,
        kind: crate::ui::tui::job::Kind::Turn,
        ran: Some((
            agent::session::Session::new(),
            Err(agent::AgentError::Cancelled),
        )),
    })
    .await;

    let back: Vec<_> = view_at(&mut tui.views, token)
        .queued
        .iter()
        .map(|q| match &q.intent {
            Intent::Prompt(text) => (text.clone(), q.origin),
            _ => panic!("only the unheard lines are queued"),
        })
        .collect();
    assert_eq!(
        back,
        [
            ("same".to_string(), Origin::Typed),
            ("same".to_string(), Origin::Channel("wechat"))
        ]
    );
    assert!(
        tui.drivers.steered_lines().is_empty(),
        "the ledger lets go with the run"
    );
}
