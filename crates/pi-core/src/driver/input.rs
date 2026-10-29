//! What reaches a running conversation at its next request boundary.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use agent::session::Prompt;

use super::background::Table;

pub struct Input {
    root: PathBuf,
    executions: Arc<Table>,
    prompts: Mutex<VecDeque<Prompt>>,
}

impl Input {
    pub fn new(root: PathBuf, executions: Arc<Table>) -> Self {
        Self {
            root,
            executions,
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
        let due = self.executions.take_ready_results(&self.root)?;
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
    use toolbox::background::RunSink as _;

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
        let executions = Arc::new(Table::default());
        let sink = crate::driver::background::Commands::new(executions.clone());
        let phone = sink.start(
            root.clone(),
            toolbox::background::Invocation::shell("phone"),
            Default::default(),
        );
        phone.channel().input("a remote question".into());
        phone.channel().result("phone result".into());
        let first = sink.start(
            root.clone(),
            toolbox::background::Invocation::shell("build"),
            Default::default(),
        );
        let elsewhere = sink.start(
            other.clone(),
            toolbox::background::Invocation::shell("other"),
            Default::default(),
        );
        let second = sink.start(
            root.clone(),
            toolbox::background::Invocation::shell("test"),
            Default::default(),
        );
        first.channel().result("built".into());
        first.end();
        second.channel().result("tested".into());
        second.end();
        elsewhere.channel().result("unrelated".into());
        elsewhere.end();

        let input = Input::new(root.clone(), executions.clone());
        input.push(prompt("typed direction"));
        assert_eq!(input.take().unwrap().text, "typed direction");
        let results = input.take().unwrap();
        assert!(results.text.contains("phone result"));
        assert!(results.text.contains("built") && results.text.contains("tested"));
        assert!(!results.text.contains("unrelated"));
        assert!(
            results.relayed.is_some(),
            "execution output is not attributed to the user"
        );
        assert!(
            input.take().is_none(),
            "the active run consumes the batch once"
        );
        let (_, super::super::background::Back::Input { text, .. }) =
            executions.take_back(&root).unwrap()
        else {
            panic!("the remote input still opens its own turn");
        };
        assert_eq!(text, "a remote question");
        assert!(executions.take_back(&root).is_none());
        assert!(
            executions
                .wait(&root, first.id(), std::time::Duration::ZERO)
                .await
                .is_err()
        );
        assert!(executions.take_back(&other).is_some());
        phone.channel().result("later".into());
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
