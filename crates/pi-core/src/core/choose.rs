//! A question put to the user with a few options, each the line they would
//! send to pick it: a prompt for the model, or a command for pi. One that
//! takes several picks sends them as one prompt, a line each. The model asks
//! with `choose`; a background job asks on fd 3. Either way the pick arrives
//! as if typed, and nothing waits for it.

use async_trait::async_trait;
use serde_json::{Value, json};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// The most options one question offers.
pub const MAX_OPTIONS: usize = 9;

/// Who asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    Model,
    /// A background job: its id, and its name as a person reads it.
    Job {
        id: u64,
        name: String,
    },
}

/// A question and the lines that answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub from: Asker,
    pub text: String,
    pub options: Vec<String>,
    /// Several may be picked, sent together as one prompt.
    pub multiple: bool,
}

impl Question {
    /// The question as asked, or why it cannot be: each option is one line,
    /// so picking it agrees to all of it, and with `multiple` none is a
    /// command, since the picks go to the model together.
    pub fn new(
        from: Asker,
        text: &str,
        options: &[String],
        multiple: bool,
    ) -> Result<Self, String> {
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
                o if multiple && !crate::input::plain(o) => Err(format!(
                    "`{o}` is a command, and several picks go to the model as words"
                )),
                o => Ok(o.to_string()),
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            from,
            text,
            options,
            multiple,
        })
    }

    /// The question `choose` was called with, read the way the call reads it.
    pub fn of_call(args: &Value) -> Result<Self, ToolError> {
        let text = args.get("question").and_then(Value::as_str).unwrap_or("");
        let options: Vec<String> = tool::parse_args_hinted(
            args.get("options").cloned().unwrap_or_default(),
            "`options` is a list of one-line strings",
        )?;
        let multiple = args.get("multiple").and_then(Value::as_bool) == Some(true);
        Self::new(Asker::Model, text, &options, multiple).map_err(ToolError::Invalid)
    }
}

/// The model's way to hand the user a decision.
pub struct Choose;

impl Choose {
    pub const NAME: &'static str = "choose";
}

#[async_trait]
impl Tool for Choose {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Put a question to the user with a few options to pick from, when the \
         decision is theirs: which approach, whether to go on, what to do next. \
         Write each option as the exact message the user would send to pick it: \
         words addressed to you, or a pi command such as `/compact <focus>` or \
         `/model <name>`, which runs as if they typed it. Put the one you \
         recommend first. With `multiple`, they may pick several, which reach \
         you together as one message, a line each; then no option may be a \
         command. The call returns at once; end your turn after it. The user's \
         pick, or whatever they write instead, arrives as their next message."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "One short sentence." },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "minItems": 1,
                    "maxItems": MAX_OPTIONS,
                    "description": "Each the one-line message that picks it.",
                },
                "multiple": {
                    "type": "boolean",
                    "description": "Let several be picked, sent together; words only, no commands.",
                },
            },
            "required": ["question", "options"],
        })
    }

    async fn execute(&self, args: Value, _ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let question = Question::of_call(&args)?;
        Ok(ToolOutput::text(format!(
            "Put to the user with {} options. End your turn now.",
            question.options.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_question_keeps_its_options_whole_and_each_on_one_line() {
        let q = Question::of_call(&json!({
            "question": "  tests   failed,\n now what? ",
            "options": [" /compact the failures ", "fix parse.rs first"],
        }))
        .unwrap();
        assert_eq!(q.text, "tests failed, now what?");
        assert_eq!(q.options, ["/compact the failures", "fix parse.rs first"]);
        assert_eq!(q.from, Asker::Model);

        let refused = |args: Value| Question::of_call(&args).unwrap_err().to_string();
        assert!(refused(json!({ "question": "?", "options": [] })).contains("1 to 9"));
        assert!(refused(json!({ "question": "?", "options": ["a\nb"] })).contains("one line"));
        assert!(refused(json!({ "question": " ", "options": ["a"] })).contains("empty"));
        assert!(refused(json!({ "question": "?", "options": [" "] })).contains("empty"));
        assert!(refused(json!({ "question": "?" })).contains("list of one-line"));
        let several =
            |options: Value| json!({ "question": "?", "options": options, "multiple": true });
        assert!(refused(several(json!(["fix it", "/compact"]))).contains("is a command"));
        assert!(refused(several(json!(["!ls"]))).contains("is a command"));
        // Trimmed, a leading space no longer keeps a slash from naming a command.
        assert!(refused(several(json!([" /api is prose"]))).contains("is a command"));
        let q = Question::of_call(&several(json!(["fix parse.rs", "!"]))).unwrap();
        assert!(q.multiple, "a bang alone is prose");
        assert!(refused(json!({ "question": "?", "options": [3] })).contains("string"));
    }
}
