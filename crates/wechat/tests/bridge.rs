//! pi-wechat's side of fd 3, driven the way pi drives it, against a server
//! that is not there: what it says up the socket is the whole contract.

use std::os::fd::AsRawFd;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_pi-wechat");

#[tokio::test]
async fn outside_pi_it_refuses_to_run() {
    let out = Command::new(BIN)
        .env_remove("PI_EVENTS_FD")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no fd 3"));
}

// A saved login goes straight to the background; a reply it cannot deliver
// is said on the screen, not lost.
#[tokio::test]
async fn a_saved_login_detaches_at_once_and_a_failed_send_is_noticed() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("wechat.json"),
        json!({
            "token": "t",
            // Nothing listens on the discard port: every call fails fast.
            "base_url": "http://127.0.0.1:9",
            "peer": "u1",
            "context_token": "c1",
            "get_updates_buf": "",
        })
        .to_string(),
    )
    .unwrap();

    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd = theirs.as_raw_fd();
    let mut cmd = Command::new(BIN);
    cmd.env("PI_EVENTS_FD", "3")
        .env("PI_HOME", home.path())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let _child = cmd.spawn().unwrap();
    drop(theirs);
    ours.set_nonblocking(true).unwrap();
    let (read, mut write) = tokio::net::UnixStream::from_std(ours).unwrap().into_split();
    let mut lines = BufReader::new(read).lines();
    let mut next = async || -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("it said something")
            .unwrap()
            .expect("fd 3 still open");
        serde_json::from_str(&line).unwrap()
    };

    let detach = next().await;
    assert!(
        detach["detach"].as_str().unwrap().contains("saved login"),
        "{detach}"
    );
    assert_eq!(next().await, json!({ "status": "connected" }));

    write
        .write_all(b"{\"started\":true}\n{\"reply\":\"hello\"}\n")
        .await
        .unwrap();
    loop {
        let said = next().await;
        if said["notice"]
            .as_str()
            .is_some_and(|n| n.contains("send failed"))
        {
            break;
        }
    }
}
