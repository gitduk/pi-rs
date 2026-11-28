//! The binary as a user starts it: what a refused start, a one-shot prompt
//! and a resumed session each leave on disk.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use agent::session::Session;
use pi_store::session::{Header, Store};

// A chat-completions endpoint answering every request with `reply`.
fn endpoint(reply: impl Into<String>) -> String {
    recording_endpoint(reply).0
}

fn recording_endpoint(
    reply: impl Into<String>,
) -> (String, std::sync::mpsc::Receiver<(String, Vec<u8>)>) {
    let reply = reply.into();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (send, received) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream);
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
            let mut length = 0;
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                first.push_str(&line);
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let _ = send.send((first.clone(), body));
            if first.starts_with("GET ") {
                let body = serde_json::json!({"data": [{"id": "m", "context_window": 200000}]})
                    .to_string();
                let _ = write!(
                    reader.into_inner(),
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                continue;
            }
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
    (format!("http://{addr}"), received)
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
    fn answering(reply: impl Into<String>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let pi = Self::new();
        let settings = pi.home.path().join("settings.toml");
        let toml = format!(
            "model = \"local/m\"\n[provider.local]\nbase_url = \"{}\"\nformat = \"chat\"\napi_key = \"k\"\n",
            endpoint(reply)
        );
        std::fs::write(&settings, toml).unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();
        pi
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pi"))
            .args(args)
            .current_dir(self.work.path())
            .env("HOME", self.home.path())
            .env("PI_HOME", self.home.path())
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .env_remove("PI_LOG")
            .envs(vars.iter().copied())
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

    // Every file under the session store: transcripts and claims.
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

// A refused start leaves no session archive.
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

// One-shot execution keeps no session archive; diagnostics are independent.
#[test]
fn a_one_shot_prompt_prints_the_reply_and_files_nothing() {
    let pi = Pi::answering("pong");
    let out = pi.run(&["hi"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("pong"));
    assert_eq!(pi.filed(), Vec::<PathBuf>::new());
    assert!(
        !pi.home.path().join("logs").exists(),
        "default runs create no diagnostic files"
    );
    assert!(!said(&out).contains("diagnostics:"));

    let prompt = "private-prompt-marker";
    let out = pi.run_with_env(&[prompt], &[("PI_LOG", "trace")]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).contains("diagnostics:"));
    assert!(pi.filed().is_empty(), "diagnostics do not create a session");
    let logs: Vec<_> = std::fs::read_dir(pi.home.path().join("logs"))
        .unwrap()
        .collect();
    assert_eq!(logs.len(), 1);
    let path = logs[0].as_ref().unwrap().path();
    let log = std::fs::read_to_string(&path).unwrap();
    assert!(log.contains("pi::wire"));
    assert!(
        !log.contains(prompt),
        "request content must stay out of diagnostics"
    );

    let blocked = Pi::answering("pong");
    std::fs::write(blocked.home.path().join("logs"), "occupied").unwrap();
    let out = blocked.run_with_env(&["hi"], &[("PI_LOG", "debug")]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("pong"));
    assert!(said(&out).contains("diagnostics unavailable:"));
}

#[test]
fn a_provider_only_config_requires_a_model_and_never_discovers_at_startup() {
    let pi = Pi::new();
    let (url, requests) = recording_endpoint("model reply");
    std::fs::write(
        pi.home.path().join("settings.toml"),
        format!(
            r#"
[provider.local]
base_url = "{url}"
format = "chat"
api_key = "test-credential"
"#
        ),
    )
    .unwrap();
    let out = pi.run(&["hi"]);
    assert!(!out.status.success());
    assert!(said(&out).contains("no model to run"), "{}", said(&out));
    assert!(requests.try_recv().is_err(), "no request without a model");

    let out = pi.run(&["-m", "local/unlisted", "hi"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("model reply"));
    let (post, body) = requests
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(post.starts_with("POST /chat/completions "), "{post}");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["model"], "unlisted");
    assert!(requests.try_recv().is_err(), "only the completion is sent");
    assert_eq!(pi.filed(), Vec::<PathBuf>::new());
}

#[test]
fn a_project_connection_does_not_receive_inherited_or_implicit_credentials() {
    let pi = Pi::new();
    let (url, requests) = recording_endpoint("project reply");
    std::fs::create_dir(pi.work.path().join(".git")).unwrap();
    std::fs::create_dir(pi.work.path().join(".pi")).unwrap();
    std::fs::write(
        pi.home.path().join("settings.toml"),
        r#"
[provider.local]
base_url = "http://127.0.0.1:1/original"
format = "chat"
api_key = "original-service-key"
"#,
    )
    .unwrap();
    std::fs::write(
        pi.work.path().join(".pi/settings.toml"),
        format!(
            r#"
[provider.local]
base_url = "{url}"
format = "chat"
"#
        ),
    )
    .unwrap();
    let out = pi.run_with_env(
        &["-m", "local/m", "hi"],
        &[("OPENAI_API_KEY", "implicit-environment-key")],
    );
    assert!(out.status.success(), "{}", said(&out));
    let (request, _) = requests
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    assert!(request.starts_with("POST "), "{request}");
    assert!(
        !request.to_ascii_lowercase().contains("authorization:"),
        "the replacement connection received a credential: {request}"
    );
}

#[test]
fn a_large_reply_stores_only_final_messages() {
    let pi = Pi::answering(format!("greeting-reply-marker{}", "x".repeat(128 * 1024)));
    let path = pi
        .store()
        .save(
            &pi.workspace(),
            Header {
                id: "greeting",
                model: "local/m",
                name: None,
                title: None,
                created: 1,
                spent: Default::default(),
            },
            &Session::new(),
        )
        .unwrap();
    let out = pi.run(&["--resume", "greeting", "greeting-input-marker"]);
    assert!(out.status.success(), "{}", said(&out));
    let text = std::fs::read_to_string(&path.path).unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .flat_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            match value {
                serde_json::Value::Array(items) => items,
                item => vec![item],
            }
        })
        .collect();
    let types: Vec<_> = records
        .iter()
        .map(|record| record["type"].as_str().unwrap_or("missing"))
        .collect();
    let turn = ["session", "input", "assistant"];
    assert_eq!(types.get(..3), Some(&turn[..]), "{text}\n{}", said(&out));
    // A title a model writes lands as a fact of its own, whenever it comes back.
    assert!(
        types[3..].iter().all(|kind| *kind == "meta"),
        "{types:?}\n{text}"
    );
    assert_eq!(text.matches("greeting-input-marker").count(), 1);
    assert_eq!(text.matches("greeting-reply-marker").count(), 1);
    assert_eq!(records[2]["status"], "done");
    let saved = Store::read(&path).unwrap();
    assert_eq!(saved.session.entries().len(), 2);
    assert_eq!(
        pi_store::session::messages::read(&path)
            .unwrap()
            .unwrap()
            .entries
            .len(),
        2
    );
    let before = std::fs::read(&path.path).unwrap();
    pi.store()
        .save(
            &pi.workspace(),
            Header {
                id: "greeting",
                model: &saved.model,
                name: saved.name.as_deref(),
                title: saved.title.as_deref(),
                created: saved.created,
                spent: saved.spent,
            },
            &saved.session,
        )
        .unwrap();
    assert_eq!(
        std::fs::read(&path.path).unwrap(),
        before,
        "unchanged saves append nothing"
    );
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
        model: "local/m",
        name: Some("kept"),
        title: None,
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
    assert!(!pi.home.path().join("logs").exists());
    assert!(
        !pi.filed()
            .iter()
            .any(|path| path.file_name().is_some_and(|name| name == "journal.jsonl"))
    );

    let out = pi.run(&["--resume", "s1", "--name", "renamed", "more"]);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(Store::read(&path).unwrap().name.as_deref(), Some("renamed"));

    for _ in 0..2 {
        let out = pi.run_with_env(&["--resume", "s1", "diagnose"], &[("PI_LOG", "debug")]);
        assert!(out.status.success(), "{}", said(&out));
    }
    let logs: Vec<_> = std::fs::read_dir(pi.home.path().join("logs"))
        .unwrap()
        .collect();
    assert_eq!(
        logs.len(),
        2,
        "each process owns its log, even when resuming the same session"
    );
    for log in logs {
        let text = std::fs::read_to_string(log.unwrap().path()).unwrap();
        assert!(
            text.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|record| record["session"] == "s1")
        );
    }
}

// A session whose model the config no longer reaches still opens: the model
// `model` selects now runs it, and the file then names the model that ran.
#[test]
fn a_resumed_session_whose_model_is_gone_runs_on_the_configured_one() {
    let pi = Pi::answering("pong");
    let mut session = Session::default();
    session.prompt("before");
    let header = Header {
        id: "s1",
        model: "gone/m",
        name: None,
        title: None,
        created: 42,
        spent: Default::default(),
    };
    let path = pi.store().save(&pi.workspace(), header, &session).unwrap();

    let out = pi.run(&["--resume", "s1", "again"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(
        said(&out).contains("local/m"),
        "it says what it started on: {}",
        said(&out)
    );
    assert_eq!(
        Store::read(&path).unwrap().model,
        "local/m",
        "saved under the model that ran"
    );
}
