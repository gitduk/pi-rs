use pi_core::core::lane::Lane;
use pi_core::driver::Ended;
use pi_core::driver::looping::{Cut, Loops, Round};
use pi_core::input::{Builtin, Drive, Intent, Step};

use super::harness::*;

// The whole point of the command: what decides another round is the tree,
// so a pass that believes it is finished is overruled by the file it just
// changed.
#[test]
fn a_loop_goes_round_while_the_tree_keeps_changing() {
    let (_dir, mut lane) = a_running_lane();
    let mut loops = Loops::default();
    loops
        .start(lane.token(), "/code-review high".into(), lane.ctx())
        .expect("no loop yet");

    let due = loops.due(lane.token()).expect("the first round is due");
    assert_eq!(due.goal, "/code-review high", "the goal goes out verbatim");
    loops.ask(lane.token());
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));

    // The same file again, with different content — what a loop like this
    // does most of the time is keep working the files it has already
    // touched. A record that only counted writes would call this idle and
    // stop; the fingerprint sees the rewrite.
    started(&mut loops, &lane);
    wrote(
        &mut lane,
        "a.rs",
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    );
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));

    // Nothing changed: a pass with nothing to do has nothing to do next
    // time either.
    started(&mut loops, &lane);
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Quiet)
    ));
    assert!(!loops.active(lane.token()), "and the loop is gone");
    assert!(
        ended(&mut loops, &lane, Ended::Done, None).is_none(),
        "a later turn is not a round"
    );
}

// A line typed between rounds ends a turn too. Counting it would move the
// loop on work it never ran — and end it, if that line wrote nothing.
#[test]
fn a_turn_the_loop_did_not_start_is_not_one_of_its_rounds() {
    let (_dir, mut lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();

    // Somebody else's turn settling, before the round was handed out.
    assert!(
        ended(&mut loops, &lane, Ended::Done, None).is_none(),
        "not the loop's round"
    );
    assert!(loops.active(lane.token()), "and the loop is untouched");

    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));
}

// Esc stops the loop and not merely the round it caught: a cut round is
// the loop ending, not a pause before the next one.
#[test]
fn a_cut_round_takes_the_loop_with_it() {
    let (_dir, mut lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();
    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Stopped, None),
        Some(Round::Cut(Cut::Stopped))
    ));
    assert!(!loops.active(lane.token()));
}

// The ceiling is the floor for a loop no other brake can catch: one that
// keeps changing files forever without ever repeating itself.
#[test]
fn a_loop_stops_at_the_configured_ceiling_with_work_still_left() {
    let (_dir, mut lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();
    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, Some(1)),
        Some(Round::Capped(1))
    ));
    assert!(!loops.active(lane.token()));

    // The loop itself has no ceiling at `None` — the config's default is
    // layered above it, at `Config::loop_cap`.
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();
    started(&mut loops, &lane);
    wrote(&mut lane, "b.rs", "fn b() {}\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));
}

// A stopped loop hands out no round and hears no turn; a second `/loop`
// while one is in force is refused rather than replacing it.
#[test]
fn a_stopped_loop_is_gone_and_a_running_one_is_not_replaced() {
    let (_dir, lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();
    let refused = loops
        .start(lane.token(), "other".into(), lane.ctx())
        .expect_err("one loop per lane");
    assert!(refused.contains("`go` is already looping"), "{refused}");

    let said = loops.stop(lane.token()).expect("a loop was in force");
    assert!(said.contains("0 round(s) of `go`"), "{said}");
    assert!(loops.due(lane.token()).is_none(), "no round after a stop");
    loops.ask(lane.token());
    assert!(
        ended(&mut loops, &lane, Ended::Done, None).is_none(),
        "and no turn heard"
    );
    assert!(loops.stop(lane.token()).is_none(), "nothing left to stop");
}

// A round that started no turn leaves nothing to measure, so the loop ends
// there instead of waiting on a turn that never comes.
#[test]
fn a_round_that_starts_no_turn_ends_the_loop() {
    let (_dir, lane) = a_running_lane();
    let mut loops = Loops::default();
    loops
        .start(lane.token(), "/gone".into(), lane.ctx())
        .unwrap();
    loops.due(lane.token()).expect("the first round is due");
    let said = loops.unstarted(lane.token()).expect("the loop ends");
    assert!(said.contains("starts no turn"), "{said}");
    assert!(!loops.active(lane.token()));
}

// A lane that goes — its checkout removed — takes its loop with it, and
// says so: every other way a loop ends leaves a line.
#[test]
fn a_loop_whose_lane_is_gone_ends_and_says_so() {
    let (_dir, lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();
    assert!(
        loops.retain(|_| true).is_empty(),
        "a live lane keeps its loop"
    );
    let said = loops.retain(|_| false);
    assert_eq!(said.len(), 1);
    assert!(said[0].contains("`go` lost its checkout"), "{}", said[0]);
    assert!(!loops.active(lane.token()));
}

// A round that restores the tree to a fingerprint it wore earlier is a
// loop seesawing forever — the round after the rewrite undid it.
#[test]
fn a_round_that_undoes_the_last_one_stops_as_oscillating() {
    let (_dir, mut lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();
    let six = "a\nb\nc\nd\ne\nf\n";
    let six_more = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", six);
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));

    // Round 2 rewrites at full size; round 3 puts the first bytes back —
    // the tree is exactly what round 1 wore, and any later round would
    // seesaw between the two.
    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", six_more);
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));

    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", six);
    assert!(
        matches!(
            ended(&mut loops, &lane, Ended::Done, None),
            Some(Round::Oscillating)
        ),
        "the tree returned to a fingerprint the loop has already worn"
    );
    assert!(!loops.active(lane.token()), "and the loop is gone");
}

// The fingerprint cannot catch a loop that keeps nibbling — one line an
// hour, forever. Two such rounds in a row are the noise floor, and the
// loop stops rather than polish past the point of return.
#[test]
fn rounds_that_only_nibble_stop_as_thin() {
    let (_dir, mut lane) = a_running_lane();
    let mut loops = Loops::default();
    loops.start(lane.token(), "go".into(), lane.ctx()).unwrap();

    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", "one\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Again)
    ));

    started(&mut loops, &lane);
    wrote(&mut lane, "a.rs", "one\ntwo\n");
    assert!(matches!(
        ended(&mut loops, &lane, Ended::Done, None),
        Some(Round::Thin)
    ));
    assert!(!loops.active(lane.token()));
}

// A goal is checked at the door, by the same reading `dispatch` gives it:
// only a line that starts a turn leaves a loop anything to measure.
#[test]
fn only_a_goal_that_starts_a_turn_is_looped() {
    let dir = tempfile::tempdir().expect("a checkout");
    let mut tui = surface(dir.path());
    let mut looped = |goal: &str| {
        tui.core
            .dispatch(Intent::Builtin(Builtin::Loop(goal.into())))
    };
    assert!(
        matches!(looped("fix the tests"), Step::Drive(Drive::Loop(Some(g))) if g == "fix the tests")
    );
    assert!(matches!(
        looped("!cargo test"),
        Step::Drive(Drive::Loop(Some(_)))
    ));
    assert!(
        matches!(looped("  "), Step::Drive(Drive::Loop(None))),
        "bare stops"
    );
    for goal in ["/help", "/loop go", "/nosuchskill"] {
        assert!(matches!(looped(goal), Step::Flash(_)), "{goal}");
    }
}

// ------------------------------------------------------------- looping
// The round the loop owes, handed out and started, as the surface does it.
fn started(loops: &mut Loops, lane: &Lane) {
    loops.due(lane.token()).expect("a round is due");
    loops.ask(lane.token());
}

// A turn on the lane ending.
fn ended(loops: &mut Loops, lane: &Lane, how: Ended, cap: Option<usize>) -> Option<Round> {
    loops.turn_ended(lane.token(), &how, lane.ctx(), cap)
}

// Write `body` to `name` in the lane's workspace and record the write, so
// the tree fingerprint the loop reads has real bytes to hash.
fn wrote(lane: &mut Lane, name: &str, body: &str) {
    let path = lane.root().join(name);
    std::fs::write(&path, body).expect("writes into the temp workspace");
    lane.ctx_mut().note_write(&path);
}
