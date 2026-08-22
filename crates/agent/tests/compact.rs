use agent::compact::{Policy, Report, plan};
use agent::log::Log;

/// Drive the real path — plan, record, derive — and hand back the new view.
fn compact(messages: &mut Vec<Message>, budget: usize, policy: &Policy) -> Report {
    let mut log = Log::from_messages(messages.clone());
    let (record, report) = plan(&log, budget, policy);
    log.record(record);
    *messages = log.context();
    report
}
use brain::estimate;
use brain::message::{
    AssistantContent, Message, ProviderCallId, ToolCall, ToolCallId, ToolResult, ToolResultContent,
    UserContent,
};
use serde_json::json;

fn call(id: &str, name: &str, args: serde_json::Value) -> Message {
    Message::Assistant {
        id: None,
        content: vec![AssistantContent::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            provider: Some(ProviderCallId(id.into())),
            name: name.into(),
            args,
        })],
    }
}

fn result(id: &str, name: &str, body: &str) -> Message {
    Message::tool_results(vec![ToolResult::text(ToolCallId(id.into()), name, body)])
}

fn useless(id: &str, name: &str, body: &str) -> Message {
    let mut r = ToolResult::text(ToolCallId(id.into()), name, body);
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

/// Every `tool_use` must still have exactly one answering `tool_result`, or the
/// next request is invalid on both wires.
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
    // Between the two sizes: superseding one read is enough, and the drop tier
    // — which would move every index — is never reached.
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
    // Eliding the one uneventful result is enough on its own.
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
    let before = estimate::tokens(&m);
    let r = compact(
        &mut m,
        before / 2,
        &Policy {
            protect_tail: 16_000,
        },
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
fn dropping_history_keeps_the_task_and_stays_balanced() {
    // The weight sits in assistant prose, which no amount of result elision
    // reclaims — dropping whole exchanges is the only measure left.
    let mut m = vec![Message::user("the original task")];
    for i in 0..10 {
        m.push(Message::assistant_text(big(40_000)));
        m.push(Message::user(format!("next {i}")));
    }
    let r = compact(&mut m, 20_000, &Policy { protect_tail: 0 });

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
    let first = compact(&mut m, 2_000, &Policy { protect_tail: 0 });
    let snapshot = m.clone();
    let second = compact(&mut m, 2_000, &Policy { protect_tail: 0 });

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
fn an_already_elided_result_is_not_counted_twice() {
    let mut m = vec![
        Message::user("go"),
        call("c1", "read", json!({ "path": "a.rs" })),
        Message::tool_results(vec![ToolResult {
            call: ToolCallId("c1".into()),
            provider: None,
            name: "read".into(),
            content: vec![ToolResultContent::Text(brain::message::Text {
                text: "[elided to fit the context window — read it again if you need it]".into(),
            })],
            is_error: false,
            useless: false,
        }]),
        call("c2", "read", json!({ "path": "a.rs" })),
        result("c2", "read", &big(9_000)),
    ];
    let r: Report = compact(&mut m, 1_000, &Policy { protect_tail: 0 });
    assert_eq!(r.superseded, 0, "already elided, nothing to reclaim: {r:?}");
}

mod budget {
    use agent::{Agent, Session};
    use async_trait::async_trait;
    use brain::catalog::ModelSpec;
    use brain::request::Request;
    use brain::stream::StreamEvent;
    use brain::transport::Transport;
    use futures::stream::BoxStream;
    use std::sync::Arc;

    struct Never;

    #[async_trait]
    impl Transport for Never {
        fn name(&self) -> &'static str {
            "anthropic"
        }
        async fn stream(
            &self,
            _: &ModelSpec,
            _: &Request,
        ) -> brain::Result<BoxStream<'static, brain::Result<StreamEvent>>> {
            unreachable!("budget needs no network")
        }
    }

    fn agent_with(context: u32, max_output: u32) -> Agent {
        let mut spec = brain::catalog::find("opus-5").unwrap();
        spec.context_window = context;
        spec.max_output_tokens = max_output;
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

    #[test]
    fn a_healthy_transcript_is_under_budget_and_stays_untouched() {
        let a = agent_with(200_000, 32_000);
        let s = Session::with_prompt("hello");
        let (record, r) = agent::compact::plan(&s.log, a.budget(), &agent::Policy::default());
        assert!(!r.touched());
        assert_eq!(
            record,
            agent::log::Compaction {
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
    let r = compact(&mut m, 4_000, &Policy { protect_tail: 0 });

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
