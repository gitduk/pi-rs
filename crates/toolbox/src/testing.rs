//! For tests: an execution host that records what its one execution said, in order, and
//! a context over a directory.

use std::sync::{Arc, Mutex};

use crate::background::{Peek, RunHandle};
use crate::channel::Told;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tool::Ctx;

#[derive(Default)]
pub(crate) struct Sink {
    pub(crate) said: Arc<Mutex<Vec<String>>>,
    pub(crate) ended: Arc<Notify>,
    pub(crate) stop: Mutex<Option<CancellationToken>>,
    pub(crate) told: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Told>>>>,
    pub(crate) output: Arc<Mutex<Option<Peek>>>,
    // Where the answer to the question last asked with `back` goes.
    pub(crate) answer: Mutex<Option<tokio::sync::oneshot::Sender<Option<String>>>>,
}

struct Handle {
    told: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Told>>>>,
    said: Arc<Mutex<Vec<String>>>,
    ended: Arc<Notify>,
    output: Arc<Mutex<Option<Peek>>>,
}

impl crate::channel::Ask for Sink {
    fn ask(
        &self,
        _root: std::path::PathBuf,
        asker: &str,
        asked: crate::channel::Asked,
    ) -> Result<Option<crate::channel::Answer>, String> {
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

impl crate::background::RunSink for Sink {
    fn start(
        &self,
        _root: std::path::PathBuf,
        description: String,
        stop: CancellationToken,
    ) -> Arc<dyn RunHandle> {
        self.said
            .lock()
            .unwrap()
            .push(format!("start {description}"));
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
    fn channel(&self) -> &dyn crate::channel::Channel {
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

impl crate::channel::Channel for Handle {
    fn listen(&self, to: tokio::sync::mpsc::UnboundedSender<Told>) {
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
    fn input(&self, text: String) {
        self.said.lock().unwrap().push(format!("input {text}"));
    }
    fn choose(&self, asked: crate::channel::Asked) {
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
}

pub(crate) fn ctx(dir: &std::path::Path) -> Ctx {
    Ctx::new(tool::Workspace::new(dir).unwrap())
}
