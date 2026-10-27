// Process liveness below is read from /proc; the runtime boundary itself is
// shared by every platform.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::Duration;

use pi_core::driver::jobs::{self, Jobs, State, Table};
use tokio_util::sync::CancellationToken;
use tool::{Ctx, Workspace};
use toolbox::background::{Again, JobSink};

use super::*;

struct Processes(Vec<u32>);

impl Processes {
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
            if let Ok(pids) = output
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<Vec<u32>, _>>()
                && pids.len() == 2
            {
                assert!(pids.iter().all(|&pid| Processes::running(pid)));
                return Processes(pids);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the shell and its descendant started")
}

fn carry_job(stopped: bool) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let path = root.join("jobs.json");
    let table = Arc::new(Table::default());
    let ctx = Ctx::new(Workspace::new(root).unwrap());
    let mut old = None;

    let ended = drive(async {
        let again = Again {
            // Ignore TERM so a stopped job is still cleaning up when the
            // runtime ends. A descendant proves the whole group is taken.
            command: "trap '' TERM; sleep 30 & echo \"$$ $!\"; wait".into(),
            cwd: root.to_path_buf(),
        };
        let rerun = toolbox::bash::rerun(again.clone(), &ctx);
        let stop = CancellationToken::new();
        let job =
            Jobs::new(table.clone()).start(root.to_path_buf(), "serve".into(), stop.clone(), None);
        let id = job.id();
        job.rerun(rerun.clone(), Some(again));
        rerun(job, stop).unwrap();
        old = Some(processes(&table, root, id).await);
        if stopped {
            table.stop(root, id).unwrap();
            assert!(!table.busy(root), "an update can restart during cleanup");
        }
        jobs::hand_over(&path, &table.carried(root)).unwrap();
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

    // The next runtime reads the handover and starts the same job again:
    // only the new shell and descendant are left running.
    let mut new = None;
    let ended = drive(async {
        let table = Arc::new(Table::default());
        let carried = jobs::take_over(&path).unwrap();
        assert_eq!(carried.len(), 1);
        let job = carried.into_iter().next().unwrap();
        let id = job.id;
        assert!(job.stopped);
        let rerun = toolbox::bash::rerun(job.again.clone(), &ctx);
        table.revive(job, rerun);
        assert_eq!(table.jobs(root)[0].state, State::Stopped);
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
    carry_job(false);
}

#[test]
fn a_restart_also_takes_down_a_stopped_job_still_cleaning_up() {
    carry_job(true);
}
