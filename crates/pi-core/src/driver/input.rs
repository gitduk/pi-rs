//! What reaches a running conversation at its next request boundary.

use std::path::PathBuf;
use std::sync::Arc;

use agent::seams::Note;
use agent::session::Prompt;
use tool::Ctx;

use super::background::Table;
use crate::core::lane::Notes;

/// The user's messages first, then what the checkout's background commands
/// have ready. Who holds the user's messages, and in what order they may go,
/// is the caller's: `prompts` is asked each time and gives none it must keep.
///
/// Notes follow: those filed for the lane while the run has its transcript,
/// already shown, then the background commands it has not been told of.
pub struct Input {
    root: PathBuf,
    executions: Arc<Table>,
    // The run's context, which knows what it wrote since a command started.
    ctx: Ctx,
    prompts: Box<dyn agent::seams::Input>,
    notes: Option<Notes>,
}

impl Input {
    pub fn new(
        root: PathBuf,
        executions: Arc<Table>,
        ctx: Ctx,
        prompts: impl agent::seams::Input + 'static,
        notes: Option<Notes>,
    ) -> Self {
        Self {
            root,
            executions,
            ctx,
            prompts: Box::new(prompts),
            notes,
        }
    }
}

impl agent::seams::Input for Input {
    fn take(&self) -> Option<Prompt> {
        if let Some(prompt) = self.prompts.take() {
            return Some(prompt);
        }
        let due = self.executions.take_ready_results(&self.root)?;
        let note = due.note_for(&self.ctx);
        Some(Prompt {
            text: due.line,
            images: Vec::new(),
            shown: Some(due.label),
            relayed: Some(note),
        })
    }

    fn note(&self) -> Option<Note> {
        if let Some(text) = self.notes.as_ref().and_then(Notes::take) {
            return Some(Note { text, shown: true });
        }
        let text = self.executions.untold(&self.root)?;
        Some(Note { text, shown: false })
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
        phone
            .channel()
            .input("a remote question".into(), String::new());
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

        let typed = std::sync::Mutex::new(Some(prompt("typed direction")));
        let ctx = Ctx::new(tool::Workspace::new(std::env::temp_dir()).unwrap());
        let input = Input::new(
            root.clone(),
            executions.clone(),
            ctx,
            move || typed.lock().unwrap().take(),
            None,
        );
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
        assert!(executions.take_back(&other).is_some());
        phone.channel().result("later".into());
        assert!(input.take().unwrap().text.contains("later"));
        assert!(input.take().is_none());
    }
}
