use crate::core::lane::Lane;
use crate::core::looping::{Cut, Round};

use super::harness::*;

// The whole point of the command: what decides another round is the tree,
// so a pass that believes it is finished is overruled by the file it just
// changed.
#[test]
fn a_loop_goes_round_while_the_tree_keeps_changing() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("/code-review high".into());

    lane.loop_running();
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    let again = lane.loop_step(None, None).expect("a loop is in force");
    assert!(
        matches!(&again, Round::Again { goal } if goal == "/code-review high"),
        "the goal goes back verbatim",
    );

    // The same file again, with different content — what a loop like this
    // does most of the time is keep working the files it has already
    // touched. A record that only counted writes would call this idle and
    // stop; the fingerprint sees the rewrite.
    lane.loop_running();
    wrote(
        &mut lane,
        "a.rs",
        "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
    );
    assert!(matches!(
        lane.loop_step(None, None),
        Some(Round::Again { .. })
    ));

    // Nothing changed: a pass with nothing to do has nothing to do next
    // time either.
    lane.loop_running();
    assert!(matches!(lane.loop_step(None, None), Some(Round::Quiet)));
    assert!(lane.looping().is_none(), "and the loop is gone");
    assert!(
        lane.loop_step(None, None).is_none(),
        "a later turn is not a round"
    );
}

// A line typed between rounds ends a turn too. Counting it would move the
// loop on work it never ran — and end it, if that line wrote nothing.
#[test]
fn a_turn_the_loop_did_not_start_is_not_one_of_its_rounds() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("go".into());

    // Somebody else's turn settling, mid-loop.
    assert!(lane.loop_step(None, None).is_none(), "not the loop's round");
    assert!(lane.looping().is_some(), "and the loop is untouched");
    assert_eq!(lane.looping().map(|l| l.round), Some(0));

    lane.loop_running();
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        lane.loop_step(None, None),
        Some(Round::Again { .. })
    ));
}

// Esc stops the loop and not merely the round it caught: a cut round is
// the loop ending, not a pause before the next one.
#[test]
fn a_cut_round_takes_the_loop_with_it() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("go".into());
    lane.loop_running();
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        lane.loop_step(Some(Cut::Stopped), None),
        Some(Round::Cut(Cut::Stopped))
    ));
    assert!(lane.looping().is_none());
}

// The ceiling is the floor for a loop no other brake can catch: one that
// keeps changing files forever without ever repeating itself.
#[test]
fn a_loop_stops_at_the_configured_ceiling_with_work_still_left() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("go".into());
    lane.loop_running();
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(matches!(
        lane.loop_step(None, Some(1)),
        Some(Round::Capped(1))
    ));
    assert!(lane.looping().is_none());

    // The lane primitive itself has no ceiling at `None` — the config's
    // default is layered above it, at `Config::loop_cap`.
    lane.loop_start("go".into());
    lane.loop_running();
    wrote(&mut lane, "b.rs", "fn b() {}\n");
    assert!(matches!(
        lane.loop_step(None, None),
        Some(Round::Again { .. })
    ));
}

// A round can outlive the loop that queued it — the surface drops those,
// and nothing about them may arm a loop that is over.
#[test]
fn a_loop_that_has_ended_cannot_be_revived_by_a_stale_round() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("go".into());
    lane.loop_running();
    assert!(matches!(lane.loop_step(None, None), Some(Round::Quiet)));

    // What a queued round would do on its way through if the queue ever let
    // one past a stopped loop: neither of these may bring it back.
    lane.loop_running();
    wrote(&mut lane, "a.rs", "fn main() {}\n");
    assert!(lane.looping().is_none(), "no loop to mark as running");
    assert!(lane.loop_step(None, None).is_none(), "and none to step");
}

// A round that restores the tree to a fingerprint it wore earlier is a
// loop seesawing forever — the round after the rewrite undid it.
#[test]
fn a_round_that_undoes_the_last_one_stops_as_oscillating() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("go".into());
    let six = "a\nb\nc\nd\ne\nf\n";
    let six_more = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
    lane.loop_running();
    wrote(&mut lane, "a.rs", six);
    assert!(matches!(
        lane.loop_step(None, None),
        Some(Round::Again { .. })
    ));

    // Round 2 rewrites at full size; round 3 puts the first bytes back —
    // the tree is exactly what round 1 wore, and any later round would
    // seesaw between the two.
    lane.loop_running();
    wrote(&mut lane, "a.rs", six_more);
    assert!(matches!(
        lane.loop_step(None, None),
        Some(Round::Again { .. })
    ));

    lane.loop_running();
    wrote(&mut lane, "a.rs", six);
    assert!(
        matches!(lane.loop_step(None, None), Some(Round::Oscillating)),
        "the tree returned to a fingerprint the loop has already worn"
    );
    assert!(lane.looping().is_none(), "and the loop is gone");
}

// The fingerprint cannot catch a loop that keeps nibbling — one line an
// hour, forever. Two such rounds in a row are the noise floor, and the
// loop stops rather than polish past the point of return.
#[test]
fn rounds_that_only_nibble_stop_as_thin() {
    let (_dir, mut lane) = a_running_lane();
    lane.loop_start("go".into());

    lane.loop_running();
    wrote(&mut lane, "a.rs", "one\n");
    assert!(matches!(
        lane.loop_step(None, None),
        Some(Round::Again { .. })
    ));

    lane.loop_running();
    wrote(&mut lane, "a.rs", "one\ntwo\n");
    assert!(matches!(lane.loop_step(None, None), Some(Round::Thin)));
    assert!(lane.looping().is_none());
}

// ------------------------------------------------------------- looping
// Write `body` to `name` in the lane's workspace and record the write, so
// the tree fingerprint the loop reads has real bytes to hash.
fn wrote(lane: &mut Lane, name: &str, body: &str) {
    let path = lane.root().join(name);
    std::fs::write(&path, body).expect("writes into the temp workspace");
    lane.ctx.note_write(&path);
}
