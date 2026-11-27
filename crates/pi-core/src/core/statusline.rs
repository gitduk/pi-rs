//! The `statusline` tool: what the row under the input shows, set from the
//! conversation and kept in one file.

use serde_json::{Value, json};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// Sets what the statusline draws. The layout is the same shape
/// `~/.pi/statusline.json` holds: lines of fields, texts and groups.
///
/// A field is a name the surface fills from what it knows — `model`,
/// `effort`, `spent`, `cache`, `ctx`, `cost`, `state`, `workspace`, `branch`,
/// `diff`, `tabs`, `flash`, `tier`, `update`, and the status parts (`elapsed`,
/// `in_out`, `speed`, `compacted`, `queued`). `style` on any of
/// them names a theme tone (`muted`, `heading`, `emphasis`, `code`, `input`,
/// `ok`, `err`, `add`, `del`) or writes one out (`"34"`,
/// `{"color":"#5c9cf5","sgr":["bold"]}`); `diff` alone paints itself, in the
/// theme's added and removed colours.
/// `{"unless": "<field>", "items": […]}` draws its items only while that
/// field has nothing to say, and `{"when": "<field>", "items": […]}`
/// only while it has something to say; the surface decides both from the same
/// facts, with no script in between.
/// A script is told the session it draws for (`PI_SESSION_DIR`), the row's
/// width in cells (`PI_COLUMNS`), and what the row knows as JSON (`PI_FACTS`);
/// the SGR runs it prints colour its answer.
pub struct Statusline;

#[async_trait::async_trait]
impl Tool for Statusline {
    fn name(&self) -> &str {
        "statusline"
    }

    fn description(&self) -> &str {
        "Set what the statusline shows: the rows above and under the input line. Each line \
         holds fields, texts, groups, scripts and conditions, left and right. One file for \
         every checkout; `reset` puts the built-in default back."
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {
            "layout": {
                "type": "object",
                "description": "lines: [{place, left, right, sep}] of items; `place` is top or bottom (default bottom); an item is a field's name, or {field, style}, {text, style}, {group: [...]}, {script: {command or name, every, timeout, placeholder}}, or {unless: <field>, items: [...]} / {when: <field>, items: [...]}, which draws its items only while that field says nothing / says something. A script shows the first line it prints, and is told `PI_SESSION_DIR`, `PI_COLUMNS` and `PI_FACTS`; its SGR runs colour it."
            },
            "reset": { "type": "boolean", "description": "put the built-in default back and drop the file" }
        }, "additionalProperties": false })
    }

    async fn execute(&self, args: Value, _ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let reset = args.get("reset").and_then(Value::as_bool).unwrap_or(false);
        if reset {
            pi_store::statusline::clear().map_err(|e| ToolError::Invalid(e.to_string()))?;
            return Ok(ToolOutput::text("the statusline is back to its default"));
        }
        let layout = args
            .get("layout")
            .ok_or_else(|| ToolError::Invalid("give a `layout`, or `reset: true`".into()))?;
        let text = serde_json::to_string(layout).map_err(|e| ToolError::Invalid(e.to_string()))?;
        let parsed = pi_store::statusline::parse(&text)
            .map_err(|why| ToolError::Invalid(why.to_string()))?;
        pi_store::statusline::write_text(&text).map_err(|e| ToolError::Invalid(e.to_string()))?;
        Ok(ToolOutput::text(summary(&parsed)))
    }
}

// What was set, said back: the model reads this instead of the screen.
fn summary(layout: &pi_store::statusline::Layout) -> String {
    use pi_store::statusline::Item;
    let mut said = Vec::new();
    // The words the model writes, so what it is told back is what it sent.
    fn walk(items: &[Item], said: &mut Vec<String>) {
        for item in items {
            match item {
                Item::Field(field) => said.push(field.name()),
                Item::Styled(s) => said.push(s.field.name()),
                Item::Script(script) => {
                    said.push(format!("{} every {}s", script.label(), script.every))
                }
                Item::Group(g) => walk(&g.group, said),
                // Said after its items, so the answer reads as the layout does.
                Item::Condition(c) => {
                    walk(&c.items, said);
                    said.push(format!("{} {}", c.keeps.key(), c.field.name()));
                }
                Item::Text(_) => {}
            }
        }
    }
    for line in &layout.lines {
        walk(&line.left, &mut said);
        walk(&line.right, &mut said);
    }
    format!(
        "statusline set: {} row(s), fields: {}",
        layout.lines.len(),
        said.join(", ")
    )
}
