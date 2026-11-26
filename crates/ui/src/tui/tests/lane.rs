use crate::tui::row::Row;
use crate::tui::view::Scroll;
use crate::tui::{Asked, Deed, Intent, Origin, View, front_view, view_at};
use agent::Event;
use pi_core::core::lane::Lane;
use pi_core::input::Builtin;
use pi_store::keys::Mode;
use pi_store::listing::Listing;

use super::harness::*;

// Recall belongs to the session, like its transcript: another session in
// the same checkout keeps its own lines out of this one's `k`.
#[tokio::test]
async fn recall_belongs_to_the_session() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());

    let store = tui.core.store.clone();
    filed_recall(&store, dir.path(), "s1", "what the first was asked");
    filed_recall(&store, dir.path(), "s2", "what the second was asked");

    // `on_test_screen` skips the startup seed `Tui::new` does, so this
    // stands in for it: a switch must replace it, not re-read the old session.
    tui.ui
        .editor
        .seed_history("s1".into(), vec!["what the first was asked".to_string()]);
    switch_to(&mut tui, running_lane_as(dir.path(), "s2"));

    let landed = tui.ui.editor.history();
    assert_eq!(
        landed.first().map(String::as_str),
        Some("what the second was asked"),
        "the session in front is what k recalls"
    );
    assert_eq!(landed.len(), 1, "replaced, not appended: {landed:?}");
    assert_eq!(tui.ui.editor.owner(), Some("s2"));
}

// `/new` and `/resume` give the lane in front another session without moving
// it: recall follows the session, not the switch.
#[tokio::test]
async fn recall_follows_a_session_the_lane_stayed_for() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let store = tui.core.store.clone();
    filed_recall(&store, dir.path(), "s1", "asked in s1");
    // A list read for a session this lane no longer holds, as `/new` leaves.
    tui.ui
        .editor
        .seed_history("left behind".into(), vec!["asked elsewhere".to_string()]);

    let was = tui.core.lanes.front();
    tui.reconcile(was);

    assert_eq!(
        tui.ui.editor.history(),
        ["asked in s1"],
        "the lane's own session is what k recalls"
    );
}

// A flash belongs to the lane it answered: carried across a switch it
// would name the wrong checkout on the row that says which one is in front.
#[tokio::test]
async fn a_flash_does_not_follow_the_surface_to_another_lane() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());

    tui.ui.flash("nothing running to stop");
    switch_to(&mut tui, running_lane(dir.path()));
    assert!(tui.ui.flash.is_none(), "the flash was left behind");
}

#[tokio::test]
async fn a_restart_confirmation_survives_redraw_and_resume_without_entering_model_context() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let mut session = agent::session::Session::default();
    session.send_prompt("the earlier request", None);
    tui.core.lane_mut().return_session(session);
    tui.core.lane_mut().finish();
    tui.rebuild_front();
    let context = tui.core.lane().session().unwrap().context();

    let line = "restarted into a new build of v2.8.8";
    tui.restarted(line);
    assert!(tui.ui.flash.is_none());
    tui.ui.flash("another notice");
    let view = front_view(&mut tui.views, tui.core.lane());
    tui.ui.tidy(view);
    tui.ui.flush(tui.core.lane(), view);
    let buf = tui.ui.screen.test_buffer();
    assert!((0..tui.ui.regions.statusline.y).any(|y| {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .contains(line)
    }));

    tui.core.save_filed();
    let lane = tui.core.lane();
    let path = tui.core.store.path_of(lane.root(), lane.id());
    let stored = pi_store::session::Store::read(&path).expect("the confirmation was saved");
    let session = stored.into_session();
    assert_eq!(session.context(), context);
    let view = front_view(&mut tui.views, tui.core.lane());
    tui.ui.rebuild(view, &session, dir.path());
    let rows = drawn_rows(view);
    assert!(rows.iter().any(|row| row.contains("the earlier request")));
    assert_eq!(rows.iter().filter(|row| row.contains(line)).count(), 1);
}

#[tokio::test]
async fn consecutive_restart_confirmations_fold_across_resume_and_live_updates() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let mut session = agent::session::Session::default();
    let line = "restarted into a new build of v2.8.8";
    for _ in 0..2 {
        session.push_notice(line);
    }
    tui.core.lane_mut().return_session(session);
    tui.core.lane_mut().finish();
    tui.rebuild_front();
    let view = front_view(&mut tui.views, tui.core.lane());
    let rows = drawn_rows(view);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].ends_with("×2"), "{rows:?}");

    tui.restarted(line);
    let entries = tui.core.lane().session().unwrap().entries().to_vec();
    assert_eq!(entries.len(), 3, "every restart remains recorded");
    let view = front_view(&mut tui.views, tui.core.lane());
    tui.ui.adopt(view, &entries);
    let live = drawn_rows(view);
    assert_eq!(live.len(), 1);
    assert!(live[0].ends_with("×3"), "{live:?}");

    tui.core.save_filed();
    let lane = tui.core.lane();
    let path = tui.core.store.path_of(lane.root(), lane.id());
    let session = pi_store::session::Store::read(&path)
        .expect("the restarts were saved")
        .into_session();
    assert_eq!(session.entries(), entries);
    let view = front_view(&mut tui.views, tui.core.lane());
    tui.ui.rebuild(view, &session, dir.path());
    assert_eq!(drawn_rows(view), live);
}

// `rebuild` (used by `/resume` and a rewind) clears the banner along with
// the rest, so a switch back must not read that as never drawn and re-open it.
#[tokio::test]
async fn switching_back_to_a_rebuilt_lane_keeps_its_transcript() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    let lane = running_lane(dir.path());
    let mut view = crate::tui::View::opening(&pi_core::core::lane::a_resolved(""), &tui.ui.paint);
    // As `rebuild` leaves it: the conversation, and no banner.
    view.surface.scrollback = vec![Row::notice("what was said before")].into();
    view.surface.opened = false;
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
        .await
        .expect("the lane is listening");
    tui.serve_lanes();
    assert!(
        lane_rows(&mut tui, token)
            .iter()
            .any(|r| r.contains("said behind you")),
        "a lane out of front did not take what its run posted"
    );

    // The screen moves to it, the way a checkout switch does: still there once.
    tui.core.lanes.show(1);
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

// A lane left `Running` queues every later prompt into a queue that only
// drains once it stops running, so every execution kind must come back idle.
#[tokio::test]
async fn every_kind_of_command_leaves_its_lane_idle() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let kinds = || {
        vec![
            ("turn", crate::tui::run::Kind::Turn),
            (
                "bash",
                crate::tui::run::Kind::Bash {
                    lines: vec!["out".into()],
                },
            ),
            ("compact", crate::tui::run::Kind::Compact(None)),
            (
                "compact with a report",
                crate::tui::run::Kind::Compact(Some(agent::Report::default())),
            ),
        ]
    };
    for (what, kind) in kinds() {
        let mut tui = surface(dir.path());
        tui.settle(crate::tui::run::Done {
            request: pi_core::execution::Request::new(
                pi_core::driver::Origin::Typed,
                tui.core.lanes[0].token(),
            ),
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
    // And the same when the execution panicked and brought no transcript home.
    for (what, kind) in kinds() {
        let mut tui = surface(dir.path());
        tui.settle(crate::tui::run::Done {
            request: pi_core::execution::Request::new(
                pi_core::driver::Origin::Typed,
                tui.core.lanes[0].token(),
            ),
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

// A `!` that panicked brings no cursor home; reading that as "nothing yet
// drawn" would lay the recovered transcript over the screen a second time.
#[tokio::test]
async fn a_panicked_bang_does_not_lay_its_transcript_down_again() {
    use agent::session::{Prompt, Session};
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let mut s = Session::new();
    s.push_bash(Prompt {
        text: "Ran `ls`\nfile".into(),
        images: Vec::new(),
        shown: Some("!ls".into()),
        relayed: None,
        context: Vec::new(),
    });
    tui.core.lane_mut().return_session(s);
    let token = tui.core.lane().token();
    // The rows as the screen has them, and the archive as it was saved.
    {
        let session = tui.core.lane().session().expect("the transcript");
        tui.ui.rebuild(
            view_at(&mut tui.views, token),
            session,
            tui.core.lane().root(),
        );
    }
    tui.core.save_lane(0).expect("saved");

    // The execution took the transcript and panicked with it, the way `start_bash`
    // lends one out. Nothing comes back but the archive on disk.
    let _ = tui.core.lane_mut().take_session();

    // The execution panicked: no lines, and no transcript came home.
    tui.settle(crate::tui::run::Done {
        request: pi_core::execution::Request::new(pi_core::driver::Origin::Typed, token),
        kind: crate::tui::run::Kind::Bash { lines: Vec::new() },
        ran: None,
    })
    .await;
    tui.start_turn(crate::tui::clipboard::prompt(
        "the next question".into(),
        None,
        &[],
    ));

    let rows = lane_rows(&mut tui, token);
    assert_eq!(
        rows.iter().filter(|r| *r == "!ls").count(),
        1,
        "the recovered transcript is not laid down twice: {rows:?}"
    );
}

// A stopped command or turn does not inject a cancelled note to the model.
#[tokio::test]
async fn a_stopped_run_does_not_tell_the_model_a_request_was_cancelled() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let stopped = |kind: crate::tui::run::Kind| {
        let dir = dir.path().to_path_buf();
        async move {
            let mut tui = surface(&dir);
            let mut session = agent::session::Session::new();
            session.prompt("the task the user actually asked for");
            tui.settle(crate::tui::run::Done {
                request: pi_core::execution::Request::new(
                    pi_core::driver::Origin::Typed,
                    tui.core.lanes[0].token(),
                ),
                kind,
                ran: Some((session, Err(agent::AgentError::Cancelled.into()))),
            })
            .await;
            let mut back = tui
                .core
                .lanes
                .at_mut(0)
                .take_session()
                .expect("the transcript back");
            back.send_prompt(String::from("now something else"), None::<String>);
            format!("{:?}", back.entries())
        }
    };

    let after_bash = stopped(crate::tui::run::Kind::Bash {
        lines: vec!["some output".into()],
    })
    .await;
    assert!(
        !after_bash.contains("stopped the previous run"),
        "a stopped `!` adds no note: {after_bash}"
    );

    let after_turn = stopped(crate::tui::run::Kind::Turn).await;
    assert!(
        !after_turn.contains("stopped the previous run"),
        "a stopped turn adds no note: {after_turn}"
    );
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
    let window = crate::tui::flash_for(&text);
    ui.flash = Some((
        text,
        std::time::Instant::now()
            .checked_sub(window)
            .expect("a clock"),
    ));
    ui.flush(&lane, &mut view);
    assert!(ui.flash.is_none(), "the expired flash was dropped");
}

// The bar's row is the bar's whether or not it says anything — a coming
// and going row would drop the newest line for the second a flash is up.
#[test]
fn the_bar_keeps_its_row_when_a_flash_comes_and_goes() {
    let mut ui = test_ui(40, 8);
    let (_dir, lane) = a_running_lane();
    let mut view = View::default();
    ui.tabs = vec![tab(crate::tui::Mark::Front, "main")];

    ui.flush(&lane, &mut view);
    let quiet = ui.regions.history.height;
    assert_eq!(
        ui.regions.statusline.height, 1,
        "the bar is a row of its own"
    );

    ui.flash("the only checkout there is");
    ui.flush(&lane, &mut view);
    assert_eq!(
        ui.regions.history.height, quiet,
        "a flash moves nothing above it"
    );
}

// The bar carries the whole ring, lanes' own and disk's alike: a checkout
// no lane has open is on it as soon as read, so `H`/`L` can reach it unpressed.
#[test]
fn the_bar_lists_the_checkouts_no_lane_has_open() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut tui = surface(dir.path());
    tui.core.lanes.at_mut(0).set_worktree(Some("pi-rs".into()));
    tui.ui
        .lists
        .checkouts
        .set(vec![
            tree("pi-rs", Some("main")),
            tree("fix-mem", Some("fix/memory")),
            tree("fw-rm", None),
        ])
        .ok();
    tui.refresh_tabs();

    let shown = bar(&tui.ui, 39);
    let at = |name: &str| {
        shown
            .find(name)
            .unwrap_or_else(|| panic!("{name}: {shown:?}"))
    };
    assert!(
        at("main") < at("fix/memory") && at("fix/memory") < at("fw-rm"),
        "{shown:?}"
    );
    // And the order it lists them in is the order a step walks.
    assert_eq!(
        tui.ui.step_checkout(&tui.core.lanes[0], true).as_deref(),
        Some("fix-mem")
    );
}

// `L`/`H` walk the checkout ring forward/back; every checkout on disk is
// in it (not just open ones), main one first per `core::worktree::list`.
#[test]
fn stepping_the_checkouts_walks_the_ring_and_wraps_both_ways() {
    let ring = |at: Option<&str>, forward: bool| {
        let ui = test_ui(80, 24);
        let trees = ["pi-rs", "f1", "f2"]
            .iter()
            .map(|n| tree(n, Some(n)))
            .collect();
        ui.lists.checkouts.set(trees).ok();
        let (_dir, mut lane) = a_running_lane();
        // A lane in the main checkout names it as its own directory.
        lane.set_worktree(Some(at.unwrap_or("pi-rs").to_string()));
        ui.step_checkout(&lane, forward)
    };
    // The main checkout is the one a lane names as its directory.
    assert_eq!(ring(None, true).as_deref(), Some("f1"));
    assert_eq!(ring(Some("f1"), true).as_deref(), Some("f2"));
    // And round the end, back to the main one.
    assert_eq!(ring(Some("f2"), true).as_deref(), Some("pi-rs"));
    // The other way round, from the main one.
    assert_eq!(ring(None, false).as_deref(), Some("f2"));
    assert_eq!(ring(Some("f1"), false).as_deref(), Some("pi-rs"));
    assert_eq!(ring(Some("f2"), false).as_deref(), Some("f1"));
}

// A step names the checkout it goes to, so a name two checkouts share is one
// stop: stepping from it must land elsewhere, not on itself.
#[test]
fn a_step_never_lands_on_the_checkout_it_left() {
    let ring = |trees: &[&str], at: &str, forward: bool| {
        let ui = test_ui(80, 24);
        ui.lists
            .checkouts
            .set(trees.iter().map(|n| tree(n, Some(n))).collect())
            .ok();
        let (_dir, mut lane) = a_running_lane();
        lane.set_worktree(Some(at.to_string()));
        ui.step_checkout(&lane, forward)
    };
    // The main checkout and a tree made outside the worktrees home share a
    // name; only the first is a stop, so the ring skips the duplicate.
    assert_eq!(
        ring(&["pi-rs", "pi-rs", "f1"], "pi-rs", true).as_deref(),
        Some("f1")
    );
    assert_eq!(
        ring(&["pi-rs", "pi-rs", "f1"], "pi-rs", false).as_deref(),
        Some("f1")
    );
    // A lane git does not list — moved into a subdirectory — reads last on the
    // bar, so the way on from it is the first checkout.
    assert_eq!(
        ring(&["pi-rs", "f1"], "sub", true).as_deref(),
        Some("pi-rs")
    );
    assert_eq!(ring(&["pi-rs", "f1"], "sub", false).as_deref(), Some("f1"));
    // Two names is a ring of two; one is nowhere to go.
    assert_eq!(ring(&["pi-rs"], "pi-rs", true), None);
    assert_eq!(ring(&["solo", "solo"], "solo", true), None);
}

// The ring walks the checkouts in git's order, the one the bar numbers them
// in, whatever order their lanes were opened in.
#[test]
fn the_ring_walks_the_checkouts_order_not_the_lanes() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let trees = ["pi-rs", "feat/add", "fix/choose", "fix/lane", "fix/nmore"]
        .iter()
        .map(|n| tree(n, Some(n)))
        .collect();
    tui.ui.lists.checkouts.set(trees).ok();

    // Opened in an order of their own: git lists checkouts by path, which
    // puts `fix/nmore` last whatever order they were made in.
    tui.core.lanes.at_mut(0).set_worktree(Some("pi-rs".into()));
    let mut guards = Vec::new();
    for name in ["fix/nmore", "feat/add", "fix/choose", "fix/lane"] {
        let (guard, mut lane) = a_running_lane();
        lane.set_worktree(Some(name.into()));
        tui.core.lanes.push(lane);
        guards.push(guard);
    }
    tui.core.lanes.show(4);
    tui.refresh_tabs();

    let names: Vec<&str> = tui.ui.tabs.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["pi-rs", "feat/add", "fix/choose", "fix/lane", "fix/nmore"],
        "the bar lists the checkouts as git does"
    );
    assert_eq!(
        tui.ui.tabs[3].mark,
        crate::tui::Mark::Front,
        "the lane in front is marked where its checkout sits"
    );

    // So a step from the main checkout goes to the checkout after it on
    // disk, not to the lane opened next.
    let step = |at: usize, forward: bool| {
        tui.ui
            .step_checkout(&tui.core.lanes[at], forward)
            .as_deref()
            .map(str::to_string)
    };
    assert_eq!(step(0, true).as_deref(), Some("feat/add"));
    assert_eq!(step(2, true).as_deref(), Some("fix/choose"));
    assert_eq!(step(1, true).as_deref(), Some("pi-rs"), "round the end");
    assert_eq!(step(0, false).as_deref(), Some("fix/nmore"));
}

// A checkout deleted from the shell leaves its lane a dead end; dropping
// idle ones keeps the bar's tab and step ring honest, with front unmoved.
#[test]
fn a_lane_whose_checkout_vanished_is_dropped_and_current_follows() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    // A stale ring entry would let a later step offer — and re-create —
    // the checkout that just went; dropping the lane drops the cache.
    tui.ui
        .lists
        .checkouts
        .set(vec![tree("fix-mem", Some("fix-mem"))])
        .ok();
    let gone = vanished_lane("fix-mem");
    tui.core.lanes.push(gone);
    assert_eq!(tui.core.lanes.len(), 2);

    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 1);
    assert_eq!(tui.core.lanes.front(), 0);
    assert!(
        tui.ui.flash.is_none(),
        "the lane leaving the bar is the whole notice"
    );
    assert!(
        tui.ui.lists.checkouts().is_empty(),
        "the stale ring entry went too"
    );

    // A vanished lane before the one in front shifts its index down.
    let mut tui = surface(dir.path());
    let earlier = vanished_lane("fix-old");
    tui.core.lanes.insert(0, earlier);
    tui.core.lanes.show(1);
    tui.core.lanes.at_mut(1).finish();
    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 1);
    assert_eq!(tui.core.lanes.front(), 0, "the front lane follows its lane");
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
    tui.core.lanes.at_mut(2).finish();
    tui.drop_vanished_lanes();
    assert_eq!(tui.core.lanes.len(), 2);
}

// The bar answers what a lane has finished, not what it's doing: a lane
// working out of sight is plain, and only an ended run wears a mark.
#[test]
fn only_a_finished_lane_wears_a_mark_in_the_bar() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let (_run_dir, mut behind) = a_running_lane();
    behind.set_worktree(Some("fix-mem".into()));
    tui.core.lanes.push(behind);
    tui.refresh_tabs();

    let marks = |tui: &crate::tui::Tui| tui.ui.tabs.iter().map(|t| t.mark).collect::<Vec<_>>();
    assert_eq!(
        marks(&tui),
        vec![crate::tui::Mark::Front, crate::tui::Mark::Plain],
        "a lane working out of sight is just a lane"
    );

    tui.core.lanes.at_mut(1).end(true, false);
    tui.refresh_tabs();
    assert_eq!(
        marks(&tui),
        vec![crate::tui::Mark::Front, crate::tui::Mark::Done]
    );

    tui.core.lanes.at_mut(1).end(false, false);
    tui.refresh_tabs();
    assert_eq!(
        marks(&tui),
        vec![crate::tui::Mark::Front, crate::tui::Mark::Failed]
    );
}

// A lane whose checkout has a driving execution has not finished between
// rounds: `Done` is its mark only once the claim is released.
#[test]
fn a_driven_lane_has_not_finished() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let (_run_dir, mut behind) = a_running_lane();
    behind.set_worktree(Some("fix-mem".into()));
    behind.end(true, false);
    let execution = ::execution::background::RunSink::start(
        &pi_core::driver::background::Commands::new(tui.drivers.background()),
        behind.root().to_path_buf().into(),
        ::execution::background::Invocation::shell("driver"),
        Default::default(),
    );
    execution.channel().drive(true);
    tui.core.lanes.push(behind);
    tui.refresh_tabs();
    assert_eq!(
        tui.ui.tabs[1].mark,
        crate::tui::Mark::Plain,
        "the driver owes this lane another turn"
    );

    execution.channel().drive(false);
    tui.refresh_tabs();
    assert_eq!(
        tui.ui.tabs[1].mark,
        crate::tui::Mark::Done,
        "nothing more is coming"
    );
}

// Nowhere to go is said, not walked to: one checkout has no next.
#[test]
fn a_lone_checkout_has_no_next() {
    let ui = test_ui(80, 24);
    ui.lists
        .checkouts
        .set(vec![tree("pi-rs", Some("pi-rs"))])
        .ok();
    let (_dir, lane) = a_running_lane();
    assert_eq!(ui.step_checkout(&lane, true), None);
    assert_eq!(ui.step_checkout(&lane, false), None);
}

// A half-typed line belongs to the lane it was typed at: a switch parks
// it on that lane's view, and it returns to the editor when the lane does.
#[tokio::test]
async fn a_draft_is_parked_on_the_lane_it_was_typed_at_and_comes_back() {
    let first = tempfile::tempdir().expect("a checkout");
    let second = tempfile::tempdir().expect("another checkout");
    let mut tui = surface(first.path());
    tui.ui.editor.set_line("half a thought meant for this lane");

    let was = tui.core.lanes.front();
    switch_to(&mut tui, running_lane_as(second.path(), "s2"));

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

    tui.core.lanes.show(was);
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

// What a line files is the list's own session, at that session's path, and
// nothing of another session's.
#[tokio::test]
async fn a_typed_line_is_filed_under_the_session_it_belongs_to() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let store = tui.core.store.clone();
    filed_recall(&store, dir.path(), "s2", "not mine");

    tui.ui.editor.seed_history("s1".into(), Vec::new());
    tui.ui.editor.remember("half a thought");
    tui.save_history();

    let mine = std::fs::read_to_string(store.history_path(dir.path(), "s1")).expect("s1's list");
    assert_eq!(crate::tui::editor::decode(&mine), ["half a thought"]);
    let other = std::fs::read_to_string(store.history_path(dir.path(), "s2")).expect("s2's list");
    assert_eq!(
        crate::tui::editor::decode(&other),
        ["not mine"],
        "another session's list is left alone"
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
        .seed_history("s1".into(), vec!["an older prompt".to_string()]);
    tui.ui.editor.set_line("half a thought meant for this lane");
    tui.ui.editor.up();
    assert_eq!(tui.ui.editor.text(), "an older prompt");

    let was = tui.core.lanes.front();
    // Another session, not just another checkout: the switch must rebind.
    switch_to(&mut tui, running_lane_as(second.path(), "s2"));

    assert_eq!(
        view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
        "half a thought meant for this lane",
        "the composing line is parked, not the recalled one"
    );
}

// Normal: `L`/`H` step checkouts, the lowercase pair is left to the caret,
// and `J`/`K` take the window half a screen at a time.
#[test]
fn normal_capitals_step_the_checkouts_and_the_window() {
    let mut ui = vim_ui();
    let trees = ["pi-rs", "f1", "f2"]
        .iter()
        .map(|n| tree(n, Some(n)))
        .collect();
    ui.lists.checkouts.set(trees).ok();
    ui.vim = Some(Mode::Normal);
    let (_dir, mut lane) = a_running_lane();
    // Where a lane in the main checkout sits: it names its own directory.
    lane.set_worktree(Some("pi-rs".into()));
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

    // The lowercase pair stays in the lane it is typed in.
    for lower in ['h', 'l'] {
        let intent = ui.key(&lane, &mut view, typed(lower), false);
        assert!(
            matches!(intent, Asked::Own(Deed::Nothing)),
            "`{lower}`: {intent:?}"
        );
    }

    // And the window moves without the caret: scrolled up by J, back by K.
    view.surface.scroll = Scroll::newest();
    ui.key(&lane, &mut view, typed('K'), false);
    let up = view.surface.scroll.by;
    assert!(up > 0, "K went back through the window: {up}");
    ui.key(&lane, &mut view, typed('J'), false);
    assert!(
        view.surface.scroll.by < up,
        "J came forward again: {}",
        view.surface.scroll.by
    );
}

// After editing, the project file is read back and reaches the
// surface; a key a flag still outranks is named, not silently ignored.
#[tokio::test]
async fn an_edited_project_file_reaches_the_surface_and_names_what_outranks_it() {
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::create_dir(dir.path().join(".pi")).unwrap();
    let mut tui = surface(dir.path());
    // An empty user file of its own, so the reload never reads the real one.
    let user = dir.path().join("settings.toml");
    std::fs::write(&user, "").expect("an empty settings file");
    tui.core.pinned.config = Some(user.display().to_string());
    tui.core.pinned.effort = Some(pi_store::args::EffortArg::High);

    std::fs::write(dir.path().join(".pi/settings.toml"), "effort = \"low\"\n")
        .expect("the project file");
    let said = tui.core.config_edited().expect("reloaded");
    assert!(
        said.iter()
            .any(|l| l.starts_with("effort: --effort outranks")),
        "{said:?}"
    );
    tui.land_lines(Listing::say(said));
}

#[tokio::test]
async fn a_turn_that_adopts_project_settings_refreshes_edit_completion() {
    use pi_store::config::PROJECT_FILE;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::create_dir_all(workspace.join(".pi")).unwrap();
    let user = dir.path().join("user.toml");
    fs::write(&user, "").unwrap();
    let mut tui = surface(&workspace);
    tui.core.pinned.config = Some(user.display().to_string());
    tui.ui.lists.pinned = tui.core.pinned.clone();
    tui.refresh_files();
    assert!(
        !tui.ui
            .lists
            .editable()
            .iter()
            .any(|file| file.name == PROJECT_FILE)
    );

    tui.settle(crate::tui::run::Done {
        request: pi_core::execution::Request::new(
            crate::tui::Origin::Typed,
            tui.core.lane().token(),
        ),
        kind: crate::tui::run::Kind::Turn,
        ran: Some((
            agent::session::Session::new(),
            Ok(llm::stream::Usage::default()),
        )),
    })
    .await;
    fs::write(workspace.join(PROJECT_FILE), "effort = \"low\"\n").unwrap();
    tui.start_turn(crate::tui::clipboard::prompt("hello".into(), None, &[]));
    assert_eq!(
        tui.core.refresh_config(),
        None,
        "the turn read the new file"
    );
    assert!(
        tui.ui
            .lists
            .editable()
            .iter()
            .any(|file| file.name == PROJECT_FILE)
    );
}

// File changes must not add transcript rows or repeat a refusal on every look.
#[test]
fn a_saved_settings_file_lands_without_being_asked_and_a_broken_one_once() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let file = dir.path().join("settings.toml");
    std::fs::write(&file, "").expect("a settings file");
    tui.core.pinned.config = Some(file.display().to_string());
    tui.refresh_files();
    assert_eq!(tui.core.refresh_config(), None, "nothing moved");

    let view = front_view(&mut tui.views, tui.core.lane());
    view.state.work = a_turn(false);
    view.state.work.as_mut().expect("a running turn").since =
        std::time::Instant::now() - std::time::Duration::from_secs(2);
    let before = drawn_rows(view);
    std::fs::write(&file, "effort = \"high\"\n").expect("an edit");
    tui.refresh_files();
    let (said, _) = tui.ui.flash.as_ref().expect("the edit is taken up");
    assert!(said.starts_with("reloaded — "), "{said:?}");
    assert_eq!(
        tui.core.lane().agent().brief.effort,
        llm::request::Effort::High
    );
    let view = front_view(&mut tui.views, tui.core.lane());
    tui.ui.flush(tui.core.lane(), view);
    assert_eq!(
        drawn_rows(view),
        before,
        "reload leaves the transcript alone"
    );
    let bar = tui.ui.regions.statusline;
    let buf = tui.ui.screen.test_buffer();
    let row = |y| {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
    };
    assert!(
        row(bar.y).contains("reloaded"),
        "the bar carries the notice"
    );

    std::fs::write(&file, "effort = ").expect("a half-written file");
    tui.refresh_files();
    let said = tui.ui.flash.clone().expect("said once");
    assert!(said.0.starts_with("nothing reloaded"), "{said:?}");
    tui.refresh_files();
    assert_eq!(tui.ui.flash.as_ref(), Some(&said), "and not again");
    assert_eq!(
        drawn_rows(front_view(&mut tui.views, tui.core.lane())),
        before,
        "a failed reload also stays out of the transcript"
    );

    std::fs::write(&file, "effort = \"low\"\n").expect("the fix");
    tui.refresh_files();
    assert!(tui.ui.flash.as_ref().unwrap().0.starts_with("reloaded — "));
    assert_eq!(
        tui.core.lane().agent().brief.effort,
        llm::request::Effort::Low
    );
}

// A reload reaches everything the config installed, not just what lanes
// read directly: the compactor holds its own connection, re-dialled on change.
#[test]
fn a_reload_installs_a_new_compactor() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let file = dir.path().join("settings.toml");
    std::fs::write(
        &file,
        "summarize_model = \"local/cheap\"\n[provider.local]\nbase_url = \"http://127.0.0.1:1/v1\"\nformat = \"openai\"\n",
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

#[test]
fn a_key_only_reload_replaces_the_running_connection() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let file = dir.path().join("settings.toml");
    let settings = |key| {
        format!(
            "[provider.local]\nbase_url = \"http://127.0.0.1:1/v1\"\nformat = \"chat\"\napi_key = \"{key}\"\n"
        )
    };
    std::fs::write(&file, settings("old-test-key")).unwrap();
    tui.core.pinned.config = Some(file.display().to_string());
    tui.core.try_reload().unwrap();
    let before = tui.core.lane().agent().model.clone();
    std::fs::write(&file, settings("new-test-key")).unwrap();
    tui.core.try_reload().unwrap();
    assert!(!std::sync::Arc::ptr_eq(
        &before,
        &tui.core.lane().agent().model
    ));
    assert_eq!(
        tui.core.config().key_for("local/m").as_deref(),
        Some("new-test-key")
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
        "[provider.local]\nbase_url = \"http://127.0.0.1:1/v1\"\nformat = \"openai\"\n",
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
        .begin(tokio_util::sync::CancellationToken::new());

    let mut carried = Session::new();
    carried.prompt("go");
    tui.core.fixture_completion(crate::tui::run::Done {
        request: pi_core::execution::Request::new(pi_core::driver::Origin::Typed, token),
        kind: crate::tui::run::Kind::Turn,
        ran: Some((carried, Ok(llm::stream::Usage::default()))),
    });

    tui.settle_all("The user exited pi.").await;

    assert!(!tui.core.lane().is_running(), "the lane is idle again");
    assert_eq!(
        tui.core.lane().session().map(|s| s.is_empty()),
        Some(false),
        "the transcript came home"
    );
}

// The rows one lane's screen shows, the way a frame reads them.
fn lane_rows(tui: &mut crate::tui::Tui, token: u64) -> Vec<String> {
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

// A lane whose transcript is gone cannot resume what it is running;
// answering "no switch" would leave the user waiting with nothing said.
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
        matches!(step, pi_core::input::Step::Notice(ref why) if !why.is_empty()),
        "the refusal is a notice, not a swap: {step:?}"
    );
}

// `fate()`'s `Now` claim: none of these may read the transcript away or
// hand one back while a run holds it. Other `Now` intents are exercised elsewhere.
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
        Intent::Surface {
            word: crate::tui::keys::KEYS.into(),
            args: String::new(),
        },
        Intent::Builtin(Builtin::Status),
        Intent::Builtin(Builtin::Name("n".into())),
        Intent::Builtin(Builtin::ListModels),
        Intent::Builtin(Builtin::Resume(String::new())),
        Intent::Builtin(Builtin::Edit(String::new())),
    ] {
        assert!(
            matches!(intent.fate(), pi_core::input::Fate::Now),
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

// A line from an execution that has to wait keeps where it came from, or the turn
// it finally starts answers the terminal and the execution hears nothing.
#[tokio::test]
async fn a_queued_line_remembers_the_command_it_came_from() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let token = tui.core.lane().token();

    tui.admit(
        Asked::Core(Intent::Prompt("from the phone".into())),
        Origin::Input(1),
        None,
        "",
    );
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("typed here".into(), None, &[]),
        Origin::Typed,
        None,
    );

    let from: Vec<_> = view_at(&mut tui.views, token)
        .queued
        .lock()
        .iter()
        .map(|q| q.request.origin)
        .collect();
    assert_eq!(from, [Origin::Input(1), Origin::Typed]);
}

#[tokio::test]
async fn a_typed_followup_waits_for_the_run_and_survives_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let token = tui.core.lane().token();
    tui.admit(
        Asked::Core(Intent::Bash("echo first".into())),
        Origin::Typed,
        Some("!echo first".into()),
        "",
    );
    let wake = tui.admit(
        Asked::Core(Intent::Prompt("new direction".into())),
        Origin::Typed,
        Some("new direction".into()),
        "",
    );
    assert!(matches!(
        wake,
        crate::tui::Wake::Do(Asked::Core(Intent::Prompt(_)), _)
    ));
    let ask = agent::session::Prompt {
        text: "model text".into(),
        shown: Some("new direction".into()),
        images: vec![llm::message::Image::Url {
            url: "https://example.test/image.png".into(),
        }],
        relayed: Some("original provenance".into()),
        context: Vec::new(),
    };
    assert!(!tui.deliver_prompt(ask.clone(), Origin::Typed, Some("new direction".into()),));
    assert!(!tui.core.runtime_busy(), "no second run was started");
    assert!(tui.core.lane_mut().stop(false));
    tui.settle(crate::tui::run::Done {
        request: pi_core::execution::Request::new(pi_core::driver::Origin::Typed, token),
        kind: crate::tui::run::Kind::Turn,
        ran: Some((
            agent::session::Session::with_prompt("original"),
            Err(agent::AgentError::Cancelled.into()),
        )),
    })
    .await;

    assert!(matches!(
        view_at(&mut tui.views, token)
            .queued
            .pop_front()
            .unwrap()
            .input,
        crate::tui::view::Pending::Intent(Intent::Bash(_))
    ));
    let queued = view_at(&mut tui.views, token).queued.pop_front().unwrap();
    assert!(view_at(&mut tui.views, token).queued.is_empty());
    assert_eq!(queued.line.as_deref(), Some("new direction"));
    let crate::tui::view::Pending::Prompt(waited) = queued.input else {
        panic!("the prompt waits as it was read");
    };
    assert_eq!(waited.prompt(), &ask);
    assert!(tui.deliver_admitted(waited, queued.request, queued.line));
    let completed = completed_run(&mut tui).await;
    tui.present_settlement(pi_core::execution::host::Settlement::Returned(completed));
    assert_eq!(tui.core.lane().session().unwrap().last_prompt(), Some(&ask));
}

// New inputs retain admission order and remain isolated from other lanes.
#[test]
fn followups_wait_behind_commands_and_remote_input_in_their_own_lane() {
    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    let queue = front_view(&mut tui.views, tui.core.lane()).queued.clone();
    tui.admit(
        Asked::Core(Intent::Bash("echo first".into())),
        Origin::Typed,
        Some("! echo first".into()),
        "",
    );
    tui.admit(
        Asked::Core(Intent::Prompt("remote input".into())),
        Origin::Input(1),
        None,
        "",
    );
    let mut ask = crate::tui::clipboard::prompt("after bash".into(), None, &[]);
    ask.images.push(llm::message::Image::Url {
        url: "https://example.test/image.png".into(),
    });
    tui.deliver_prompt(ask.clone(), Origin::Typed, Some("after bash".into()));
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("second direction".into(), None, &[]),
        Origin::Typed,
        Some("second direction".into()),
    );
    assert_eq!(queue.lock().len(), 4);

    switch_to(&mut tui, running_lane(dir.path()));
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("another lane".into(), None, &[]),
        Origin::Typed,
        Some("another lane".into()),
    );
    assert_eq!(
        front_view(&mut tui.views, tui.core.lane())
            .queued
            .lock()
            .len(),
        1
    );

    assert!(matches!(
        queue.pop_front().unwrap().input,
        crate::tui::view::Pending::Intent(Intent::Bash(_))
    ));
    assert_eq!(queue.pop_front().unwrap().request.origin, Origin::Input(1));
    let crate::tui::view::Pending::Prompt(pending) = queue.pop_front().unwrap().input else {
        panic!("expected prompt")
    };
    assert_eq!(pending.prompt(), &ask);
    let crate::tui::view::Pending::Prompt(pending) = queue.pop_front().unwrap().input else {
        panic!("expected prompt")
    };
    assert_eq!(pending.prompt().text, "second direction");
    assert!(queue.is_empty());
    assert_eq!(
        front_view(&mut tui.views, tui.core.lane())
            .queued
            .lock()
            .len(),
        1
    );
}

#[tokio::test]
async fn up_inside_a_multiline_draft_keeps_steering_and_waiting_commands() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    tui.admit(
        Asked::Core(Intent::Bash("echo first".into())),
        Origin::Typed,
        Some("!echo first".into()),
        "",
    );
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("new direction".into(), None, &[]),
        Origin::Typed,
        Some("new direction".into()),
    );
    let queue = front_view(&mut tui.views, tui.core.lane()).queued.clone();
    tui.ui.editor.set_line("first line\nsecond line");
    let asked = tui.ui.key(
        tui.core.lane(),
        front_view(&mut tui.views, tui.core.lane()),
        crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        true,
    );
    assert!(matches!(asked, Asked::Own(Deed::Nothing)));
    assert_eq!(tui.ui.editor.text(), "first line\nsecond line");
    assert!(
        tui.ui.editor.recalls_next(),
        "the caret moved to the first line"
    );
    assert_eq!(queue.lock().len(), 2, "neither queue was recalled");
    assert!(matches!(
        queue.pop_front().unwrap().input,
        crate::tui::view::Pending::Intent(Intent::Bash(_))
    ));
    let crate::tui::view::Pending::Prompt(pending) = queue.pop_front().unwrap().input else {
        panic!("expected prompt")
    };
    assert_eq!(pending.prompt().text, "new direction");
}

#[tokio::test]
async fn recalling_steering_and_waiting_commands_keeps_submission_order_and_remote_input() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let dir = tempfile::tempdir().unwrap();
    let mut tui = surface(dir.path());
    tui.admit(
        Asked::Core(Intent::Bash("echo first".into())),
        Origin::Typed,
        Some("!echo first".into()),
        "",
    );
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("first direction".into(), None, &[]),
        Origin::Typed,
        Some("first direction".into()),
    );
    tui.admit(
        Asked::Core(Intent::Prompt("remote input".into())),
        Origin::Input(1),
        None,
        "",
    );
    tui.admit(
        Asked::Core(Intent::Bash("echo second".into())),
        Origin::Typed,
        Some("!echo second".into()),
        "",
    );
    tui.deliver_prompt(
        crate::tui::clipboard::prompt("second direction".into(), None, &[]),
        Origin::Typed,
        Some("second direction".into()),
    );
    let queue = front_view(&mut tui.views, tui.core.lane()).queued.clone();
    let ids: Vec<_> = queue
        .lock()
        .iter()
        .filter(|q| q.line.is_some())
        .map(|q| q.request)
        .collect();
    tui.ui.editor.set_line("draft");
    let asked = tui.ui.key(
        tui.core.lane(),
        front_view(&mut tui.views, tui.core.lane()),
        crate::tui::TermEvent::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        true,
    );
    let Asked::Own(Deed::RecallQueued(recalled)) = asked else {
        panic!("both kinds of pending typed input can be recalled");
    };
    assert_eq!(recalled, ids);
    tui.carry(Deed::RecallQueued(recalled)).await;
    assert_eq!(
        tui.ui.editor.text(),
        "!echo first\nfirst direction\n!echo second\nsecond direction\ndraft"
    );
    assert!(tui.core.lane().is_running());
    assert_eq!(queue.pop_front().unwrap().request.origin, Origin::Input(1));
    assert!(queue.is_empty());
}

// Background output is an automatic input; command-like output is literal. One
// that arrives under the running turn joins it instead of opening another.
#[tokio::test]
async fn queued_command_results_open_one_turn_under_their_label() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.core.lane_mut().finish();
    tui.core
        .lane_mut()
        .return_session(agent::session::Session::new());
    tui.core.enable_background(tui.drivers.background());
    let execution = ::execution::background::RunSink::start(
        &pi_core::driver::background::Commands::new(tui.drivers.background()),
        dir.path().to_path_buf().into(),
        ::execution::background::Invocation::shell("build"),
        Default::default(),
    );
    let results = ["check the build", "/compact"];
    for text in results {
        execution.channel().result(text.into());
    }
    execution.end();
    let event = next_event(&mut tui).await;
    let pi_core::execution::host::Event::Started(started) = &event else {
        panic!("the core starts automatic input")
    };
    assert!(started.entries.iter().any(|entry|matches!(entry,agent::session::Entry::Ask {ask,..} if ask.text.contains(results[0]) && ask.shown.is_some())));
    tui.runtime_event(event);
    let completed = completed_run(&mut tui).await;
    tui.present_settlement(pi_core::execution::host::Settlement::Returned(completed));
    let asks: Vec<&agent::session::Prompt> = tui
        .core
        .lane()
        .session()
        .into_iter()
        .flat_map(|session| session.entries())
        .filter_map(|entry| match entry {
            agent::session::Entry::Ask { ask, .. } => Some(ask),
            _ => None,
        })
        .collect();
    for text in results {
        assert!(
            asks.iter()
                .any(|ask| ask.text.contains(text) && ask.shown.is_some()),
            "{text} is admitted as prose under its label, got {asks:?}"
        );
    }
    assert!(tui.driver_line().is_none());
}

// Remote submissions share command parsing, including the leading-space
// escape for prose that begins with a slash.
#[tokio::test]
async fn remote_submissions_use_the_same_parser_as_local_lines() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.core.lane_mut().finish();
    let execution = ::execution::background::RunSink::start(
        &pi_core::driver::background::Commands::new(tui.drivers.background()),
        dir.path().to_path_buf().into(),
        ::execution::background::Invocation::shell("wechat"),
        Default::default(),
    );
    for line in ["/compact", "!echo hello", " /compact"] {
        execution.channel().input(line.into(), String::new());
        let Some((Origin::Input(_), crate::tui::Wake::Do(Asked::Core(intent), _))) =
            tui.driver_line()
        else {
            panic!("{line} is admitted");
        };
        assert_eq!(
            intent,
            pi_core::input::read(
                line,
                tui.core.commands(),
                Some(&tui.core.lane().agent().brief.registry)
            )
        );
    }
}

// A note belongs to the turn its line opens: a command that opens none files
// none, so a driving script's note cannot ride an unrelated turn later.
#[tokio::test]
async fn a_note_is_filed_only_for_a_line_that_starts_a_turn() {
    fn noted(tui: &crate::tui::Tui, text: &str) -> bool {
        (tui.core.lane().session().map(|s| s.entries().to_vec()))
            .unwrap_or_default()
            .iter()
            .any(|e| matches!(e, agent::session::Entry::Note { note, .. } if note.contains(text)))
    }

    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.core.lane_mut().finish();
    tui.core
        .lane_mut()
        .return_session(agent::session::Session::new());
    let execution = ::execution::background::RunSink::start(
        &pi_core::driver::background::Commands::new(tui.drivers.background()),
        dir.path().to_path_buf().into(),
        ::execution::background::Invocation::shell("driver"),
        Default::default(),
    );

    execution
        .channel()
        .input("/status".into(), "This is loop round 2.".into());
    assert!(
        tui.driver_line().is_some(),
        "the command is carried out all the same"
    );
    assert!(
        !noted(&tui, "round 2"),
        "a line that opens no turn files no note"
    );

    execution
        .channel()
        .input("!echo hi".into(), "This is loop round 4.".into());
    assert!(
        tui.driver_line().is_some(),
        "a shell command is carried out all the same"
    );
    assert!(!noted(&tui, "round 4"), "a shell line opens no turn either");

    execution
        .channel()
        .input("keep going".into(), "This is loop round 5.".into());
    assert!(tui.driver_line().is_some());
    assert!(
        noted(&tui, "round 5"),
        "a prompt's note goes before its turn"
    );
}

// A note submitted under a run rides its own line: the run takes both together
// at its next request, never the note alone.
#[tokio::test]
async fn a_queued_line_keeps_its_note_until_the_run_takes_both() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let execution = ::execution::background::RunSink::start(
        &pi_core::driver::background::Commands::new(tui.drivers.background()),
        dir.path().to_path_buf().into(),
        ::execution::background::Invocation::shell("driver"),
        Default::default(),
    );

    execution
        .channel()
        .input("keep going".into(), "This is loop round 6.".into());
    assert!(
        matches!(
            tui.driver_line(),
            Some((Origin::Input(_), crate::tui::Wake::Nothing))
        ),
        "a line sent under a run waits for it"
    );
    let held = (tui.core.lane().notes()).expect("the run holds the transcript");
    assert!(
        held.take().is_none(),
        "the note waits with its line, not with the request in flight"
    );
    let queued = (front_view(&mut tui.views, tui.core.lane())
        .queued
        .pop_front())
    .expect("the line waits on its lane");
    assert_eq!(queued.note, "This is loop round 6.");
}

// A note filed while idle waits for the next turn's save; quitting or a
// restart into an update must not take it with them unsaved.
#[tokio::test]
async fn leaving_saves_what_an_idle_lane_filed() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    tui.core.lane_mut().finish();
    tui.core
        .lane_mut()
        .return_session(agent::session::Session::new());
    let path = tui.core.store.path_of(dir.path(), tui.core.lane().id());
    // Nothing said: leaving writes no empty session to be offered back.
    tui.settle_all("The user exited pi.").await;
    assert!(!path.exists());

    tui.core
        .lane_mut()
        .push_note("The user stopped background command #2.");
    tui.settle_all("The user exited pi.").await;
    let stored = pi_store::session::Store::read(&path).expect("saved on the way out");
    assert!(stored.session.entries().iter().any(|e| matches!(
        e,
        agent::session::Entry::Note { note, .. } if note.contains("command #2")
    )));
}

// A command stopped by leaving has a cause in the resumed model context, even
// when the transcript was lent to a turn as shutdown began.
#[tokio::test]
async fn leaving_with_a_command_preserves_its_stop_cause_on_resume() {
    use ::execution::background::{Again, RunSink as _};

    for running in [false, true] {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let mut carried = agent::session::Session::new();
        carried.prompt("run sleep 30 in the background");
        if running {
            tui.core.fixture_completion(crate::tui::run::Done {
                request: pi_core::execution::Request::new(
                    pi_core::driver::Origin::Typed,
                    tui.core.lane().token(),
                ),
                kind: crate::tui::run::Kind::Turn,
                ran: Some((carried, Err(agent::AgentError::Cancelled.into()))),
            });
        } else {
            tui.core.lane_mut().finish();
            tui.core.lane_mut().return_session(carried);
        }
        tui.core.enable_background(tui.drivers.background());
        let stop = tokio_util::sync::CancellationToken::new();
        let execution = pi_core::driver::background::Commands::new(tui.drivers.background()).start(
            dir.path().into(),
            ::execution::background::Invocation::shell("sleep 30; echo done"),
            stop.clone(),
        );
        execution.rerun(
            std::sync::Arc::new(|_, _| Ok(())),
            Again {
                command: "sleep 30; echo done".into(),
                cwd: dir.path().into(),
                ..Again::default()
            },
        );
        let path = tui.core.store.path_of(dir.path(), tui.core.lane().id());

        tui.settle_all("The user exited pi.").await;
        tui.core.hand_over_background();
        assert!(stop.is_cancelled());

        let stored = pi_store::session::Store::read(&path).expect("saved on exit");
        let context = serde_json::to_string(&stored.session.context()).unwrap();
        assert!(context.contains("The user exited pi."), "{context}");
        assert!(context.contains("#1 (sleep 30; echo done)"), "{context}");
        assert!(context.contains("during shutdown"), "{context}");
        assert!(context.contains("did not finish normally"), "{context}");

        let mut resumed = surface(dir.path());
        resumed.core.lane_mut().finish();
        resumed
            .core
            .lane_mut()
            .return_session(stored.into_session());
        resumed.core.enable_background(resumed.drivers.background());
        resumed.core.revive_commands();
        let executions = resumed.drivers.background();
        assert_eq!(
            executions.background(dir.path())[0].state,
            pi_core::driver::background::State::Stopped
        );
        assert!(
            resumed.driver_line().is_none(),
            "resuming does not restart the execution"
        );
    }
}

// A line draws the model as its endpoint names it; the identity that
// addresses the provider stays in the bar's input for a script that needs it.
#[test]
fn a_line_draws_the_models_name_and_the_input_keeps_its_identity() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let view = crate::tui::front_view(&mut tui.views, tui.core.lane());
    let facts = crate::tui::statusline::Facts::of(
        tui.core.lane(),
        view,
        None,
        Vec::new(),
        Default::default(),
    );
    assert_eq!(
        facts.value(pi_store::statusline::Field::Model).as_deref(),
        Some("m"),
        "the bar draws the endpoint's name"
    );
    assert_eq!(tui.core.lane().model(), "local/m", "/status names it whole");
}
