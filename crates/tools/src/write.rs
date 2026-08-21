use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput};

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create a file or replace its entire contents. Missing parent directories \
         are created. To change part of an existing file, use edit."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative path." },
                "content": { "type": "string", "description": "Full file contents." },
            },
            "required": ["path", "content"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = serde_json::from_value(args)?;
        let path = ctx.workspace.resolve(&args.path)?;
        let rel = ctx.workspace.display(&path);

        if tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_dir()) {
            return Err(ToolError::Invalid(format!("{rel} is a directory")));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&path, &args.content).await?;

        let lines = args.content.lines().count();
        let unit = if lines == 1 { "line" } else { "lines" };
        let tag = crate::read::tag(&args.content);
        Ok(ToolOutput::text(format!(
            "[{rel}#{tag}] wrote {lines} {unit}, {} bytes",
            args.content.len()
        )))
    }
}
