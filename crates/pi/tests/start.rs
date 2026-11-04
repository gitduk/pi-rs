//! The binary as a user starts it: what a refused start, a one-shot prompt
//! and a resumed session each leave on disk.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use agent::session::Session;
use pi_store::session::{Header, Store};

// A chat-completions endpoint answering every request with `reply`.
fn endpoint(reply: &'static str) -> String {
    recording_endpoint(reply).0
}

fn recording_endpoint(
    reply: &'static str,
) -> (String, std::sync::mpsc::Receiver<(String, Vec<u8>)>) {
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
    fn answering(reply: &'static str) -> Self {
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

#[test]
fn a_provider_only_config_discovers_models_and_sends_the_original_id() {
    let pi = Pi::new();
    let (url, requests) = recording_endpoint("discovered reply");
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
    for args in [vec!["hi"], vec!["-m", "local/unlisted", "hi"]] {
        let out = pi.run(&args);
        assert!(out.status.success(), "{}", said(&out));
        assert!(String::from_utf8_lossy(&out.stdout).contains("discovered reply"));
        let (get, _) = requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(get.starts_with("GET /models "), "{get}");
        let (post, body) = requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(post.starts_with("POST /chat/completions "), "{post}");
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            body["model"],
            if args.len() == 1 { "m" } else { "unlisted" }
        );
    }
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
    let out = pi.run_with_env(&["hi"], &[("OPENAI_API_KEY", "implicit-environment-key")]);
    assert!(out.status.success(), "{}", said(&out));
    for method in ["GET ", "POST "] {
        let (request, _) = requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(request.starts_with(method), "{request}");
        assert!(
            !request.to_ascii_lowercase().contains("authorization:"),
            "the replacement connection received a credential: {request}"
        );
    }
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
