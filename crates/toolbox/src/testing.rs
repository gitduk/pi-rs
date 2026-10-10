//! For tests: a job host that records what its one job said, in order, and
//! a context over a directory.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tool::{Ctx, JobHandle, Told};

#[derive(Default)]
pub(crate) struct Sink {
    pub(crate) said: Arc<Mutex<Vec<String>>>,
    pub(crate) ended: Arc<Notify>,
    pub(crate) stop: Mutex<Option<CancellationToken>>,
    pub(crate) told: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Told>>>,
}

struct Handle {
    said: Arc<Mutex<Vec<String>>>,
    ended: Arc<Notify>,
}

impl tool::JobSink for Sink {
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
    fn result(&self, text: String, _spent: llm::stream::Usage) {
        self.said
            .lock()
            .unwrap()
            .push(format!("result {}", text.trim()));
    }
    fn input(&self, text: String) {
        self.said.lock().unwrap().push(format!("input {text}"));
    }
    fn interrupt(&self) {
        self.said.lock().unwrap().push("interrupt".into());
    }
    fn end(&self) {
        self.said.lock().unwrap().push("end".into());
        self.ended.notify_one();
    }
}

pub(crate) fn ctx(dir: &std::path::Path) -> Ctx {
    Ctx::new(tool::Workspace::new(dir).unwrap())
}
