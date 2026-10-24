//! A question put to the user with a few options, each the line they would
//! send to pick it: a prompt for the model, or a command for pi. One that
//! takes several picks sends them as one prompt, a line each. A script asks
//! with `choose` on fd 3; the pick goes out as if typed, and nothing waits
//! for it — unless the script asked to have the answer back, and waits.

/// The most options one question offers.
pub const MAX_OPTIONS: usize = 9;

/// Who asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// A script tool's call, by the tool's name.
    Tool { name: String },
    /// A background job: its id, and its name as a person reads it.
    Job { id: u64, name: String },
}

impl Asker {
    /// Who asks, as the person reads it.
    pub fn name(&self) -> &str {
        match self {
            Asker::Tool { name } | Asker::Job { name, .. } => name,
        }
    }
}

/// A question and the lines that answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    /// Numbered by whoever keeps it, so an answer or a withdrawal can name it.
    pub id: u64,
    pub from: Asker,
    pub text: String,
    pub options: Vec<String>,
    /// The answer goes back to the asker, which waits on it, not out as typed.
    pub back: bool,
}

impl Question {
    /// The question a tool asked, checked as `new` checks one.
    pub fn of(from: Asker, asked: &toolbox::background::Asked) -> Result<Self, String> {
        let mut question = Self::new(from, &asked.question, &asked.options)?;
        question.back = asked.back;
        Ok(question)
    }

    /// The question as a job that follows is told it.
    pub fn asked(&self) -> toolbox::background::Asked {
        toolbox::background::Asked {
            question: self.text.clone(),
            options: self.options.clone(),
            back: self.back,
        }
    }

    /// The options at `picks` as the line they send, a line each; none when
    /// nothing was picked. A place past the last, one picked twice, or a
    /// command among several, is no answer: several go to the model as words.
    pub fn picked(&self, picks: &[usize]) -> Result<Option<String>, String> {
        if (1..picks.len()).any(|i| picks[..i].contains(&picks[i])) {
            return Err("an option is picked twice".into());
        }
        let lines = (picks.iter())
            .map(|&at| {
                (self.options.get(at).map(String::as_str))
                    .ok_or_else(|| format!("it has no option {}", at + 1))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(command) = lines
            .iter()
            .find(|l| !crate::input::plain(l))
            .filter(|_| lines.len() > 1)
        {
            return Err(format!("`{command}` is a command, so it goes alone"));
        }
        Ok((!lines.is_empty()).then(|| lines.join("\n")))
    }

    /// The question as asked, or why it cannot be: each option is one line,
    /// so picking it agrees to all of it.
    pub fn new(from: Asker, text: &str, options: &[String]) -> Result<Self, String> {
        let text = pi_store::text::collapsed(text);
        if text.is_empty() {
            return Err("the question is empty".into());
        }
        if !(1..=MAX_OPTIONS).contains(&options.len()) {
            return Err(format!("give 1 to {MAX_OPTIONS} options"));
        }
        let options = (options.iter())
            .map(|o| match o.trim() {
                "" => Err("an option is empty".to_string()),
                o if o.contains(['\n', '\r']) => Err(format!("`{o}` is not one line")),
                o => Ok(o.to_string()),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            id: 0,
            from,
            text,
            options,
            back: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_keeps_its_options_whole_and_each_on_one_line() {
        let from = || Asker::Tool {
            name: "choose".into(),
        };
        let ask = |text: &str, options: &[&str]| {
            let options: Vec<String> = options.iter().map(|o| o.to_string()).collect();
            Question::new(from(), text, &options)
        };
        let q = ask(
            "  tests   failed,\n now what? ",
            &[" /compact the failures ", "fix parse.rs first"],
        )
        .unwrap();
        assert_eq!(q.text, "tests failed, now what?");
        assert_eq!(q.options, ["/compact the failures", "fix parse.rs first"]);

        let refused = |text: &str, options: &[&str]| ask(text, options).unwrap_err();
        assert!(refused("?", &[]).contains("1 to 9"));
        assert!(refused("?", &["a\nb"]).contains("one line"));
        assert!(refused(" ", &["a"]).contains("empty"));
        assert!(refused("?", &[" "]).contains("empty"));
    }

    // One or several may be picked; a command only alone, since several go
    // to the model together as words.
    #[test]
    fn a_command_is_picked_alone() {
        let options = ["fix it", "/compact", "!ls", " /api is prose", "!"].map(String::from);
        let from = Asker::Tool {
            name: "choose".into(),
        };
        let q = Question::new(from, "?", &options).unwrap();
        assert_eq!(q.picked(&[1]), Ok(Some("/compact".into())));
        assert_eq!(
            q.picked(&[0, 4]),
            Ok(Some("fix it\n!".into())),
            "a bang alone is prose"
        );
        assert_eq!(q.picked(&[]), Ok(None));
        for several in [[0, 1], [0, 2], [0, 3]] {
            // Trimmed, a leading space no longer keeps a slash from naming a command.
            let why = q.picked(&several).unwrap_err();
            assert!(why.contains("goes alone"), "{several:?}: {why}");
        }
        assert!(q.picked(&[0, 0]).unwrap_err().contains("twice"));
        assert!(q.picked(&[9]).is_err());
    }
}
