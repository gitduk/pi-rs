use agent::compaction::ladder::plan;
use agent::session::{Compaction, Entry, Omission, Prompt, Seen, Session};
use agent::{Policy, Report};
use llm::estimate;

mod common;
use common::spec;
use llm::message::{AssistantContent, Message, ToolCall, ToolResult, UserContent};
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

// The same call again — same tool, name, arguments — supersedes the older
// answer; a different range, file, or edit is a different call.
#[test]
fn only_the_same_call_made_again_supersedes() {
    let edit =
        |old: &str| json!({ "path": "a.rs", "edits": [{ "old_string": old, "new_string": "x" }] });
    let rows: &[(&str, Vec<Message>, usize)] = &[
        (
            "a later read of the same file",
            vec![
                Message::user("go"),
                call("c1", "read", json!({ "path": "a.rs" })),
                result("c1", "read", &big(9_000)),
                call("c2", "read", json!({ "path": "a.rs" })),
                result("c2", "read", &big(9_000)),
            ],
            1,
        ),
        (
            "the same command run again",
            vec![
                Message::user("go"),
                call("c1", "bash", json!({ "command": "cargo test" })),
                result("c1", "bash", &big(9_000)),
                call("c2", "bash", json!({ "command": "cargo test" })),
                result("c2", "bash", &big(9_000)),
            ],
            1,
        ),
        (
            "a ranged read, then the whole file",
            vec![
                Message::user("go"),
                call(
                    "c1",
                    "read",
                    json!({ "path": "a.rs", "offset": 10, "limit": 5 }),
                ),
                result("c1", "read", &big(9_000)),
                call("c2", "read", json!({ "path": "a.rs" })),
                result("c2", "read", &big(9_000)),
            ],
            0,
        ),
        (
            "reads of different files",
            vec![
                Message::user("go"),
                call("c1", "read", json!({ "path": "a.rs" })),
                result("c1", "read", &big(9_000)),
                call("c2", "read", json!({ "path": "b.rs" })),
                result("c2", "read", &big(9_000)),
            ],
            0,
        ),
        (
            "two different edits of the same file",
            vec![
                Message::user("go"),
                call("c1", "edit", edit("one")),
                result("c1", "edit", &big(9_000)),
                call("c2", "edit", edit("two")),
                result("c2", "edit", &big(9_000)),
            ],
            0,
        ),
    ];
    for (what, m, want) in rows {
        let mut m = m.clone();
        let r = compact(&mut m, 4_000, &Policy::default());
        assert_eq!(r.superseded, *want, "{what}: {r:?}");
        if *want == 1 {
            // The notice replaces the dead weight and the newest answer survives.
            assert_eq!(r.dropped, 0, "{what}: {r:?}");
            assert!(
                body_of(&m[2]).contains("the same call ran again later"),
                "{what}: {}",
                body_of(&m[2])
            );
            assert!(
                body_of(&m[4]).starts_with("xxx"),
                "{what}: the newest answer must survive: {}",
                body_of(&m[4])
            );
        }
        assert_balanced(&m);
    }
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
    let r = compact(&mut m, before / 2, &Policy::default());

    assert!(r.aged_out > 0, "{r:?}");
    // The last exchange is what the agent is working from.
    assert!(
        body_of(m.last().unwrap()).starts_with("xxx"),
        "the newest result must survive"
    );
    assert!(r.after < r.before);
    assert_balanced(&m);
}

// Keeps a distinctive head and tail and omits the middle; too small to
// prune, or ends too big to fit, lowers to the notice alone.
#[test]
fn an_aged_out_result_keeps_its_ends_or_lowers_to_the_notice() {
    let rows: &[(&str, String, usize, bool)] = &[
        (
            "a distinctive head and tail survive",
            format!("HEAD-BEGIN\n{}\nTAIL-END", big(30_000)),
            2_000,
            true,
        ),
        ("a result under the prune threshold", big(200), 100, false),
        ("ends too big for the window", big(30_000), 500, false),
    ];
    for (what, body, budget, keeps_ends) in rows {
        let mut m = vec![
            Message::user("go"),
            call("c1", "bash", json!({ "command": "cmd" })),
            result("c1", "bash", body),
        ];
        let r = compact(
            &mut m,
            *budget,
            &Policy {
                protect_tail: 0,
                ..Policy::default()
            },
        );
        assert_eq!(r.aged_out, 1, "{what}: {r:?}");
        let notice = body_of(&m[2]);
        if *keeps_ends {
            assert!(notice.starts_with("[omitted"), "{what}: {notice}");
            assert!(notice.contains("HEAD-BEGIN"), "{what}: {notice}");
            assert!(notice.contains("TAIL-END"), "{what}: {notice}");
            assert!(notice.contains("chars omitted"), "{what}: {notice}");
        } else {
            assert_eq!(
                notice, "[omitted to fit the context window]",
                "{what}: {notice}"
            );
        }
        assert_balanced(&m);
    }
}

// A skill body is compacted like any other result: nothing is pinned, and the
// model calls the skill again when it needs the instructions back.
#[test]
fn a_skill_result_is_compacted_like_any_other() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "skill", json!({ "name": "commit" })),
        result("c1", "skill", &big(20_000)),
        call("c2", "bash", json!({ "command": "cmd" })),
        result("c2", "bash", &big(20_000)),
    ];
    let r = compact(
        &mut m,
        4_000,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );
    assert!(r.aged_out > 0, "{r:?}");
    assert!(
        body_of(&m[2]).starts_with("[omitted"),
        "the oldest result goes first, skill or not: {}",
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
    let r = compact(
        &mut m,
        20_000,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );

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
    // The weight sits in assistant prose, so result omission cannot reclaim it
    // and the drop tier is what has to run.
    let mut m = vec![Message::user("the original task")];
    for i in 0..8 {
        m.push(Message::Assistant {
            content: vec![
                AssistantContent::Text(llm::message::Text { text: big(20_000) }),
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

    assert!(
        r.dropped > 0,
        "the drop tier has to run for this to mean anything: {r:?}"
    );
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
        s.push_results(vec![ToolResult::text(
            format!("c{i}"),
            "write",
            "wrote 400 lines",
        )]);
    }

    // `protect_tail` off: with the default tail, the remainder stays over
    // budget and the drop tier takes the exchanges instead of the args tier.
    let policy = Policy {
        protect_tail: 0,
        ..Policy::default()
    };
    let budget = estimate::tokens(&s.context(), &spec()) / 4;
    let (record, report) = plan(&s, &spec(), budget, &policy);
    s.record(record);

    assert!(report.args_taken > 0, "{report:?}");
    assert_eq!(
        report.dropped, 0,
        "the arguments alone were enough: {report:?}"
    );
    let view = s.context();
    assert_balanced(&view);

    let calls: Vec<&ToolCall> = view.iter().flat_map(|m| m.tool_calls()).collect();
    assert_eq!(calls.len(), 6, "every call keeps its block");
    let taken: Vec<&&ToolCall> = calls
        .iter()
        .filter(|c| {
            c.args["content"]
                .as_str()
                .is_some_and(|t| t.starts_with("[omitted"))
        })
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
    assert_eq!(
        r2.args_taken, 0,
        "the same arguments were taken twice: {r2:?}"
    );
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
    let first = compact(
        &mut m,
        2_000,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );
    let snapshot = m.clone();
    let second = compact(
        &mut m,
        2_000,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );

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
    // An omission lives in the compaction entry, not the stored result, so
    // "already omitted" is a fact about the session, not a prefix in the body.
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
    use agent::Agent;
    use agent::session::Session;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use llm::message::{AssistantContent, ToolCall, ToolResult};
    use llm::model::ModelSpec;
    use llm::request::Request;
    use llm::stream::StreamEvent;
    use llm::transport::Transport;
    use serde_json::json;
    use std::sync::Arc;

    // Fourteen distinct reads: enough weight that a compaction has something
    // to drop, and distinct paths so the supersede tier cannot take it first.
    fn bulky_session() -> Session {
        let mut s = Session::with_prompt("go");
        for i in 0..14 {
            s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
                id: format!("c{i}"),
                name: "read".into(),
                args: json!({ "path": format!("f{i}.rs") }),
            })]);
            s.push_previewed(vec![(
                ToolResult::text(format!("c{i}"), "read", big(4_000)),
                None,
            )]);
        }
        s
    }

    struct Never;

    // A summary that never comes back. The manual pass always summarizes
    // what it drops, so this is the one test that reaches a network at all.
    struct Empty;

    #[async_trait]
    impl Transport for Empty {
        async fn stream(
            &self,
            _: &ModelSpec,
            _: &Request,
        ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[async_trait]
    impl Transport for Never {
        async fn stream(
            &self,
            _: &ModelSpec,
            _: &Request,
        ) -> llm::Result<BoxStream<'static, llm::Result<StreamEvent>>> {
            unreachable!("budget needs no network")
        }
    }

    fn agent_with(context: u32, max_output: u32) -> Agent {
        let spec = llm::model::ModelSpec {
            context_window: context,
            max_output_tokens: max_output,
            ..spec()
        };
        Agent::new(Arc::new(Never), spec)
    }

    // Reserved output room is capped: uncapped, a reservation bigger than the
    // window would leave the transcript nothing.
    #[test]
    fn the_budget_never_starves_the_transcript() {
        // 20k window against a spec declaring 64k of output: reserving it
        // verbatim would leave the transcript zero and compact every turn.
        let a = agent_with(20_000, 64_000);
        assert!(a.budget() >= 5_000, "{}", a.budget());

        let a = agent_with(200_000, 32_000);
        let b = a.budget();
        assert!(b > 120_000 && b < 200_000, "{b}");
    }
    #[tokio::test]
    async fn a_manual_compaction_runs_even_though_the_transcript_fits() {
        // The whole point of asking for it: the user knows a phase ended, and
        // no budget can tell. This transcript is far under the window.
        let mut a = agent_with(1_000_000, 32_000);
        crate::common::compacting(&mut a, Some((Arc::new(Empty), spec())));
        let mut s = bulky_session();
        let before = llm::estimate::tokens(&s.context(), &spec());
        assert!(before < a.budget(), "the automatic pass would decline this");

        let (report, _) = a.compact_now(&mut s, None).await.expect("something to do");
        assert!(report.touched());
        let after = llm::estimate::tokens(&s.context(), &spec());
        assert!(after < before, "{before} -> {after}");
        // It stops at the tail the agent is working from rather than at zero.
        assert!(after >= a.kept_tokens() / 2, "took the tail too: {after}");
    }

    // A flat 16k tail against a 9k budget protects more than the budget holds,
    // so every tier that reaches only what precedes the tail reaches nothing.
    #[test]
    fn a_small_window_keeps_room_to_compact_into() {
        let a = agent_with(20_000, 64_000);
        let s = bulky_session();
        let budget = a.budget();
        assert!(
            llm::estimate::tokens(&s.context(), a.spec()) > budget,
            "the transcript has to start over budget for this to mean anything"
        );

        let policy = agent::Policy {
            protect_tail: a.kept_tokens(),
            ..agent::Policy::default()
        };
        let (_record, report) = agent::compaction::ladder::plan(&s, a.spec(), budget, &policy);
        assert!(
            !report.still_over,
            "the tail left nothing to reclaim: {report:?}"
        );
    }

    // Nothing over budget, nothing to do: `plan` records a compaction that
    // changed nothing, and `compact_now` declines outright.
    #[tokio::test]
    async fn a_healthy_transcript_has_nothing_to_compact() {
        let a = agent_with(200_000, 32_000);
        let s = Session::with_prompt("hello");
        let (record, r) =
            agent::compaction::ladder::plan(&s, a.spec(), a.budget(), &agent::Policy::default());
        assert!(!r.touched());
        assert_eq!(
            record,
            agent::session::Compaction {
                tokens_before: r.before,
                tokens_after: r.after,
                ..Default::default()
            }
        );

        let mut s = Session::with_prompt("hello");
        assert!(a.compact_now(&mut s, None).await.is_none());
    }
}

// Six parallel results cost the planner six framings but the sender one, so
// compaction must plan against the budget the request will actually spend.
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

// Summaries ride the first user message, so the planner must count them too
// — undercounting fires compaction late, after the provider already refused.
#[test]
fn the_planner_and_the_sender_count_the_summaries_the_same() {
    let mut s = Session::new();
    s.prompt("go");
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: big(600),
    })]);
    s.prompt("and now this");
    s.record(Compaction {
        summary: Some("earlier: the first question, and what came of it".to_string()),
        ..Compaction::default()
    });

    let (_record, report) = plan(&s, &spec(), usize::MAX, &Policy::default());
    assert_eq!(
        report.before,
        estimate::tokens(&s.context(), &spec()),
        "the two estimates have drifted apart again"
    );
}

// An assistant turn is never omitted: its `tool_use` blocks must stay legal,
// so the sender carries the whole turn however the record reads it.
#[test]
fn an_omission_naming_an_assistant_turn_is_read_the_same_by_both() {
    let mut s = Session::new();
    s.prompt("go");
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: big(600),
    })]);
    s.prompt("and now this");

    let id = s
        .view()
        .iter()
        .find_map(|seen| match seen {
            Seen::As(Entry::Answer { .. }) => Some(seen.id()),
            _ => None,
        })
        .expect("the answer is in the view");
    s.record(Compaction {
        omissions: vec![Omission {
            entry: id,
            block: None,
            notice: "[omitted: the turn was rolled up]".into(),
        }],
        ..Compaction::default()
    });

    assert!(
        matches!(
            s.view().iter().find(|seen| seen.id() == id),
            Some(Seen::As(_))
        ),
        "the turn is sent whole, so it is not the view that omits it"
    );
    let (_record, report) = plan(&s, &spec(), usize::MAX, &Policy::default());
    assert_eq!(
        report.before,
        estimate::tokens(&s.context(), &spec()),
        "the two estimates have drifted apart again"
    );
}

// A result standing in as a notice is still a `tool_result` on the wire, so
// the planner must price the block it's sent in, not the notice alone.
#[test]
fn the_planner_and_the_sender_count_an_omitted_result_the_same() {
    let mut s = Session::new();
    s.prompt("go");
    s.push_assistant(vec![AssistantContent::ToolCall(ToolCall {
        id: "c1".into(),
        name: "grep".into(),
        args: json!({ "pattern": "x" }),
    })]);
    s.push_results(vec![ToolResult::text("c1", "grep", big(9_000))]);
    s.prompt("and now this");

    // The result as a notice, the way a pass that only had room for content —
    // and not for whole rounds — leaves it.
    let id = s
        .view()
        .iter()
        .find_map(|seen| match seen {
            Seen::As(Entry::Tool { .. }) => Some(seen.id()),
            _ => None,
        })
        .expect("the result is in the view");
    s.record(Compaction {
        omissions: vec![Omission {
            entry: id,
            block: None,
            notice: "[omitted: the body was rolled up]".into(),
        }],
        ..Compaction::default()
    });
    assert!(matches!(
        s.view().iter().find(|seen| seen.id() == id),
        Some(Seen::Omitted { .. })
    ));

    let (_record, report) = plan(&s, &spec(), usize::MAX, &Policy::default());
    assert_eq!(
        report.before,
        estimate::tokens(&s.context(), &spec()),
        "the two estimates have drifted apart again"
    );
}

// A `!` command's output is not a question, so it must not be dropped as a
// round of its own, out from under the question that refers to it.
#[test]
fn a_bang_command_goes_with_the_question_that_refers_to_it() {
    let mut s = Session::new();
    s.prompt("the task");
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: big(9_000),
    })]);
    let ran = s.push_bash(Prompt {
        text: "Ran `cargo test`\nFAILED at auth.rs:14".into(),
        image: None,
        shown: Some("!cargo test".into()),
    });
    s.prompt("fix that");
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: "on it".into(),
    })]);

    // The floor: everything droppable goes.
    let (record, r) = plan(
        &s,
        &spec(),
        0,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );
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
    s.push_bash(Prompt {
        text: big(30_000),
        image: None,
        shown: Some("!cargo test".into()),
    });
    s.prompt("fix that");
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: "on it".into(),
    })]);

    let budget = estimate::tokens(&s.context(), &spec()) / 2;
    let (record, r) = plan(
        &s,
        &spec(),
        budget,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );
    s.record(record);
    assert!(r.aged_out > 0, "the aside was never shrunk: {r:?}");

    let joined: String = s.context().iter().map(Message::text).collect();
    assert!(
        joined.contains("the task"),
        "the question stays: {joined:.200}"
    );
    assert!(joined.contains("fix that"), "and so does this one");
}

// What the round-sized unit is for: dropping just a turn and its results
// would leave the question that asked for them standing, answered but gone.
#[test]
fn dropping_leaves_no_question_without_its_answer() {
    // Tool-bearing rounds, cut by the default tail guard.
    let mut s = Session::new();
    s.prompt("the original task");
    for i in 0..7 {
        s.push_assistant(vec![
            AssistantContent::Text(llm::message::Text { text: big(20_000) }),
            AssistantContent::ToolCall(ToolCall {
                id: format!("c{i}"),
                name: "read".into(),
                args: json!({ "path": format!("f{i}.rs") }),
            }),
        ]);
        s.push_results(vec![ToolResult::text(format!("c{i}"), "read", "contents")]);
        s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
            text: big(20_000),
        })]);
        s.prompt(format!("question {i}"));
    }
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: "last".into(),
    })]);
    let budget = estimate::tokens(&s.context(), &spec()) / 6;
    let (record, report) = plan(
        &s,
        &spec(),
        budget,
        &Policy {
            protect_tail: 0,
            ..Policy::default()
        },
    );
    assert!(
        report.dropped > 0,
        "nothing was dropped, so nothing is proven: {report:?}"
    );
    s.record(record);
    let tool_rounds = s.context();

    // Text-only rounds, under a tiny tail guard: the same unit, lighter data.
    let mut s = Session::new();
    s.prompt("the task");
    for i in 0..8 {
        s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
            text: big(40),
        })]);
        s.prompt(format!("question {i}"));
    }
    s.push_assistant(vec![AssistantContent::Text(llm::message::Text {
        text: "last".into(),
    })]);
    let budget = estimate::tokens(&s.context(), &spec()) / 3;
    let (record, report) = plan(
        &s,
        &spec(),
        budget,
        &Policy {
            protect_tail: 8,
            ..Default::default()
        },
    );
    assert!(report.dropped > 0, "the setup must actually drop something");
    s.record(record);

    let typed = |m: &Message| {
        matches!(m, Message::User { content }
            if content.iter().any(|c| matches!(c, UserContent::Text(_))))
    };
    for (label, view) in [
        ("tool rounds", tool_rounds.clone()),
        ("text rounds", s.context()),
    ] {
        assert_balanced(&view);
        // No question stands with nothing answering it: two user turns in a
        // row is a question whose answer was taken and whose asking stayed.
        for i in 1..view.len().saturating_sub(1) {
            assert!(
                !(typed(&view[i]) && typed(&view[i + 1])),
                "{label}: `{}` was left with no answer after it",
                view[i].text()
            );
        }
    }
    assert_eq!(
        tool_rounds[0].text(),
        "the original task",
        "the task itself stays"
    );
    assert!(
        s.context().iter().any(|m| m.text().contains("the task")),
        "the opening task is round zero's head and stays"
    );
}
