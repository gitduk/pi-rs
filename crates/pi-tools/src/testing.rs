//! For tests: an execution host that records what its one execution said, in order, and
//! a context over a directory.

use std::sync::{Arc, Mutex};

use crate::runtime::background::{Invocation, Peek, RunHandle};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tool::Ctx;

#[derive(Default)]
pub(crate) struct Sink {
    pub(crate) said: Arc<Mutex<Vec<String>>>,
    pub(crate) ended: Arc<Notify>,
    pub(crate) stop: Mutex<Option<CancellationToken>>,
    pub(crate) told: Arc<Mutex<Option<crate::runtime::channel::Sender>>>,
    pub(crate) output: Arc<Mutex<Option<Peek>>>,
    // Where the answer to the question last asked with `back` goes.
    pub(crate) answer: Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>,
}

struct Handle {
    told: Arc<Mutex<Option<crate::runtime::channel::Sender>>>,
    said: Arc<Mutex<Vec<String>>>,
    ended: Arc<Notify>,
    output: Arc<Mutex<Option<Peek>>>,
}

impl crate::runtime::channel::Ask for Sink {
    fn ask(
        &self,
        _scope: crate::runtime::background::Scope,
        asker: &str,
        asked: crate::runtime::channel::Asked,
    ) -> Result<Option<crate::runtime::channel::Answer>, String> {
        self.said
            .lock()
            .unwrap()
            .push(format!("ask {asker} {}", asked.question));
        if !asked.back {
            return Ok(None);
        }
        let (to, answer) = tokio::sync::oneshot::channel();
        *self.answer.lock().unwrap() = Some(to);
        Ok(Some(answer))
    }
}

impl crate::runtime::background::RunSink for Sink {
    fn start(
        &self,
        _scope: crate::runtime::background::Scope,
        invocation: Invocation,
        stop: CancellationToken,
    ) -> Arc<dyn RunHandle> {
        self.said
            .lock()
            .unwrap()
            .push(format!("start {}", invocation.title()));
        *self.stop.lock().unwrap() = Some(stop);
        Arc::new(Handle {
            told: self.told.clone(),
            said: self.said.clone(),
            ended: self.ended.clone(),
            output: self.output.clone(),
        })
    }
}

impl RunHandle for Handle {
    fn channel(&self) -> &dyn crate::runtime::channel::Channel {
        self
    }

    fn id(&self) -> u64 {
        7
    }
    fn watch(&self, output: Peek) {
        *self.output.lock().unwrap() = Some(output);
    }
    fn end(&self) {
        self.said.lock().unwrap().push("end".into());
        self.ended.notify_one();
    }
}

impl crate::runtime::channel::Channel for Handle {
    fn watch_prompts(&self) {
        self.said.lock().unwrap().push("watch".into());
    }
    fn answer_text(&self, question: u64, text: String) {
        self.said
            .lock()
            .unwrap()
            .push(format!("answer {question}: {text}"));
    }

    fn message(&self, _: message::WireMessage) -> Result<crate::runtime::channel::Receipt, String> {
        Err("no conversation is registered".into())
    }

    fn observe(
        &self,
        _: message::SessionId,
        _: Option<crate::runtime::channel::Cursor>,
    ) -> Result<Box<dyn crate::runtime::channel::Observation>, String> {
        Err("no conversation is registered".into())
    }

    fn listen(&self, to: crate::runtime::channel::Sender) {
        *self.told.lock().unwrap() = Some(to);
    }

    fn status(&self, text: String) {
        self.said.lock().unwrap().push(format!("status {text}"));
    }
    fn result(&self, text: String) {
        self.said
            .lock()
            .unwrap()
            .push(format!("result {}", text.trim()));
    }
    fn input(&self, text: String, note: String) {
        let said = if note.is_empty() {
            format!("input {text}")
        } else {
            format!("input {text} ({note})")
        };
        self.said.lock().unwrap().push(said);
    }
    fn task(
        &self,
        text: String,
        source: String,
        reason: String,
        event: Option<String>,
    ) -> Result<crate::runtime::channel::Receipt, String> {
        let _ = event;
        self.said
            .lock()
            .unwrap()
            .push(format!("task {text} ({source}: {reason})"));
        Err("no conversation is registered".into())
    }
    fn respond(
        &self,
        to: message::MessageId,
        text: String,
        stop: bool,
    ) -> Result<crate::runtime::channel::Receipt, String> {
        let _ = stop;
        self.said
            .lock()
            .unwrap()
            .push(format!("respond {} {text}", to.as_str()));
        Err("no conversation is registered".into())
    }
    fn ask(
        &self,
        asked: crate::runtime::channel::Asked,
    ) -> Result<crate::runtime::channel::Answer, String> {
        self.said.lock().unwrap().push(format!(
            "ask-wait {}: {}",
            asked.question,
            asked.options.join(" | ")
        ));
        Err("nothing here can put a question to the person".into())
    }
    fn choose(&self, asked: crate::runtime::channel::Asked) {
        self.said.lock().unwrap().push(format!(
            "choose {}: {}",
            asked.question,
            asked.options.join(" | ")
        ));
    }
    fn answer(&self, question: u64, picks: Vec<usize>) {
        self.said
            .lock()
            .unwrap()
            .push(format!("answer {question}: {picks:?}"));
    }
    fn interrupt(&self) {
        self.said.lock().unwrap().push("interrupt".into());
    }
    fn follow(&self) {
        self.said.lock().unwrap().push("follow".into());
    }
    fn drive(&self, on: bool) {
        self.said.lock().unwrap().push(format!("drive {on}"));
    }
}

pub(crate) fn ctx(dir: &std::path::Path) -> Ctx {
    Ctx::new(tool::Workspace::new(dir).unwrap())
}

/// A file the model has seen: written, its mtime backdated so a command's own
/// write moves it even on a coarse clock, and noted as the view.
pub(crate) fn seen(
    dir: &std::path::Path,
    ctx: &Ctx,
    name: &str,
    content: &str,
) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    (std::fs::File::options().write(true).open(&path).unwrap())
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(10))
        .unwrap();
    ctx.note_view(&path, content);
    path
}
