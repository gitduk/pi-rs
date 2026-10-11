mod edits;
mod notes;
mod sketch;

use self::edits::{Anchor, Refusal};
use self::notes::{blank_note, broke_syntax, notes};
use self::sketch::sketch;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::rows::view_hash;
use tool::limit::{MAX_BYTES, over_limit};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

// What a call that gets the arguments wrong is told, so it resends the call
// instead of staring at a bare serde error for a field it never named.
const ARGS_HINT: &str = "`edit` takes `path` and `edits`: a list of replacements, each \
    naming what to find (`old_string`, or `insert_after`/`insert_before`) and what to put \
    there (`new_string`). Send the whole call again.";

const SHAPE: &str = r#"Replace text in one file, by naming the text to find.

`path` names the file. `edits` is a list, and every entry is matched against the
file as it is now — not against what an earlier entry left. Nothing is written
unless all of them land, and a refusal leaves the file untouched.

An entry names its anchor in exactly one of three ways:

- `old_string`: the text to replace — the literal text, not a regex.
- `insert_after`: new content goes immediately after this text.
- `insert_before`: new content goes immediately before it.

`new_string` is what goes there: the replacement, or, for the two inserts, just
the new content — do not repeat the anchor in it. An empty `new_string` deletes
what `old_string` matched; to delete whole lines, include their newlines, or the
rows are emptied and left blank.

The anchor must match once. Matching twice is refused with every place named,
unless `replace_all` — when a line is not unique, widen the anchor with the
lines around it. An anchor that ends with a newline lands the insert on the next
line; one that does not, lands it inside the line.

`whole_block: true` reads the anchor as the first line of a block — a function,
a class, a markdown section — and takes the whole block, so its body need not be
quoted. The report names the lines that covered.

Reach for bash (sed, perl) when a pattern alone defines the change, and for
write when the file does not exist yet.
"#;

#[derive(Deserialize)]
struct Args {
    path: String,
    edits: Vec<EditArg>,
}

// One entry as the call spells it: the three anchors are optional here and
// refused unless exactly one is given, so a wrong call is answered, not lost.
#[derive(Deserialize)]
struct EditArg {
    #[serde(default)]
    old_string: Option<String>,
    #[serde(default)]
    insert_after: Option<String>,
    #[serde(default)]
    insert_before: Option<String>,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    whole_block: bool,
}

// The shapes models send: `edits` as a JSON string, or a lone entry instead of
// a list of one. Accepted here, since refusing them costs a turn for nothing.
fn prepare(mut args: Value) -> Value {
    let Some(edits) = args.get_mut("edits") else {
        return args;
    };
    if let Some(text) = edits.as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(text)
    {
        *edits = parsed;
    }
    if edits.is_object() {
        *edits = Value::Array(vec![std::mem::take(edits)]);
    }
    args
}

fn to_edits(path: &str, args: &[EditArg]) -> Result<Vec<edits::Edit>, ToolError> {
    args.iter()
        .enumerate()
        .map(|(index, arg)| {
            let anchor = match (&arg.old_string, &arg.insert_after, &arg.insert_before) {
                (Some(text), None, None) => Anchor::Replace(text.clone()),
                (None, Some(text), None) => Anchor::After(text.clone()),
                (None, None, Some(text)) => Anchor::Before(text.clone()),
                _ => {
                    let given = [&arg.old_string, &arg.insert_after, &arg.insert_before]
                        .into_iter()
                        .flatten()
                        .count();
                    return Err(ToolError::Invalid(format!(
                        "in {path}, edits[{index}]: {} — give exactly one of \
                             `old_string`, `insert_after`, `insert_before`",
                        match given {
                            0 => "no anchor".to_string(),
                            2 => "two anchors".to_string(),
                            n => format!("{n} anchors"),
                        }
                    )));
                }
            };
            // `whole_block` needs a parser: a block that never opened would
            // send the model hunting for a line in a file nothing parses.
            if arg.whole_block && crate::syntax::Lang::of(path).is_none() {
                return Err(ToolError::Invalid(format!(
                    "in {path}, edits[{index}]: `whole_block` needs a parser and {path} has \
                     none — quote the block in `old_string` instead, or use `insert_after`"
                )));
            }
            Ok(edits::Edit {
                anchor,
                new: arg.new_string.clone(),
                replace_all: arg.replace_all,
                whole_block: arg.whole_block,
            })
        })
        .collect()
}

// `s` cut to `max` characters, `…` where it was cut.
pub(super) fn crop(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut t: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        t.push('…');
    }
    t
}

// The refusal, logged under its tag and returned as the prose the model reads.
fn refuse(path: &str, refusal: &Refusal, count: usize) -> ToolError {
    tracing::warn!(
        target: "pi::edit",
        stage = "refuse",
        path,
        edits = count,
        reason = refusal.tag(),
        error = %refusal,
        "edit refused"
    );
    ToolError::Invalid(refusal.to_string())
}

pub struct Edit;

impl Edit {
    pub const NAME: &'static str = "edit";
}

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        SHAPE
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Workspace-relative path of the file to edit.",
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "One or more replacements. Each is matched against \
                                    the file as it is now, not against what an earlier \
                                    one left.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {
                                "type": "string",
                                "description": "Literal text to replace. Give exactly one \
                                                of old_string, insert_after, insert_before.",
                            },
                            "insert_after": {
                                "type": "string",
                                "description": "Literal text to put new_string immediately \
                                                after.",
                            },
                            "insert_before": {
                                "type": "string",
                                "description": "Literal text to put new_string immediately \
                                                before.",
                            },
                            "new_string": {
                                "type": "string",
                                "description": "The replacement, or for an insert the new \
                                                content alone — do not repeat the anchor. \
                                                Empty deletes what old_string matched.",
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "Take every match instead of refusing an \
                                                anchor that names several. Default false.",
                            },
                            "whole_block": {
                                "type": "boolean",
                                "description": "Read the anchor as the first line of a \
                                                block (a function, a class, a markdown \
                                                section) and take the whole block. Default \
                                                false.",
                            },
                        },
                        "required": ["new_string"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["path", "edits"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Write
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tool::parse_args_hinted(prepare(args), ARGS_HINT)?;
        let path = args.path.as_str();
        let edits = to_edits(path, &args.edits)?;
        let real = ctx.workspace.resolve(path, self.tier())?;
        // read refuses these at the same ceiling: below, the whole file is held
        // twice over — content and syntax tree.
        if let Ok(meta) = tokio::fs::metadata(&real).await
            && meta.len() > MAX_BYTES
        {
            return Err(ToolError::Invalid(over_limit(path, meta.len())));
        }
        // Held across the read and the write: two edits to one file in a turn
        // would both read the same bytes, and one would vanish with no error.
        let _guard = ctx.lock_file(&real).await;
        let content = tokio::fs::read_to_string(&real).await.map_err(|e| {
            let hint = match e.kind() {
                // Only the file that is not there earns the pointer to write.
                // One that is not text is never rewritten as text.
                std::io::ErrorKind::NotFound => {
                    ". edit changes existing files; use write to create one"
                }
                std::io::ErrorKind::InvalidData => {
                    ". edit is for text; convert it first, or leave it alone"
                }
                _ => "",
            };
            ToolError::Invalid(format!("{path}: {e}{hint}"))
        })?;
        // Read-before-edit, and the staleness note: both read off the last view
        // of this file, recorded by read/grep/write/edit.
        let Some(viewed) = ctx.viewed_hash(&real) else {
            return Err(ToolError::Invalid(format!(
                "{path}: read it before editing it"
            )));
        };
        let stale = viewed != view_hash(&content);

        // Nothing has touched the disk yet: a refused call leaves no trace.
        let applied =
            edits::apply(path, &content, &edits).map_err(|e| refuse(path, &e, edits.len()))?;
        if let Some(why) = broke_syntax(path, &content, &applied.content, &applied.landed) {
            tracing::warn!(
                target: "pi::edit",
                stage = "syntax",
                path,
                edits = edits.len(),
                "edit refused"
            );
            return Err(ToolError::Invalid(why));
        }

        ctx.note_write(&real);
        crate::write::atomic_write(&real, applied.content.as_bytes()).await?;
        // The model just saw this change; the note is for outside drift.
        ctx.note_view(&real, &view_hash(&applied.content));
        // What the tool is being asked to do, by shape: the one number that
        // says whether the insert anchors are earning their place.
        let forms = |want: fn(&Anchor) -> bool| edits.iter().filter(|e| want(&e.anchor)).count();
        tracing::info!(
            target: "pi::edit",
            stage = "apply",
            path,
            edits = edits.len(),
            replace = forms(|a| matches!(a, Anchor::Replace(_))),
            after = forms(|a| matches!(a, Anchor::After(_))),
            before = forms(|a| matches!(a, Anchor::Before(_))),
            blocks = edits.iter().filter(|e| e.whole_block).count(),
            "edit applied"
        );

        let patch = sketch(path, &applied);
        // Compared against apply's own view: `apply` strips the BOM, so
        // matching against raw content would miss a block starting on line 1.
        let (_, body) = edits::split_bom(&content);
        let mut report = notes(path, body, &applied);
        report.push_str(&patch);
        report.push('\n');
        if !applied.left_blank.is_empty() {
            report.push_str(&blank_note(&applied.left_blank));
        }
        if stale {
            report.push_str(&format!("\nfile changed since your last view: {path}"));
        }
        Ok(ToolOutput::text(report).with_preview(patch))
    }
}
