//! The binary as a user starts it: what a refused start, a one-shot prompt
//! and a resumed session each leave on disk.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use agent::session::Session;
use pi_store::session::{Header, Store};

// A chat-completions endpoint answering every request with `reply`.
fn endpoint(reply: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let _ = reader.read_exact(&mut vec![0; length]);
            let chunk = |delta: serde_json::Value, finish: Option<&str>| {
                let chunk = serde_json::json!({
                    "id": "c",
                    "object": "chat.completion.chunk",
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
                });
                format!("data: {chunk}\n\n")
            };
            let body = [
                chunk(
                    serde_json::json!({ "role": "assistant", "content": reply }),
                    None,
                ),
                chunk(serde_json::json!({}), Some("stop")),
                "data: [DONE]\n\n".into(),
            ]
            .concat();
            let _ = write!(
                reader.into_inner(),
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}")
}

// A pi home and a workspace of its own, so no run reads or files anything of
// the user's.
struct Pi {
    home: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Pi {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            work: tempfile::tempdir().unwrap(),
        }
    }

    // Configured to talk to an endpoint that always says `reply`.
    fn answering(reply: &'static str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let pi = Self::new();
        let settings = pi.home.path().join("settings.toml");
        let toml = format!(
            "base_url = \"{}\"\nformat = \"chat\"\napi_key = \"k\"\nmodel = \"m\"\n",
            endpoint(reply)
        );
        std::fs::write(&settings, toml).unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        pi
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .args(args)
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env("PI_HOME", self.home.path())
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn workspace(&self) -> PathBuf {
        self.work.path().canonicalize().unwrap()
    }

    fn store(&self) -> Store {
        Store::new(self.home.path().join("sessions"))
    }

    // Every file under the session store: transcripts, journals, claims.
    fn filed(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                match entry.path() {
                    p if p.is_dir() => walk(&p, found),
                    p => found.push(p),
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.home.path().join("sessions"), &mut found);
        found
    }
}

fn said(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// Each refusal comes before the journal and the transcript are made.
#[test]
fn a_refused_start_files_nothing() {
    let pi = Pi::new();
    let out = pi.run(&["hi"]);
    assert!(!out.status.success());
    assert!(said(&out).contains("no model to run"), "{}", said(&out));
    assert_eq!(pi.filed(), Vec::<PathBuf>::new());

    let pi = Pi::answering("pong");
    let out = pi.run(&["--resume", "nope", "hi"]);
    assert!(!out.status.success());
    assert!(said(&out).contains("no session `nope`"), "{}", said(&out));
    assert_eq!(pi.filed(), Vec::<PathBuf>::new());
}

// A one-shot prompt nobody asked to keep leaves nothing, not even a log.
#[test]
fn a_one_shot_prompt_prints_the_reply_and_files_nothing() {
    let pi = Pi::answering("pong");
    let out = pi.run(&["hi"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("pong"));
    assert_eq!(pi.filed(), Vec::<PathBuf>::new());
}

// A session named with `--resume` is kept: the turn lands in it, under its
// own id, birth and name — until `--name` gives it another.
#[test]
fn a_resumed_one_shot_is_filed_into_the_session_it_named() {
    let pi = Pi::answering("pong");
    let mut session = Session::default();
    session.prompt("before");
    let header = Header {
        id: "s1",
        model: "m",
        name: Some("kept"),
        created: 42,
        spent: Default::default(),
    };
    let path = pi.store().save(&pi.workspace(), header, &session).unwrap();
    let before = session.entries().len();

    let out = pi.run(&["--resume", "s1", "again"]);
    assert!(out.status.success(), "{}", said(&out));
    let stored = Store::read(&path).unwrap();
    assert_eq!(
        (stored.id.as_str(), stored.created, stored.name.as_deref()),
        ("s1", 42, Some("kept"))
    );
    let prompts: Vec<_> = (stored.session.entries().iter())
        .filter_map(|e| e.prompt().map(|p| p.shown_text().to_string()))
        .collect();
    assert_eq!(prompts, ["before", "again"]);
    assert!(stored.session.entries().len() > before + 1, "and the reply");
    assert!(
        pi.store().journal_path(&pi.workspace(), "s1").is_file(),
        "a kept session keeps its journal"
    );

    let out = pi.run(&["--resume", "s1", "--name", "renamed", "more"]);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(Store::read(&path).unwrap().name.as_deref(), Some("renamed"));
}
