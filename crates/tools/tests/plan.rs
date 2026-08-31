use serde_json::json;
use tools::todo::{Todo, TodoStatus, render};
use tools::{Ctx, Tool, ToolError, Workspace};

fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::new(dir.path()).unwrap();
    (dir, Ctx::new(ws))
}

fn item(task: &str, status: &str) -> serde_json::Value {
    json!({ "task": task, "status": status })
}

// What the tool answers with, which is the plan as it now stands: the model has
// no other view of it.
async fn run(c: &Ctx, args: serde_json::Value) -> String {
    tools::todo::TodoTool.execute(args, c).await.unwrap().flatten()
}

async fn err(c: &Ctx, args: serde_json::Value) -> ToolError {
    tools::todo::TodoTool.execute(args, c).await.unwrap_err()
}

async fn set(c: &Ctx, items: Vec<serde_json::Value>) -> String {
    run(c, json!({ "op": "set", "items": items })).await
}

#[tokio::test]
async fn a_written_list_comes_back_numbered_marked_and_counted() {
    let (_d, c) = ctx();
    let out = set(
        &c,
        vec![
            item("read the code", "done"),
            item("fix the bug", "in_progress"),
            item("run tests", "pending"),
        ],
    )
    .await;

    assert!(out.contains("1. [x] read the code"), "{out}");
    assert!(out.contains("2. [~] fix the bug"), "{out}");
    assert!(out.contains("3. [ ] run tests"), "{out}");
    assert!(out.contains("2 open, 1 closed"), "{out}");
}

// The numbers the answer shows are what `mark` addresses, so the answer has to
// carry the list rather than a count of it.
#[tokio::test]
async fn the_answer_is_the_plan_not_an_acknowledgement() {
    let (_d, c) = ctx();
    let out = set(&c, vec![item("fix the bug", "pending")]).await;
    assert!(out.contains("fix the bug"), "{out}");
    assert!(!out.contains("Recorded"), "{out}");
}

#[tokio::test]
async fn an_item_written_without_a_status_is_pending() {
    let (_d, c) = ctx();
    let out = set(&c, vec![json!({ "task": "write the plan" })]).await;
    assert!(out.contains("1. [ ] write the plan"), "{out}");
}

#[tokio::test]
async fn marking_moves_the_named_items_and_leaves_the_rest() {
    let (_d, c) = ctx();
    set(
        &c,
        vec![
            item("a", "in_progress"),
            item("b", "pending"),
            item("c", "pending"),
        ],
    )
    .await;

    let out = run(&c, json!({ "op": "mark", "at": [1, 2], "status": "done" })).await;
    assert!(out.contains("1. [x] a"), "{out}");
    assert!(out.contains("2. [x] b"), "{out}");
    assert!(out.contains("3. [ ] c"), "{out}");
    assert!(out.contains("1 open, 2 closed"), "{out}");
}

#[tokio::test]
async fn marking_one_item_doing_returns_the_previous_one_to_pending() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "in_progress"), item("b", "pending")]).await;

    let out = run(&c, json!({ "op": "mark", "at": [2], "status": "in_progress" })).await;
    // The item just named is the one in progress; the earlier one describes
    // work that has since been left.
    assert!(out.contains("1. [ ] a"), "{out}");
    assert!(out.contains("2. [~] b"), "{out}");
}

#[tokio::test]
async fn a_mark_carries_its_reason_for_blocked_work() {
    let (_d, c) = ctx();
    set(&c, vec![item("deploy", "pending")]).await;

    let out = run(
        &c,
        json!({ "op": "mark", "at": [1], "status": "blocked", "note": "waiting on credentials" }),
    )
    .await;
    assert!(out.contains("[!] deploy — waiting on credentials"), "{out}");
}

// Checked before anything moves: a mark that failed halfway would leave a plan
// neither the model nor the user wrote.
#[tokio::test]
async fn an_out_of_range_number_changes_nothing() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "pending"), item("b", "pending")]).await;

    let e = err(&c, json!({ "op": "mark", "at": [1, 5], "status": "done" })).await;
    assert!(e.to_string().contains("no item 5"), "{e}");
    assert!(e.to_string().contains("1 to 2"), "{e}");

    let held = c.todos.lock().unwrap();
    assert!(held.iter().all(|t| t.status == TodoStatus::Pending), "{held:?}");
}

#[tokio::test]
async fn zero_is_not_an_item_because_the_list_is_numbered_from_one() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "pending")]).await;
    let e = err(&c, json!({ "op": "mark", "at": [0], "status": "done" })).await;
    assert!(e.to_string().contains("no item 0"), "{e}");
}

#[tokio::test]
async fn marking_an_empty_list_says_to_write_one_first() {
    let (_d, c) = ctx();
    let e = err(&c, json!({ "op": "mark", "at": [1], "status": "done" })).await;
    assert!(e.to_string().contains("op `set`"), "{e}");
}

// The args are flat, so what each op needs is checked here rather than by the
// type — and the message says what the op wanted, not what the struct did.
#[tokio::test]
async fn an_op_missing_what_it_needs_says_which_field() {
    let (_d, c) = ctx();
    assert!(
        err(&c, json!({ "op": "set" })).await.to_string().contains("`items`"),
    );
    set(&c, vec![item("a", "pending")]).await;
    assert!(err(&c, json!({ "op": "mark", "at": [1] })).await.to_string().contains("`status`"));
    assert!(
        err(&c, json!({ "op": "mark", "status": "done" }))
            .await
            .to_string()
            .contains("`at`")
    );
}

// A mark that moves nothing and answers with the unchanged plan reads as a mark
// that worked.
#[tokio::test]
async fn a_mark_with_no_numbers_is_refused_rather_than_doing_nothing() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "pending")]).await;
    let e = err(&c, json!({ "op": "mark", "at": [], "status": "done" })).await;
    assert!(e.to_string().contains("at least one"), "{e}");
}

#[tokio::test]
async fn clearing_drops_the_list() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "pending")]).await;

    let out = run(&c, json!({ "op": "clear" })).await;
    assert!(out.contains("the list is empty"), "{out}");
    assert!(c.todos.lock().unwrap().is_empty());
}

// A long run can age the plan's last answer out of context, and `mark` needs
// the numbers that answer showed.
#[tokio::test]
async fn showing_reads_the_plan_back_without_changing_it() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "in_progress"), item("b", "pending")]).await;

    let out = run(&c, json!({ "op": "show" })).await;
    assert!(out.contains("1. [~] a"), "{out}");
    assert!(out.contains("2. [ ] b"), "{out}");
    assert_eq!(c.todos.lock().unwrap()[0].status, TodoStatus::InProgress);
}

#[tokio::test]
async fn showing_an_empty_plan_says_so_rather_than_failing() {
    let (_d, c) = ctx();
    assert!(run(&c, json!({ "op": "show" })).await.contains("the list is empty"));
}

#[tokio::test]
async fn only_one_item_can_be_in_progress() {
    let (_d, c) = ctx();
    let out = set(&c, vec![item("a", "in_progress"), item("b", "in_progress")]).await;

    // Two in progress is a plan the agent is not actually following.
    assert!(out.contains("1. [~] a"), "{out}");
    assert!(out.contains("2. [ ] b"), "{out}");
    let held = c.todos.lock().unwrap();
    assert_eq!(held[1].status, TodoStatus::Pending);
}

#[tokio::test]
async fn the_list_is_replaced_whole_not_merged() {
    let (_d, c) = ctx();
    set(&c, vec![item("a", "pending"), item("b", "pending")]).await;
    set(&c, vec![item("c", "pending")]).await;

    let held = c.todos.lock().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].task, "c");
}

#[tokio::test]
async fn a_reason_shows_for_blocked_and_abandoned_work_only() {
    let (_d, c) = ctx();
    let out = set(
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

// The numbers are positions in the whole list. One that shifted when a finished
// task folded away would retarget the `mark` that reads it.
#[test]
fn a_long_list_collapses_its_finished_work_without_renumbering() {
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
    assert!(out.contains("26. [~] the live one"), "{out}");
    assert!(out.contains("25. [x] done 24"), "recent closures still show: {out}");
    assert!(!out.contains("1. [x] done 0"), "{out}");
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
    assert!(out.contains("1. [x] t0"), "{out}");
}

#[tokio::test]
async fn an_empty_list_says_so_instead_of_rendering_nothing() {
    let (_d, c) = ctx();
    assert!(set(&c, vec![]).await.contains("the list is empty"));
}
