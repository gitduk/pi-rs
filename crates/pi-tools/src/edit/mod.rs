mod edits;
mod notes;
mod sketch;

use self::edits::{Anchor, Refusal};
use self::notes::{blank_note, broke_syntax, notes};
use self::sketch::sketch;
pub(crate) use self::sketch::{CHANGED_RUN, rows as sketch_rows};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use tool::limit::{MAX_BYTES, over_limit};
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

/// The diff a whole-file write shows: one landing that took every old row,
/// changed runs cut past `changed_run` rows.
/// The rows a whole-file rewrite changed, and the same rows as one text: the
/// two ways a caller reads one change.
pub(crate) fn rewrite_rows(
    path: &str,
    old: &str,
    new: &str,
    changed_run: usize,
) -> Vec<tool::row::Row> {
    let rows = edits::split_bom(new).1.lines().count();
    let applied = edits::Applied {
        content: new.to_string(),
        landed: vec![edits::Landed {
            start: 1,
            end: rows,
            took: edits::split_bom(old)
                .1
                .lines()
                .map(str::to_string)
                .collect(),
            took_at: 1,
        }],
        left_blank: Vec::new(),
    };
    sketch_rows(path, &applied, changed_run)
}

pub(crate) fn rewrite_sketch(path: &str, old: &str, new: &str, changed_run: usize) -> String {
    tool::row::render(&rewrite_rows(path, old, new, changed_run))
}

// What a call that gets the arguments wrong is told, so it resends the call
// instead of staring at a bare serde error for a field it never named.
const ARGS_HINT: &str = "`edit` requires `path` and `edits`. \
    Set `edits` to a list of replacements. \
    Each replacement requires exactly one anchor: `old_string`, `insert_after`, or `insert_before`. \
    Use `new_string` for the new content. Send the complete call again.";

const SHAPE: &str = r#"Replace text in one file by specifying its existing text.

Use this tool when existing text identifies the change.
Examples include a particular line, function, or matching text.

Use `path` to name the file.
Use `edits` for the list of changes.
Each change matches against the file before this call.
Changes match independently of other changes in the list.
The tool writes only if every change succeeds.
A refusal leaves the file unchanged.

An anchor is existing text that identifies where to change the file.
Specify exactly one anchor per change.

- Use `old_string` for literal text to replace, not a regex.
- Use `insert_after` to insert new content immediately after the literal anchor text.
- Use `insert_before` to insert new content immediately before the literal anchor text.

Use `new_string` for the replacement or inserted content.
For an insertion, include only the new content.
Do not repeat the anchor.
An empty `new_string` deletes the text matched by `old_string`.
To delete complete lines, include their newlines in `old_string`.
Otherwise, the line breaks remain as blank lines.

The anchor must match exactly once unless `replace_all` is true.
The tool refuses multiple matches and lists their locations.
If an anchor is not unique, include surrounding lines to identify it.
If an anchor ends with a newline, the insertion starts on the next line.
Otherwise, the insertion stays within the line.

Set `whole_block: true` to select a complete block by its first line.
Supported blocks include functions, classes, and Markdown sections.
You do not need to quote the block's body.
The result reports the selected line range.

Use an available shell tool when a pattern or position alone selects the change.
Examples include regex substitutions, line ranges, and character translations.
Use `write` when the file does not exist.
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
                        "in {path}, edits[{index}]: {}. Specify exactly one of \
                             `old_string`, `insert_after`, `insert_before`.",
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
                    "in {path}, edits[{index}]: `whole_block` requires a parser. \
                     No parser supports {path}. Quote the block in `old_string` or use `insert_after`."
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
                    "description": "Specify the file path relative to the workspace.",
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Specify one or more changes. Each matches independently against the file before this call.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {
                                "type": "string",
                                "description": "Specify literal text to replace. Supply exactly one of `old_string`, `insert_after`, or `insert_before`.",
                            },
                            "insert_after": {
                                "type": "string",
                                "description": "Insert `new_string` immediately after this literal text.",
                            },
                            "insert_before": {
                                "type": "string",
                                "description": "Insert `new_string` immediately before this literal text.",
                            },
                            "new_string": {
                                "type": "string",
                                "description": "Specify the replacement or inserted content. For an insertion, do not repeat the anchor. An empty string deletes the text matched by `old_string`.",
                            },
                            "replace_all": {
                                "type": "boolean",
                                "description": "Apply the change to every match. If false, the anchor must match exactly once. The default is false.",
                            },
                            "whole_block": {
                                "type": "boolean",
                                "description": "Select a complete block by using its first line as the anchor. Blocks include functions, classes, and Markdown sections. The default is false.",
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
                    ". edit changes existing files. Use write to create one"
                }
                std::io::ErrorKind::InvalidData => {
                    ". edit is for text. Convert it first, or leave it alone"
                }
                _ => "",
            };
            ToolError::Invalid(format!("{path}: {e}{hint}"))
        })?;
        // Read-before-edit, and the staleness note: both read off the last view
        // of this file, recorded by read/grep/write/edit.
        let Some(viewed) = ctx.viewed(&real) else {
            return Err(ToolError::Invalid(format!(
                "{path}: read it before editing it"
            )));
        };
        let stale = *viewed != *content;

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

        crate::write::atomic_write(&real, applied.content.as_bytes()).await?;
        ctx.note_write(&real);
        // The model just saw this change; the note is for outside drift.
        ctx.note_view(&real, &applied.content);
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

        let patch = sketch(path, &applied, CHANGED_RUN);
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
        // The model is spared a long run; the preview carries it whole.
        Ok(ToolOutput::text(report).with_preview(sketch(path, &applied, usize::MAX)))
    }
}
