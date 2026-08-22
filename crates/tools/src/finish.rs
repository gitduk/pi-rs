use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

pub const NAME: &str = "yield";

/// Ends the run with a value shaped by a caller-supplied schema.
///
/// The schema goes to the provider, which is what actually constrains the
/// model. The check here is narrow and named: required top-level properties
/// must be present. It is not JSON Schema validation and does not pretend to be
/// — half a validator that claims to be whole is worse than none.
pub struct Yield {
    schema: Value,
    description: String,
}

impl Yield {
    pub fn new(schema: Value) -> Self {
        Self {
            description: "Deliver the final result and end the run. Call this once, when the \
                          work is done and the answer fits the schema. Nothing after it runs."
                .to_string(),
            schema,
        }
    }

    /// Top-level property names the schema marks required.
    fn required(&self) -> Vec<&str> {
        self.schema
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Tool for Yield {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let missing: Vec<&str> = self
            .required()
            .into_iter()
            .filter(|k| args.get(*k).is_none())
            .collect();
        if !missing.is_empty() {
            // Returned to the model as an ordinary tool error, so it can send
            // the whole object again rather than the run failing.
            return Err(ToolError::Invalid(format!(
                "the result is missing required field(s): {}. Send the whole object again.",
                missing.join(", ")
            )));
        }

        let rendered = serde_json::to_string_pretty(&args)?;
        *ctx.yielded.lock().expect("yield slot poisoned") = Some(args);
        Ok(ToolOutput::text("result accepted; the run ends here")
            .with_preview(rendered.lines().count().to_string() + " lines of JSON"))
    }
}

/// A schema usable by the tool. A bare object schema is the common case and the
/// only shape a top-level tool input can take on either wire.
pub fn check(schema: &Value) -> Result<(), String> {
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => Ok(()),
        Some(other) => Err(format!(
            "the schema's top level is `{other}`; a tool input must be an object. \
             Wrap it, e.g. {}",
            json!({ "type": "object", "properties": { "result": { "type": other } } })
        )),
        None => Err("the schema needs a top-level \"type\": \"object\"".into()),
    }
}
