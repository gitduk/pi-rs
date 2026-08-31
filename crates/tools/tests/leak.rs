use serde_json::json;
use tools::{Ctx, Tool, Workspace};

// A shell that backgrounds a long sleeper and then waits. Killing only the
// shell leaves the sleeper running.
#[tokio::test]
async fn a_timeout_takes_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let c = Ctx::new(Workspace::new(dir.path()).unwrap());
    let pidfile = dir.path().join("gpid");

    let err = match tools::bash::Bash
        .execute(
            json!({
                "command": format!("sleep 30 & echo $! > {}; sleep 30", pidfile.display()),
                "timeout_ms": 400,
            }),
            &c,
        )
        .await
    {
        Err(e) => e,
        Ok(out) => panic!("expected a timeout error, got: {}", out.flatten()),
    };
    assert!(
        err.to_string().contains("everything it spawned were killed"),
        "{err}"
    );
    assert_eq!(err.code(), Some("TOOL_TIMEOUT"));

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "grandchild {pid} outlived the timeout"
    );
}
