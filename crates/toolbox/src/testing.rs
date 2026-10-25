//! For tests: a job host that records what its one job said, in order, and
//! a context over a directory.

use std::sync::{Arc, Mutex};

use crate::background::{JobHandle, Peek, Told};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tool::Ctx;

#[derive(Default)]
pub(crate) struct Sink {
    pub(crate) said: Arc<Mutex<Vec<String>>>,
    pub(crate) ended: Arc<Notify>,
    pub(crate) stop: Mutex<Option<CancellationToken>>,
    pub(crate) told: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Told>>>,
    pub(crate) output: Arc<Mutex<Option<Peek>>>,
    // Where the answer to the question last asked with `back` goes.
    pub(crate) answer: Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>,
}

struct Handle {
    said: Arc<Mutex<Vec<String>>>,
    ended: Arc<Notify>,
    output: Arc<Mutex<Option<Peek>>>,
}

impl crate::background::Ask for Sink {
    fn ask(
        &self,
        _root: std::path::PathBuf,
        asker: &str,
        asked: crate::background::Asked,
    ) -> Result<Option<crate::background::Answer>, String> {
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

impl crate::background::JobSink for Sink {
    fn start(
        &self,
        _root: std::path::PathBuf,
        description: String,
        stop: CancellationToken,
        told: Option<tokio::sync::mpsc::UnboundedSender<Told>>,
    ) -> Arc<dyn JobHandle> {
        self.said
            .lock()
            .unwrap()
            .push(format!("start {description}"));
        *self.stop.lock().unwrap() = Some(stop);
        *self.told.lock().unwrap() = told;
        Arc::new(Handle {
            said: self.said.clone(),
            ended: self.ended.clone(),
            output: self.output.clone(),
        })
    }
}

impl JobHandle for Handle {
    fn id(&self) -> u64 {
        7
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
    fn input(&self, text: String) {
        self.said.lock().unwrap().push(format!("input {text}"));
    }
    fn choose(&self, asked: crate::background::Asked) {
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
    fn watch(&self, output: Peek) {
        *self.output.lock().unwrap() = Some(output);
    }
    fn end(&self) {
        self.said.lock().unwrap().push("end".into());
        self.ended.notify_one();
    }
}

pub(crate) fn ctx(dir: &std::path::Path) -> Ctx {
    Ctx::new(tool::Workspace::new(dir).unwrap())
}
