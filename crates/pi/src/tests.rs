// Process liveness below is read from /proc; the runtime boundary itself is
// shared by every platform.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use pi_core::driver::background::{self, Commands, State, Table};
use tokio_util::sync::CancellationToken;
use tool::{Ctx, Workspace};
use toolbox::background::{Again, RunSink};

use super::*;

struct Processes(Vec<u32>);

impl Processes {
    fn printed(output: &str) -> Option<Self> {
        let pids = output
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<Vec<u32>, _>>()
            .ok()?;
        (pids.len() == 2).then(|| Self(pids))
    }

    fn running(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // A killed descendant can remain a zombie until its adopter reaps it.
        !stat.rsplit_once(") ").unwrap().1.starts_with(['Z', 'X'])
    }

    fn assert_gone(&self) {
        let until = std::time::Instant::now() + Duration::from_secs(3);
        while self.0.iter().any(|&pid| Self::running(pid)) {
            assert!(
                std::time::Instant::now() < until,
                "still running: {:?}",
                self.0
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    async fn wait_gone(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.0.iter().any(|&pid| Self::running(pid)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the shell and its descendant were stopped");
    }
}

impl Drop for Processes {
    fn drop(&mut self) {
        // Clean up even if a regression makes a liveness assertion fail.
        if let Some(&group) = self.0.first() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{group}")])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

async fn processes(table: &Table, root: &Path, id: u64) -> Processes {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            // A restarted run attaches its output after the previous one
            // has let go, so there may not be a stream to read yet.
            let output = table.output(root, id).unwrap_or_default();
            if let Some(processes) = Processes::printed(&output) {
                assert!(processes.0.iter().all(|&pid| Processes::running(pid)));
                return processes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the shell and its descendant started")
}

fn carry_command(stopped: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("background.json");
    let table = Arc::new(Table::default());
    let ctx = Ctx::new(Workspace::new(root).unwrap());
    let mut old = None;

    let ended = drive(async {
        let again = Again {
            // Ignore TERM so a stopped execution is still cleaning up when the
            // runtime ends. A descendant proves the whole group is taken.
            command: "trap '' TERM; sleep 30 & echo \"$$ $!\"; wait".into(),
            cwd: root.to_path_buf(),
            ..Again::default()
        };
        let rerun = toolbox::bash::rerun(again.clone(), &ctx);
        let stop = CancellationToken::new();
        let execution = Commands::new(table.clone()).start(
            root.to_path_buf(),
            toolbox::background::Invocation::shell("serve"),
            stop.clone(),
        );
        let id = execution.id();
        execution.rerun(rerun.clone(), Some(again));
        rerun(execution, stop).unwrap();
        old = Some(processes(&table, root, id).await);
        if stopped {
            table.stop(root, id).unwrap();
            assert!(!table.busy(root), "an update can restart during cleanup");
        }
        background::hand_over(&path, &table.carried(root)).unwrap();
        Ok(Ended::Restart(core::Restart {
            lanes: vec![(root.to_path_buf(), None)],
            front: 0,
            flags: Vec::new(),
        }))
    })
    .unwrap();

    assert!(matches!(ended, Ended::Restart(_)));
    assert!(tokio::runtime::Handle::try_current().is_err());
    let old = old.unwrap();
    old.assert_gone();

    // The next runtime reads the handover and starts the same execution again:
    // only the new shell and descendant are left running.
    let mut new = None;
    let ended = drive(async {
        let table = Arc::new(Table::default());
        let carried = background::take_over(&path).unwrap();
        assert_eq!(carried.len(), 1);
        let execution = carried.into_iter().next().unwrap();
        let id = execution.id;
        assert!(execution.stopped);
        let rerun = toolbox::bash::rerun(execution.again.clone(), &ctx);
        table.revive(execution, rerun);
        assert_eq!(table.background(root)[0].state, State::Stopped);
        table.restart(root, id).unwrap();
        new = Some(processes(&table, root, id).await);
        old.assert_gone();
        Ok(Ended::Exit(ExitCode::SUCCESS))
    })
    .unwrap();
    assert!(matches!(ended, Ended::Exit(ExitCode::SUCCESS)));
    new.unwrap().assert_gone();
}

#[test]
fn a_restart_takes_down_background_processes_before_the_next_run() {
    carry_command(false);
}

#[test]
fn a_restart_also_takes_down_a_stopped_command_still_cleaning_up() {
    carry_command(true);
}

#[tokio::test]
async fn foreground_shells_are_isolated_and_cancel_with_their_owner() {
    let dir = tempfile::tempdir().unwrap();
    let first = Ctx::new(Workspace::new(dir.path()).unwrap());
    let second = Ctx::new(first.workspace.clone());
    let _first = first.cancel.clone().drop_guard();
    let _second = second.cancel.clone().drop_guard();
    let mut runs = Vec::new();
    for (index, ctx) in [&first, &second].into_iter().enumerate() {
        let registry = core::shells::foreground(toolbox::builtin());
        let shell = registry.get(toolbox::bash::Bash::NAME).unwrap();
        let command = format!("trap '' TERM; sleep 30 & echo \"$$ $!\" > pids{index}; wait");
        let context = ctx.clone();
        let call = tokio::spawn(async move {
            shell
                .execute(serde_json::json!({"command":command}), &context)
                .await
        });
        let processes = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(dir.path().join(format!("pids{index}")))
                    && let Some(processes) = Processes::printed(&text)
                {
                    break processes;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!call.is_finished());
        runs.push((call, processes));
    }
    first.cancel.cancel();
    runs[0].1.wait_gone().await;
    assert!(runs[1].1.0.iter().all(|&pid| Processes::running(pid)));
    second.cancel.cancel();
    runs[1].1.wait_gone().await;
    for (call, _) in runs {
        assert!(matches!(
            call.await.unwrap(),
            Err(tool::ToolError::Cancelled)
        ));
    }
}

#[tokio::test]
async fn cancelling_a_foreground_shell_stops_the_process_group() {
    {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::new(Workspace::new(dir.path()).unwrap());
        let _stop = ctx.cancel.clone().drop_guard();
        let registry = core::shells::foreground(toolbox::builtin());
        let bash = registry.get(toolbox::bash::Bash::NAME).unwrap();
        let call = {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                bash.execute(
                    serde_json::json!({"command": "sleep 30 & echo \"$$ $!\" > pids; wait"}),
                    &ctx,
                )
                .await
            })
        };
        let processes = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(output) = std::fs::read_to_string(dir.path().join("pids"))
                    && let Some(processes) = Processes::printed(&output)
                {
                    break processes;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        ctx.cancel.cancel();
        assert!(matches!(
            call.await.unwrap(),
            Err(tool::ToolError::Cancelled)
        ));
        processes.wait_gone().await;
    }
}
