//! What reaches a running conversation at its next request boundary.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use agent::session::Prompt;

use super::jobs::Table;

pub struct Input {
    root: PathBuf,
    jobs: Arc<Table>,
    prompts: Mutex<VecDeque<Prompt>>,
}

impl Input {
    pub fn new(root: PathBuf, jobs: Arc<Table>) -> Self {
        Self {
            root,
            jobs,
            prompts: Mutex::default(),
        }
    }

    pub fn push(&self, prompt: Prompt) {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(prompt);
    }

    /// User messages that the run did not consume, for its caller to keep.
    pub fn pending(&self) -> Vec<Prompt> {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect()
    }
}

impl agent::seams::Input for Input {
    fn take(&self) -> Option<Prompt> {
        if let Some(prompt) = self
            .prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
        {
            return Some(prompt);
        }
        let due = self.jobs.take_ready_results(&self.root)?;
        Some(Prompt {
            text: due.line,
            images: Vec::new(),
            shown: Some(due.label),
            relayed: Some(due.note),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::seams::Input as _;
    use toolbox::background::JobSink as _;

    fn prompt(text: &str) -> Prompt {
        Prompt {
            text: text.into(),
            images: Vec::new(),
            shown: None,
            relayed: None,
        }
    }

    #[tokio::test]
    async fn results_join_the_active_run_once_without_taking_other_checkouts_or_user_inputs() {
        let root = PathBuf::from("/checkout");
        let other = PathBuf::from("/elsewhere");
        let jobs = Arc::new(Table::default());
        let sink = crate::driver::jobs::Jobs::new(jobs.clone());
        let phone = sink.start(root.clone(), "phone".into(), Default::default(), None);
        phone.input("a remote question".into());
        phone.result("phone result".into());
        let first = sink.start(root.clone(), "build".into(), Default::default(), None);
        let elsewhere = sink.start(other.clone(), "other".into(), Default::default(), None);
        let second = sink.start(root.clone(), "test".into(), Default::default(), None);
        first.result("built".into());
        first.end();
        second.result("tested".into());
        second.end();
        elsewhere.result("unrelated".into());
        elsewhere.end();

        let input = Input::new(root.clone(), jobs.clone());
        input.push(prompt("typed direction"));
        assert_eq!(input.take().unwrap().text, "typed direction");
        let results = input.take().unwrap();
        assert!(results.text.contains("phone result"));
        assert!(results.text.contains("built") && results.text.contains("tested"));
        assert!(!results.text.contains("unrelated"));
        assert!(
            results.relayed.is_some(),
            "job output is not attributed to the user"
        );
        assert!(
            input.take().is_none(),
            "the active run consumes the batch once"
        );
        let (_, super::super::jobs::Back::Input { text, .. }) = jobs.take_back(&root).unwrap()
        else {
            panic!("the remote input still opens its own turn");
        };
        assert_eq!(text, "a remote question");
        assert!(jobs.take_back(&root).is_none());
        assert!(
            jobs.wait(&root, first.id(), std::time::Duration::ZERO)
                .await
                .is_err()
        );
        assert!(jobs.take_back(&other).is_some());
        phone.result("later".into());
        assert!(input.take().unwrap().text.contains("later"));
        assert!(input.take().is_none());
    }

    #[test]
    fn unconsumed_prompts_can_be_returned_in_order() {
        let input = Input::new("/checkout".into(), Arc::default());
        input.push(prompt("first"));
        input.push(prompt("second"));
        assert_eq!(
            input
                .pending()
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(input.pending().is_empty());
    }
}
