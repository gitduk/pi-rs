use serde_json::json;
use tools::todo::{Todo, TodoStatus, render};
use tools::{Ctx, Tool, Workspace};

fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

fn item(task: &str, status: &str) -> serde_json::Value {
    json!({ "task": task, "status": status })
}

/// What the tool answers with. Not the list: that reaches the model as a note
/// on the next request, recomputed from the stored plan.
async fn write(c: &Ctx, items: Vec<serde_json::Value>) -> String {
    tools::todo::TodoTool
        .execute(json!({ "items": items }), c)
        .await
        .unwrap()
        .flatten()
}

/// The plan as it now stands, the way the note and `/todo` render it.
async fn shown(c: &Ctx, items: Vec<serde_json::Value>) -> String {
    write(c, items).await;
    render(&c.todos.lock().unwrap())
}

#[tokio::test]
async fn a_written_list_comes_back_marked_and_counted() {
    let (_d, c) = ctx();
    let out = shown(
        &c,
        vec![
            item("read the code", "done"),
            item("fix the bug", "in_progress"),
            item("run tests", "pending"),
        ],
    )
    .await;

    assert!(out.contains("[x] read the code"), "{out}");
    assert!(out.contains("[~] fix the bug"), "{out}");
    assert!(out.contains("[ ] run tests"), "{out}");
    assert!(out.contains("2 open, 1 closed"), "{out}");
}

#[tokio::test]
async fn only_one_item_can_be_in_progress() {
    let (_d, c) = ctx();
    let out = shown(&c, vec![item("a", "in_progress"), item("b", "in_progress")]).await;

    // Two in progress is a plan the agent is not actually following.
    assert!(out.contains("[~] a"), "{out}");
    assert!(out.contains("[ ] b"), "{out}");
    let held = c.todos.lock().unwrap();
    assert_eq!(held[1].status, TodoStatus::Pending);
}

#[tokio::test]
async fn the_list_is_replaced_whole_not_merged() {
    let (_d, c) = ctx();
    write(&c, vec![item("a", "pending"), item("b", "pending")]).await;
    write(&c, vec![item("c", "pending")]).await;

    let held = c.todos.lock().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].task, "c");
}

#[tokio::test]
async fn a_reason_shows_for_blocked_and_abandoned_work_only() {
    let (_d, c) = ctx();
    let out = shown(
        &c,
        vec![
            json!({ "task": "deploy", "status": "blocked", "note": "waiting on credentials" }),
            json!({ "task": "rewrite", "status": "abandoned", "note": "out of scope" }),
            json!({ "task": "test", "status": "pending", "note": "ignored here" }),
        ],
    )
    .await;

    assert!(out.contains("[!] deploy — waiting on credentials"), "{out}");
    assert!(out.contains("[-] rewrite — out of scope"), "{out}");
    assert!(!out.contains("ignored here"), "{out}");
}

#[test]
fn a_long_list_collapses_its_finished_work() {
    let mut items: Vec<Todo> = (0..25)
        .map(|i| Todo {
            task: format!("done {i}"),
            status: TodoStatus::Done,
            note: None,
        })
        .collect();
    items.push(Todo {
        task: "the live one".into(),
        status: TodoStatus::InProgress,
        note: None,
    });

    let out = render(&items);
    // A finished task carries less than a pending one and should cost less.
    assert!(out.contains("… 22 finished"), "{out}");
    assert!(out.contains("[~] the live one"), "{out}");
    assert!(
        out.contains("[x] done 24"),
        "recent closures still show: {out}"
    );
    assert!(!out.contains("[x] done 0"), "{out}");
}

#[test]
fn a_short_list_shows_everything() {
    let items: Vec<Todo> = (0..5)
        .map(|i| Todo {
            task: format!("t{i}"),
            status: TodoStatus::Done,
            note: None,
        })
        .collect();
    let out = render(&items);
    assert!(!out.contains("finished\n… "), "{out}");
    assert!(out.contains("[x] t0"), "{out}");
}

#[tokio::test]
async fn an_empty_list_says_so_instead_of_rendering_nothing() {
    let (_d, c) = ctx();
    assert!(shown(&c, vec![]).await.contains("the list is empty"));
}

/// The tool answers with a count, not the plan. Echoing the list here as well
/// would leave one copy per call in the transcript, each stale the moment the
/// next call lands — and the note already carries the current one.
#[tokio::test]
async fn the_tool_acknowledges_rather_than_repeating_the_list() {
    let (_d, c) = ctx();
    let out = write(
        &c,
        vec![item("read the code", "done"), item("fix the bug", "pending")],
    )
    .await;
    assert_eq!(out.trim(), "Recorded: 1 of 2 open.");
    assert!(!out.contains("fix the bug"), "{out}");
}
