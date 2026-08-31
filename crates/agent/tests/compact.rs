use agent::compact::{Policy, Report, plan};
use agent::session::{Session, UserBody, UserText};
use brain::estimate;

mod common;
use common::spec;
use brain::message::{AssistantContent, Message, ToolCall, ToolResult, UserContent};
use serde_json::json;

// Drive the real path — plan, record, derive — and hand back the new view.
fn compact(messages: &mut Vec<Message>, budget: usize, policy: &Policy) -> Report {
    let mut log = Session::from_messages(messages.iter().cloned());
    let (record, report) = plan(&log, &spec(), budget, policy);
    log.record(record);
    *messages = log.context();
    report
}

fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
    Message::Assistant {
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            args,
        })],
    }
}

fn result(id: &str, name: &str, body: &str) -> Message {
    Message::tool_results(vec![ToolResult::text(id, name, body)])
}

fn useless(id: &str, name: &str, body: &str) -> Message {
    let mut r = ToolResult::text(id, name, body);
    r.useless = true;
    Message::tool_results(vec![r])
}

fn body_of(m: &Message) -> String {
    match m {
        Message::User { content } => content
            .iter()
            .filter_map(|c| match c {
                UserContent::ToolResult(r) => Some(r.flatten_text()),
                _ => None,
            })
            .collect(),
        _ => String::new(),
    }
}

// Every `tool_use` must still have exactly one answering `tool_result`, or the
// next request is invalid on both wires.
fn assert_balanced(messages: &[Message]) {
    let calls: Vec<_> = messages
        .iter()
        .flat_map(|m| m.tool_calls())
        .map(|c| c.id.clone())
        .collect();
    let results: Vec<_> = messages
        .iter()
        .flat_map(|m| match m {
            Message::User { content } => content
                .iter()
                .filter_map(|c| match c {
                    UserContent::ToolResult(r) => Some(r.call.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect();
    assert_eq!(calls, results, "every call needs its own result, in order");
}

fn big(n: usize) -> String {
    "x".repeat(n)
}

#[test]
fn a_transcript_under_budget_is_left_alone() {
    let mut m = vec![Message::user("hi"), Message::assistant_text("there")];
    let before = m.clone();
    let r = compact(&mut m, 100_000, &Policy::default());
    assert_eq!(m, before);
    assert!(!r.touched());
    assert_eq!(r.before, r.after);
}

#[test]
fn an_older_read_of_the_same_path_is_superseded_and_the_newest_survives() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "read", json!({ "path": "a.rs" })),
        result("c1", "read", &big(9_000)),
        call("c2", "read", json!({ "path": "a.rs" })),
        result("c2", "read", &big(9_000)),
    ];
    // Between the two sizes: superseding one read is enough — a superseded
    // result is dead weight and keeps only its notice — and the drop tier,
    // which would move every index, is never reached.
    let r = compact(&mut m, 4_000, &Policy::default());

    assert_eq!(r.superseded, 1);
    assert_eq!(r.dropped, 0, "{r:?}");
    assert!(
        body_of(&m[2]).contains("superseded by a later read"),
        "{}",
        body_of(&m[2])
    );
    assert!(
        body_of(&m[4]).starts_with("xxx"),
        "the newest read must survive"
    );
    assert_balanced(&m);
}

#[test]
fn a_ranged_read_is_superseded_by_a_later_read_of_the_same_file() {
    let mut m = vec![
        Message::user("go"),
        call(
            "c1",
            "read",
            json!({ "path": "a.rs", "offset": 10, "limit": 5 }),
        ),
        result("c1", "read", &big(9_000)),
        call("c2", "read", json!({ "path": "a.rs" })),
        result("c2", "read", &big(9_000)),
    ];
    // Offset and limit are deliberately outside the key.
    assert_eq!(compact(&mut m, 4_000, &Policy::default()).superseded, 1);
}

#[test]
fn reads_of_different_files_never_supersede_each_other() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "read", json!({ "path": "a.rs" })),
        result("c1", "read", &big(9_000)),
        call("c2", "read", json!({ "path": "b.rs" })),
        result("c2", "read", &big(9_000)),
    ];
    assert_eq!(compact(&mut m, 100_000, &Policy::default()).superseded, 0);
}

// The plan is a singleton: every call answers with the whole list, so an
// earlier answer has since been rewritten.
#[test]
fn an_older_plan_is_superseded_however_the_call_was_written() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "todo", json!({ "op": "set", "items": [] })),
        result("c1", "todo", &big(9_000)),
        call("c2", "todo", json!({ "op": "mark", "at": [1], "status": "done" })),
        result("c2", "todo", &big(9_000)),
    ];
    let r = compact(&mut m, 4_000, &Policy::default());

    assert_eq!(r.superseded, 1);
    assert!(
        body_of(&m[2]).contains("superseded by a later todo"),
        "{}",
        body_of(&m[2])
    );
    assert!(
        body_of(&m[4]).starts_with("xxx"),
        "the newest plan must survive"
    );
    assert_balanced(&m);
}

#[test]
fn an_edit_result_is_never_superseded_by_a_later_edit() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "edit", json!({ "patch": "one" })),
        result("c1", "edit", &big(9_000)),
        call("c2", "edit", json!({ "patch": "two" })),
        result("c2", "edit", &big(9_000)),
    ];
    // An edit records something that happened; later edits do not unmake it.
    assert_eq!(compact(&mut m, 4_000, &Policy::default()).superseded, 0);
}

#[test]
fn results_their_tool_called_uneventful_go_first() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "grep", json!({ "pattern": "zzz" })),
        useless("c1", "grep", &big(3_000)),
        call("c2", "read", json!({ "path": "a.rs" })),
        result("c2", "read", &big(300)),
    ];
    // Omitting the one uneventful result is enough on its own.
    let r = compact(&mut m, 500, &Policy::default());
    assert_eq!(r.uneventful, 1);
    assert_eq!(r.dropped, 0, "{r:?}");
    assert!(
        body_of(&m[2]).contains("reported nothing"),
        "{}",
        body_of(&m[2])
    );
}

#[test]
fn the_working_tail_survives_while_older_results_age_out() {
    let mut m = vec![Message::user("go")];
    for i in 0..8 {
        m.push(call(
            &format!("c{i}"),
            "bash",
            json!({ "command": format!("cmd{i}") }),
        ));
        m.push(result(&format!("c{i}"), "bash", &big(30_000)));
    }
    let before = estimate::tokens(&m, &spec());
    let r = compact(
        &mut m,
        before / 2,
        &Policy::default(),
    );

    assert!(r.aged_out > 0, "{r:?}");
    // The last exchange is what the agent is working from.
    assert!(
        body_of(m.last().unwrap()).starts_with("xxx"),
        "the newest result must survive"
    );
    assert!(r.after < r.before);
    assert_balanced(&m);
}

#[test]
fn an_aged_out_result_keeps_its_head_and_tail() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "bash", json!({ "command": "cmd" })),
        // The shape of a test run: a distinctive head, a long boring middle,
        // and the failure summary at the tail.
        result(
            "c1",
            "bash",
            &format!("HEAD-BEGIN\n{}\nTAIL-END", big(30_000)),
        ),
    ];
    let r = compact(
        &mut m,
        2_000,
        &Policy { protect_tail: 0, ..Policy::default() },
    );

    assert_eq!(r.aged_out, 1, "{r:?}");
    let body = body_of(&m[2]);
    assert!(body.starts_with("[omitted"), "{}", body);
    assert!(body.contains("HEAD-BEGIN"), "{body}");
    assert!(body.contains("TAIL-END"), "{body}");
    assert!(body.contains("chars omitted"), "{body}");
    assert_balanced(&m);
}

#[test]
fn a_result_under_the_prune_threshold_keeps_only_the_notice() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "bash", json!({ "command": "cmd" })),
        result("c1", "bash", &big(200)),
    ];
    let r = compact(
        &mut m,
        100,
        &Policy { protect_tail: 0, ..Policy::default() },
    );

    assert_eq!(r.aged_out, 1, "{r:?}");
    let body = body_of(&m[2]);
    assert_eq!(body, "[omitted to fit the context window]", "{body}");
    assert_balanced(&m);
}

#[test]
fn a_head_and_tail_too_big_for_the_window_lowers_to_the_notice() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "bash", json!({ "command": "cmd" })),
        result("c1", "bash", &big(30_000)),
    ];
    // Budget below what even a pruned result costs: the last rung drops the
    // kept ends, leaving the notice alone.
    let r = compact(
        &mut m,
        500,
        &Policy { protect_tail: 0, ..Policy::default() },
    );

    assert_eq!(r.aged_out, 1, "{r:?}");
    assert_eq!(
        body_of(&m[2]),
        "[omitted to fit the context window]",
        "{}",
        body_of(&m[2])
    );
    assert_balanced(&m);
}

#[test]
fn the_drop_tier_spares_a_skill_exchange() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "skill", json!({ "name": "commit" })),
        result("c1", "skill", &big(20_000)),
        call("c2", "bash", json!({ "command": "cmd" })),
        result("c2", "bash", &big(20_000)),
    ];
    let r = compact(
        &mut m,
        200,
        &Policy { protect_tail: 0, ..Policy::default() },
    );

    // The skill body is instructions being followed: omission refuses it, and
    // so does the drop tier, which takes the bash exchange instead.
    assert!(r.dropped > 0, "{r:?}");
    assert!(
        body_of(&m[2]).starts_with("xxx"),
        "the skill body must survive: {}",
        body_of(&m[2])
    );
    assert_balanced(&m);
}

#[test]
fn dropping_history_keeps_the_task_and_stays_balanced() {
    // The weight sits in assistant prose, which no amount of result omission
    // reclaims — dropping whole exchanges is the only measure left.
    let mut m = vec![Message::user("the original task")];
    for i in 0..10 {
        m.push(Message::assistant_text(big(40_000)));
        m.push(Message::user(format!("next {i}")));
    }
    let r = compact(&mut m, 20_000, &Policy { protect_tail: 0, ..Policy::default() });

    assert!(r.dropped > 0, "{r:?}");
    assert_eq!(
        m[0].text(),
        "the original task",
        "the task itself never goes"
    );
    assert_balanced(&m);
    assert!(r.after <= 20_000 || r.still_over, "{r:?}");
}

#[test]
fn a_dropped_exchange_never_orphans_the_result_that_answered_it() {
    // The shape that broke it: a tool result followed straight away by a
    // prompt. `context()` merged those two user messages into one, so the
    // message list was shorter than the id list `plan` walked beside it — and
    // the same index reached a different turn in each. What went into
    // `dropped` was not what came out of the view, leaving a `tool_result`
    // whose `tool_use` was gone and a request both formats refuse.
    //
    // One list now, `Seen` carrying its own id, so there is no second index to
    // disagree with.
    // The weight sits in assistant prose, so result omission cannot reclaim it
    // and the drop tier is what has to run.
    let mut m = vec![Message::user("the original task")];
    for i in 0..8 {
        m.push(Message::Assistant {
            content: vec![
                AssistantContent::Text(brain::message::Text { text: big(20_000) }),
                AssistantContent::ToolCall(ToolCall {
                    id: format!("c{i}"),
                    name: "read".into(),
                    args: json!({ "path": format!("f{i}.rs") }),
                }),
            ],
        });
        m.push(result(&format!("c{i}"), "read", "ok"));
        m.push(Message::user(format!("and now {i}")));
    }
    let r = compact(
        &mut m,
        8_000,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );

    assert!(r.dropped > 0, "the drop tier has to run for this to mean anything: {r:?}");
    assert_balanced(&m);
    assert_eq!(
        m[0].text().split("and now").next().unwrap().trim(),
        "the original task",
        "the task itself never goes"
    );
}

// The one weight on the assistant side compaction may take. What went is the
// bulk; what a later turn still needs — which file, which call — stays.
#[test]
fn an_oversized_argument_goes_while_the_path_beside_it_stays() {
    let mut s = Session::new();
    s.prompt("write them");
    for i in 0..6 {
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: format!("c{i}"),
            name: "write".into(),
            args: json!({ "path": format!("f{i}.rs"), "content": big(30_000) }),
        })]);
        s.push_results(vec![ToolResult::text(format!("c{i}"), "write", "wrote 400 lines")]);
    }

    // `protect_tail` off: what is under test is the arguments tier, and with
    // the default tail the remainder stays over budget and the drop tier —
    // correctly — takes the exchanges instead.
    let policy = Policy { protect_tail: 0, ..Policy::default() };
    let budget = estimate::tokens(&s.context(), &spec()) / 4;
    let (record, report) = plan(&s, &spec(), budget, &policy);
    s.record(record);

    assert!(report.args_taken > 0, "{report:?}");
    assert_eq!(report.dropped, 0, "the arguments alone were enough: {report:?}");
    let view = s.context();
    assert_balanced(&view);

    let calls: Vec<&ToolCall> = view.iter().flat_map(|m| m.tool_calls()).collect();
    assert_eq!(calls.len(), 6, "every call keeps its block");
    let taken: Vec<&&ToolCall> = calls
        .iter()
        .filter(|c| c.args["content"].as_str().is_some_and(|t| t.starts_with("[omitted")))
        .collect();
    assert!(!taken.is_empty(), "nothing was taken: {calls:?}");
    for c in &taken {
        assert!(
            c.args["path"].as_str().is_some_and(|p| p.ends_with(".rs")),
            "the path went with the content: {:?}",
            c.args
        );
    }
    // The turn itself is still shown, so the screen must not mark it gone.
    let ids: Vec<_> = s.history().map(|e| e.id()).collect();
    assert!(
        ids.iter().any(|id| !s.out_of_view().contains(id)),
        "a taken argument is not the entry leaving the view"
    );
}

// An argument already taken must not be taken again — the second pass would
// find the notice, not the bulk, and record an omission that reclaims nothing.
#[test]
fn arguments_already_taken_are_not_taken_twice() {
    let mut s = Session::new();
    s.prompt("write them");
    for i in 0..6 {
        s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
            id: format!("c{i}"),
            name: "write".into(),
            args: json!({ "path": format!("f{i}.rs"), "content": big(30_000) }),
        })]);
        s.push_results(vec![ToolResult::text(format!("c{i}"), "write", "ok")]);
    }
    let budget = estimate::tokens(&s.context(), &spec()) / 4;

    let (first, r1) = plan(&s, &spec(), budget, &Policy::default());
    s.record(first);
    assert!(r1.args_taken > 0, "{r1:?}");

    let (_second, r2) = plan(&s, &spec(), budget, &Policy::default());
    assert_eq!(r2.args_taken, 0, "the same arguments were taken twice: {r2:?}");
}

#[test]
fn compaction_converges_instead_of_shrinking_forever() {
    let mut m = vec![Message::user("go")];
    for i in 0..4 {
        m.push(call(
            &format!("c{i}"),
            "read",
            json!({ "path": format!("f{i}.rs") }),
        ));
        m.push(result(&format!("c{i}"), "read", &big(20_000)));
    }
    let first = compact(&mut m, 2_000, &Policy { protect_tail: 0, ..Policy::default() });
    let snapshot = m.clone();
    let second = compact(&mut m, 2_000, &Policy { protect_tail: 0, ..Policy::default() });

    // A second pass over an already-compacted transcript must find nothing.
    assert_eq!(m, snapshot, "{second:?}");
    assert!(!second.touched(), "{second:?}");
    assert!(first.after < first.before);
}

#[test]
fn a_transcript_that_cannot_fit_says_so_rather_than_pretending() {
    let mut m = vec![Message::user(big(60_000))];
    let r = compact(&mut m, 100, &Policy::default());
    // One message, and it is the task: nothing left to give.
    assert!(r.still_over);
    assert_eq!(m.len(), 1);
}

#[test]
fn an_entry_elided_by_an_earlier_pass_is_not_elided_again() {
    // Under the record model an omission lives in the compaction entry, not in
    // the stored result — so "already omitted" is a fact about the session, not
    // a prefix to sniff for in the body.
    let messages = vec![
        Message::user("go"),
        call("c1", "read", json!({ "path": "a.rs" })),
        result("c1", "read", &big(9_000)),
    ];
    let mut log = Session::from_messages(messages.clone());
    let policy = Policy {
        protect_tail: 0,
        ..Policy::default()
    };

    let (first, r1) = plan(&log, &spec(), 1_000, &policy);
    assert_eq!(r1.aged_out, 1, "{r1:?}");
    let omitted = first.omissions.len();
    log.record(first);

    let (second, r2) = plan(&log, &spec(), 1_000, &policy);
    assert_eq!(omitted, 1);
    assert!(
        second.omissions.is_empty(),
        "a second pass must not restate the first one's omissions: {second:?}"
    );
    assert_eq!(r2.aged_out, 0, "nothing left to reclaim: {r2:?}");
}

mod budget {
    use super::big;
    use super::spec;
    use agent::session::{Session, UserBody};
#[allow(unused_imports)]
use brain::message::Text as _Text;
    use brain::message::{AssistantContent, ToolCall, ToolResult};
    use agent::Agent;
    use async_trait::async_trait;
    use brain::model::ModelSpec;
    use brain::request::Request;
    use brain::stream::StreamEvent;
    use brain::transport::Transport;
    use futures::stream::BoxStream;
    use serde_json::json;
    use std::sync::Arc;

    struct Never;

    #[async_trait]
    impl Transport for Never {
        async fn stream(
            &self,
            _: &ModelSpec,
            _: &Request,
        ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
            unreachable!("budget needs no network")
        }
    }

    fn agent_with(context: u32, max_output: u32) -> Agent {
        let spec = brain::model::ModelSpec {
            context_window: context,
            max_output_tokens: max_output,
            ..spec()
        };
        Agent::new(Arc::new(Never), spec)
    }

    #[test]
    fn an_output_cap_larger_than_the_window_never_starves_the_transcript() {
        // 20k window against a spec declaring 64k of output: reserving it
        // verbatim would leave the transcript zero and compact every turn.
        let a = agent_with(20_000, 64_000);
        assert!(a.budget() >= 5_000, "{}", a.budget());
    }

    #[test]
    fn a_normal_window_leaves_most_of_itself_to_the_transcript() {
        let a = agent_with(200_000, 32_000);
        let b = a.budget();
        assert!(b > 120_000 && b < 200_000, "{b}");
    }

    #[tokio::test]
    async fn a_manual_compaction_runs_even_though_the_transcript_fits() {
        // The whole point of asking for it: the user knows a phase ended, and
        // no budget can tell. This transcript is far under the window.
        let mut a = agent_with(1_000_000, 32_000);
        a.summarize = false; // Never has no network to summarize with.
        let mut s = Session::with_prompt("go");
        // Distinct paths, so the supersede tier has nothing to take and the
        // tail protection is what has to stop the descent.
        for i in 0..14 {
            let path = format!("f{i}.rs");
            s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
                id: format!("c{i}"),
                name: "read".into(),
                args: json!({ "path": path }),
            })]);
            s.push_user(UserBody::Result {
                result: ToolResult::text(format!("c{i}"), "read", big(4_000)),
                preview: None,
            });
        }
        let before = brain::estimate::tokens(&s.context(), &spec());
        assert!(before < a.budget(), "the automatic pass would decline this");

        let (report, _) = a.compact_now(&mut s, None).await.expect("something to do");
        assert!(report.touched());
        let after = brain::estimate::tokens(&s.context(), &spec());
        assert!(after < before, "{before} -> {after}");
        // It stops at the tail the agent is working from rather than at zero.
        assert!(
            after >= a.kept_tokens().unwrap() / 2,
            "took the tail too: {after}"
        );
    }

    #[tokio::test]
    async fn a_short_transcript_has_nothing_to_compact() {
        let a = agent_with(200_000, 32_000);
        let mut s = Session::with_prompt("hello");
        assert!(a.compact_now(&mut s, None).await.is_none());
    }

    #[test]
    fn a_healthy_transcript_is_under_budget_and_stays_untouched() {
        let a = agent_with(200_000, 32_000);
        let s = Session::with_prompt("hello");
        let (record, r) = agent::compact::plan(&s, &a.spec, a.budget(), &agent::Policy::default());
        assert!(!r.touched());
        assert_eq!(
            record,
            agent::session::Compaction {
                tokens_before: r.before,
                tokens_after: r.after,
                ..Default::default()
            }
        );
    }
}

#[test]
fn a_skill_body_survives_a_compaction_that_takes_everything_else() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "skill", json!({ "name": "commit" })),
        result("c1", "skill", &big(9_000)),
        call("c2", "read", json!({ "path": "a.rs" })),
        result("c2", "read", &big(9_000)),
        call("c3", "read", json!({ "path": "a.rs" })),
        result("c3", "read", &big(9_000)),
    ];
    let r = compact(&mut m, 4_000, &Policy { protect_tail: 0, ..Policy::default() });

    // Instructions the agent is in the middle of following are not spare
    // context, whatever the budget says.
    assert!(
        body_of(&m[2]).starts_with("xxx"),
        "the skill body must survive: {}",
        body_of(&m[2])
    );
    assert!(
        r.superseded + r.aged_out > 0,
        "everything else was still reclaimed: {r:?}"
    );
}

// The planner counts `MESSAGE_OVERHEAD` per entry and the sender counts it per
// message. Those were two different numbers while the view merged a turn's
// user entries: six parallel results cost the planner six framings and the
// sender one, so compaction planned against a budget the request never spent.
// One entry, one message closes it — and this is what keeps it closed.
#[test]
fn the_planner_and_the_sender_count_the_same_transcript() {
    let mut s = Session::new();
    s.prompt("go");
    for turn in 0..3 {
        let calls: Vec<AssistantContent> = (0..6)
            .map(|i| {
                AssistantContent::ToolCall(ToolCall {
                    id: format!("t{turn}c{i}"),
                    name: "read".into(),
                    args: json!({ "path": format!("f{i}.rs") }),
                })
            })
            .collect();
        s.push_assistant(calls);
        s.push_results(
            (0..6)
                .map(|i| ToolResult::text(format!("t{turn}c{i}"), "read", big(30)))
                .collect(),
        );
    }
    s.prompt("and now this");

    let (_record, report) = plan(&s, &spec(), usize::MAX, &Policy::default());
    assert_eq!(
        report.before,
        estimate::tokens(&s.context(), &spec()),
        "the two estimates have drifted apart again"
    );
}

// A `!` command's output is not a question. It used to be stored as the same
// `Text` a prompt is, so the drop tier read it as one — and took it as a round
// of its own, out from under the `fix that` that referred to it.
#[test]
fn a_bang_command_goes_with_the_question_that_refers_to_it() {
    let mut s = Session::new();
    s.prompt("the task");
    s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
        text: big(9_000),
    })]);
    let ran = s.push_user(UserBody::Aside(UserText {
        text: "Ran `cargo test`\nFAILED at auth.rs:14".into(),
        shown: Some("!cargo test".into()),
    }));
    s.prompt("fix that");
    s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
        text: "on it".into(),
    })]);

    // The floor: everything droppable goes.
    let (record, r) = plan(&s, &spec(), 0, &Policy { protect_tail: 0, ..Policy::default() });
    s.record(record);
    assert!(r.dropped > 0 || r.aged_out > 0, "{r:?}");

    let joined: String = s
        .context()
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>()
        .join("\n");
    // Present, not merely accounted for: a dropped entry leaves nothing, where
    // an omitted one leaves a notice the model can still read.
    let ran_present = joined.contains("auth.rs:14") || joined.contains("[omitted");
    assert!(
        !joined.contains("fix that") || ran_present,
        "`fix that` outlived what it points at:\n{joined}"
    );
    let _ = ran;
}

// And it is the one piece of user-side text that *may* be shrunk: nothing is
// waiting on an answer to it, and a `!cargo test` can be tens of kilobytes.
#[test]
fn a_bang_command_can_be_shrunk_where_a_question_cannot() {
    let mut s = Session::new();
    s.prompt("the task");
    s.push_user(UserBody::Aside(UserText {
        text: big(30_000),
        shown: Some("!cargo test".into()),
    }));
    s.prompt("fix that");
    s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
        text: "on it".into(),
    })]);

    let budget = estimate::tokens(&s.context(), &spec()) / 2;
    let (record, r) = plan(&s, &spec(), budget, &Policy { protect_tail: 0, ..Policy::default() });
    s.record(record);
    assert!(r.aged_out > 0, "the aside was never shrunk: {r:?}");

    let joined: String = s.context().iter().map(Message::text).collect();
    assert!(joined.contains("the task"), "the question stays: {joined:.200}");
    assert!(joined.contains("fix that"), "and so does this one");
}

// What the round-sized unit is for. Dropping an assistant turn and its
// results left the prompt that asked for them standing with nothing after it
// — legal on both wires, and pure waste: a question already answered, whose
// answer is gone, paid for on every turn from here on.
//
// One prompt may stand unanswered, and only one: the opening task, which is
// kept on purpose.
#[test]
fn dropping_leaves_no_question_without_its_answer() {
    let mut s = Session::new();
    s.prompt("the original task");
    for i in 0..7 {
        s.push_assistant(vec![
            AssistantContent::Text(brain::message::Text { text: big(20_000) }),
            AssistantContent::ToolCall(ToolCall {
                id: format!("c{i}"),
                name: "read".into(),
                args: json!({ "path": format!("f{i}.rs") }),
            }),
        ]);
        s.push_results(vec![ToolResult::text(format!("c{i}"), "read", "contents")]);
        s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
            text: big(20_000),
        })]);
        s.prompt(format!("question {i}"));
    }
    s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
        text: "last".into(),
    })]);

    let policy = Policy { protect_tail: 0, ..Policy::default() };
    let budget = estimate::tokens(&s.context(), &spec()) / 6;
    let (record, report) = plan(&s, &spec(), budget, &policy);
    s.record(record);
    assert!(report.dropped > 0, "nothing was dropped, so nothing is proven: {report:?}");

    let view = s.context();
    assert_balanced(&view);
    let typed = |m: &Message| {
        matches!(m, Message::User { content }
            if content.iter().any(|c| matches!(c, UserContent::Text(_))))
    };
    for i in 1..view.len().saturating_sub(1) {
        assert!(
            !(typed(&view[i]) && typed(&view[i + 1])),
            "`{}` was left with no answer after it:\n{}",
            view[i].text(),
            view.iter()
                .map(|m| format!("  {:.60}", m.text()))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    assert_eq!(view[0].text(), "the original task", "the task itself stays");
}

// The drop tier's unit is an assistant turn and the results answering it. A
// turn that called no tool has no answers, so it must go alone — sweeping
// forward to the next assistant takes the user's next question with it, and
// that question is not stale context, it is what they asked.
#[test]
fn dropping_a_chat_only_turn_does_not_take_the_next_question_with_it() {
    let mut s = Session::new();
    s.prompt("the task");
    for i in 0..8 {
        s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
            text: big(40),
        })]);
        s.prompt(format!("question {i}"));
    }
    s.push_assistant(vec![AssistantContent::Text(brain::message::Text {
        text: "last".into(),
    })]);

    let budget = estimate::tokens(&s.context(), &spec()) / 3;
    let (record, _report) = plan(&s, &spec(), budget, &Policy::default());
    s.record(record);

    let left: String = s
        .context()
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n");
    for i in 0..8 {
        let q = format!("question {i}");
        let a = left.contains(&q);
        assert!(a || !left.contains(&format!("question {}", i + 1)),
            "`{q}` was dropped while a later question survived — a question is \
             not the answer's spare context");
    }
}
