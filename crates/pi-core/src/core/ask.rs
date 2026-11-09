//! Model-directed user input, rendered through the host's input capability.

use std::sync::Arc;

use async_trait::async_trait;
use message::{Block, ModelRequest};
use pi_tools::channel::{Ask, Asked};
use serde::Deserialize;
use serde_json::{Value, json};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

pub struct AskUser(pub Arc<dyn Ask>);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    text: String,
    #[serde(default)]
    options: Vec<String>,
}

fn arguments(value: Value) -> Result<Args, ToolError> {
    let mut args: Args = tool::parse_args(value).map_err(|e| ToolError::Invalid(e.to_string()))?;
    args.text = args.text.trim().to_string();
    args.options = args
        .options
        .into_iter()
        .map(|s| s.trim().to_string())
        .collect();
    if args.text.is_empty()
        || args.options.len() > super::question::MAX_OPTIONS
        || args
            .options
            .iter()
            .any(|s| s.is_empty() || s.contains(['\n', '\r']))
        || args
            .options
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            != args.options.len()
    {
        return Err(ToolError::Invalid(
            "provide a nonempty text and distinct nonempty single-line options".into(),
        ));
    }
    Ok(args)
}

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        "ask"
    }
    fn description(&self) -> &str {
        "Ask the user for missing information or a choice and wait for their answer. Options are suggestions; free text is always accepted."
    }
    fn tier(&self) -> Tier {
        Tier::Read
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {
            "text": { "type": "string", "minLength": 1 },
            "options": { "type": "array", "maxItems": super::question::MAX_OPTIONS, "uniqueItems": true,
                "items": { "type": "string", "minLength": 1 } }
        }, "required": ["text"], "additionalProperties": false })
    }
    fn model_request(&self, args: &Value) -> (ModelRequest, Vec<Block>) {
        match arguments(args.clone()) {
            Ok(args) => (
                ModelRequest::Ask {
                    options: args.options,
                },
                vec![Block::Text(args.text)],
            ),
            // Malformed invocations return a native tool error without opening user input.
            Err(_) => (
                ModelRequest::Tool {
                    name: self.name().into(),
                },
                vec![Block::Data(args.clone())],
            ),
        }
    }
    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args = arguments(args)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let request = ctx
            .request()
            .filter(|message| {
                matches!(
                    message.kind(),
                    message::Kind::Request(message::Request::FromModel(ModelRequest::Ask { .. }))
                )
            })
            .and_then(message::Message::request_id);
        let asked = Asked {
            question: args.text,
            options: args.options,
            back: true,
        };
        let answer = match &request {
            Some(request) => self.0.user_input(ctx.into(), request, self.name(), asked),
            None => self.0.ask(ctx.into(), self.name(), asked),
        }
        .map_err(ToolError::Invalid)?
        .ok_or_else(|| ToolError::Invalid("user input handler returned no response port".into()))?;
        let answer = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            answer = answer => answer.map_err(|_| ToolError::Invalid("user input handler closed".into()))?,
        };
        match answer {
            Some(text) if !text.trim().is_empty() => {
                if let Some(request) = &request
                    && let Some(response) = self.0.response(request).map_err(ToolError::Invalid)?
                {
                    let message::Kind::Response { status, .. } = response.kind() else {
                        unreachable!("accepted user response")
                    };
                    match status {
                        message::Status::Stopped => return Err(ToolError::Cancelled),
                        message::Status::Failed(failure) => {
                            return Err(ToolError::Invalid(format!(
                                "{} [code: {}]",
                                failure.message, failure.code
                            )));
                        }
                        message::Status::Done => {}
                    }
                    return Ok(ToolOutput {
                        content: crate::driver::input::answer_content(response.body()),
                        preview: None,
                        spent: Default::default(),
                        warned: false,
                    });
                }
                Ok(ToolOutput::text(text))
            }
            Some(_) => Err(ToolError::Invalid("user answer must not be blank".into())),
            None => Err(ToolError::Cancelled),
        }
    }
}

#[cfg(test)]
mod tests;
