use std::collections::HashMap;

use crate::{Blocks, Body, Error, Files, LinePos, Op, Patch, Section, Target, tag};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    /// Where the new lines sit in the file now.
    pub start: usize,
    pub end: usize,
    /// The original lines it displaced, in order. Empty for an insertion.
    ///
    /// Carried rather than left to the caller to work out: what a range
    /// replaced is known here and nowhere after, since the file it was in has
    /// already been rewritten by the time anyone reads this.
    pub took: Vec<String>,
    /// The 1-based line the displaced lines started at in the original file.
    /// Meaningful only when `took` is non-empty; insertions set it to `start`.
    pub took_at: usize,
}

impl Landed {
    /// How many lines this put in the file.
    ///
    /// Zero for a hunk that only removed, which is the one case where `end`
    /// sits below `start` — there is no row for it to name.
    pub fn gave(&self) -> usize {
        (self.end + 1).saturating_sub(self.start)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Write {
        path: String,
        content: String,
        landed: Vec<Landed>,
    },
    Remove {
        path: String,
    },
    /// Edits land on the source, then the final content moves to `to`.
    Rename {
        from: String,
        to: String,
        content: String,
        landed: Vec<Landed>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
}

// Keeps a file's trailing-newline state, which `lines()` silently discards,
// and whether its rows end in `\r\n`, so new rows join with the same ending
// the file already has instead of mixing CRLF and LF.
fn split(content: &str) -> (Vec<&str>, bool, bool) {
    if content.is_empty() {
        return (Vec::new(), true, false);
    }
    let trailing = content.ends_with('\n');
    let crlf = content.contains("\r\n");
    let body = if trailing {
        &content[..content.len() - 1]
    } else {
        content
    };
    let lines: Vec<&str> = body
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    (lines, trailing, crlf)
}

fn join(lines: &[String], trailing: bool, crlf: bool) -> String {
    let sep = if crlf { "\r\n" } else { "\n" };
    let mut out = lines.join(sep);
    if trailing && !out.is_empty() {
        out.push_str(sep);
    }
    out
}
/// Validate every section, then build the whole plan. Nothing reaches the caller
/// unless all of it succeeds: a half-applied patch is worse than a rejected one.
pub fn apply(patch: &Patch, files: &Files<'_>, blocks: &dyn Blocks) -> Result<Plan, Error> {
    for section in &patch.sections {
        let content = *files
            .get(section.path.as_str())
            .ok_or_else(|| Error::Missing {
                path: section.path.clone(),
            })?;
        let actual = tag(content);
        if actual != section.tag {
            return Err(Error::StaleTag {
                path: section.path.clone(),
                expected: section.tag.clone(),
                actual,
            });
        }
    }

    // Blocks resolve first, so everything below sees only line ranges and a
    // construct that cannot be found rejects the patch before any edit is built.
    let resolved: Vec<Vec<Op>> = patch
        .sections
        .iter()
        .map(|s| resolve(s, files[s.path.as_str()], blocks))
        .collect::<Result<_, _>>()?;

    // Registers fill from original content, so a move reads the same bytes
    // whether its CUT and PUT sit in one section or in two files.
    let mut registers: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for (section, ops) in patch.sections.iter().zip(&resolved) {
        let content = files[section.path.as_str()];
        let (lines, _, _) = split(content);
        for op in ops {
            if let Op::Cut {
                target: Target::Range { start, end },
                register,
            } = op
            {
                bounds(section, *start, *end, lines.len())?;
                let taken = lines[start - 1..*end]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                registers.insert(register.clone(), taken);
            }
        }
    }

    let mut plan = Plan::default();
    for (section, ops) in patch.sections.iter().zip(&resolved) {
        plan.changes.push(build(
            section,
            ops,
            files[section.path.as_str()],
            &registers,
        )?);
    }
    Ok(plan)
}

// Turn every `N*` into the range it names. A construct that cannot be found is
// an error, never a guess: guessing here rewrites code nobody looked at.
fn resolve(section: &Section, content: &str, blocks: &dyn Blocks) -> Result<Vec<Op>, Error> {
    let end_of = |line: usize| -> Result<usize, Error> {
        blocks
            .end_of(&section.path, content, line)
            .ok_or_else(|| Error::NoBlockAt {
                path: section.path.clone(),
                line,
            })
    };
    let as_range = |t: &Target| -> Result<Target, Error> {
        Ok(match *t {
            Target::Range { .. } => *t,
            Target::Block { line } => Target::Range {
                start: line,
                end: end_of(line)?,
            },
        })
    };

    section
        .ops
        .iter()
        .map(|op| {
            Ok(match op {
                Op::Replace { target, body } => Op::Replace {
                    target: as_range(target)?,
                    body: body.clone(),
                },
                Op::Cut { target, register } => Op::Cut {
                    target: as_range(target)?,
                    register: register.clone(),
                },
                Op::InsertAfter {
                    at: LinePos::AfterBlock(line),
                    body,
                } => Op::InsertAfter {
                    at: LinePos::At(end_of(*line)?),
                    body: body.clone(),
                },
                other => other.clone(),
            })
        })
        .collect()
}

fn bounds(section: &Section, start: usize, end: usize, len: usize) -> Result<(), Error> {
    if start > len || end > len {
        return Err(Error::OutOfRange {
            path: section.path.clone(),
            start,
            end,
            len,
        });
    }
    Ok(())
}

fn fill(
    body: &Body,
    registers: &HashMap<Option<String>, Vec<String>>,
) -> Result<Vec<String>, Error> {
    match body {
        Body::Lines(l) => Ok(l.clone()),
        Body::Register(name) => registers.get(name).cloned().ok_or_else(|| match name {
            Some(n) => Error::UnknownRegister { name: n.clone() },
            None => Error::EmptyAnonymous,
        }),
    }
}

fn build(
    section: &Section,
    ops: &[Op],
    content: &str,
    registers: &HashMap<Option<String>, Vec<String>>,
) -> Result<Change, Error> {
    if ops.iter().any(|o| matches!(o, Op::Remove)) {
        if ops.len() > 1 {
            return Err(Error::RemoveWithOps {
                path: section.path.clone(),
            });
        }
        return Ok(Change::Remove {
            path: section.path.clone(),
        });
    }

    let (lines, trailing, crlf) = split(content);
    let len = lines.len();

    let mut spans: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut before: HashMap<usize, Vec<String>> = HashMap::new();
    let mut after: HashMap<usize, Vec<String>> = HashMap::new();
    let mut dest: Option<&str> = None;

    for op in ops {
        match op {
            Op::Replace {
                target: Target::Range { start, end },
                body,
            } => {
                bounds(section, *start, *end, len)?;
                spans.push((*start, *end, fill(body, registers)?));
            }
            Op::Cut {
                target: Target::Range { start, end },
                ..
            } => spans.push((*start, *end, Vec::new())),
            Op::InsertBefore { line, body } => {
                if *line > len.max(1) {
                    return Err(Error::OutOfRange {
                        path: section.path.clone(),
                        start: *line,
                        end: *line,
                        len,
                    });
                }
                before
                    .entry(*line)
                    .or_default()
                    .extend(fill(body, registers)?);
            }
            Op::InsertAfter { at, body } => {
                let n = match at {
                    LinePos::At(n) => {
                        bounds(section, *n, *n, len)?;
                        *n
                    }
                    LinePos::AfterBlock(_) => unreachable!("resolved before build"),
                };
                after.entry(n).or_default().extend(fill(body, registers)?);
            }
            Op::Move { dest: d } => dest = Some(d),
            Op::Remove => unreachable!("handled above"),
            Op::Replace { .. } | Op::Cut { .. } => unreachable!("blocks resolved before build"),
        }
    }

    spans.sort_by_key(|(start, _, _)| *start);
    for pair in spans.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.1 >= b.0 {
            return Err(Error::Overlap {
                path: section.path.clone(),
                a_start: a.0,
                a_end: a.1,
                b_start: b.0,
                b_end: b.1,
                overlap: b.0,
            });
        }
    }

    // An insertion buried inside a replaced span has no anchor left once the
    // span is gone; dropping it silently would apply a patch nobody wrote.
    for (start, end, _) in &spans {
        let inside_before = before.keys().find(|k| **k > *start && **k <= *end);
        let inside_after = after.keys().find(|k| **k >= *start && **k < *end);
        if let Some(k) = inside_before.or(inside_after) {
            return Err(Error::Overlap {
                path: section.path.clone(),
                a_start: *start,
                a_end: *end,
                b_start: *k,
                b_end: *k,
                overlap: *k,
            });
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(len);
    let mut landed: Vec<Landed> = Vec::new();
    let record =
        |out: &mut Vec<String>, body: Vec<String>, took: Vec<String>, took_at: usize, landed: &mut Vec<Landed>| {
            if body.is_empty() && took.is_empty() {
                return;
            }
            let start = out.len() + 1;
            out.extend(body);
            landed.push(Landed {
                start,
                end: out.len(),
                took,
                took_at,
            });
        };

    if len == 0 {
        record(
            &mut out,
            before.remove(&1).unwrap_or_default(),
            Vec::new(),
            1,
            &mut landed,
        );
    }
    let mut i = 1;
    while i <= len {
        record(
            &mut out,
            before.remove(&i).unwrap_or_default(),
            Vec::new(),
            i,
            &mut landed,
        );
        // Ranges name original lines, so the cursor jumps past the whole span
        // and later hunks keep their pre-patch numbering.
        match spans.iter().find(|(start, _, _)| *start == i) {
            Some((_, end, body)) => {
                let took = lines
                    .get(i - 1..*end)
                    .unwrap_or_default()
                    .iter()
                    .map(|l| l.to_string())
                    .collect();
                record(&mut out, body.clone(), took, i, &mut landed);
                record(
                    &mut out,
                    after.remove(end).unwrap_or_default(),
                    Vec::new(),
                    i,
                    &mut landed,
                );
                i = end + 1;
            }
            None => {
                out.push(lines[i - 1].to_string());
                record(
                    &mut out,
                    after.remove(&i).unwrap_or_default(),
                    Vec::new(),
                    i,
                    &mut landed,
                );
                i += 1;
            }
        }
    }
    if len == 0 {
        record(
            &mut out,
            after.remove(&0).unwrap_or_default(),
            Vec::new(),
            1,
            &mut landed,
        );
    }

    let content = join(&out, trailing, crlf);
    Ok(match dest {
        Some(to) => Change::Rename {
            from: section.path.clone(),
            to: to.to_string(),
            content,
            landed,
        },
        None => Change::Write {
            path: section.path.clone(),
            content,
            landed,
        },
    })
}

/// A standard unified patch of the changes, for readers that understand one.
///
/// Built from the hunks the applier already knows rather than a fresh diff:
/// the took/gave rows are the change, and a diff would only re-derive them.
/// Rows are LF; a file's own line ending is applied on write, not here.
/// Removals of whole files are left out — nothing in a standard patch names
/// a deleted file more usefully than the report already does.
///
/// Context lines come from the file's prior content, which `apply` no longer
/// has once a change has been built — the caller passes it in.
pub fn unified_patch(changes: &[Change], before: &HashMap<&str, &str>) -> String {
    let mut out = String::new();
    for change in changes {
        let (old_path, new_path, old, content, landed) = match change {
            Change::Remove { .. } => continue,
            Change::Write {
                path,
                content,
                landed,
                ..
            } => (
                path,
                path,
                before.get(path.as_str()).copied().unwrap_or_default(),
                content,
                landed,
            ),
            Change::Rename {
                from,
                to,
                content,
                landed,
                ..
            } => (
                from,
                to,
                before.get(from.as_str()).copied().unwrap_or_default(),
                content,
                landed,
            ),
        };
        hunks(&mut out, old_path, new_path, old, content, landed);
    }
    out
}

/// The first line the changes touch in the new file, for a reader to jump to.
/// A hunk that only removed anchors at the line before the deletion, which is
/// where the patch header places it and the closest row still in the file.
pub fn first_changed_line(changes: &[Change]) -> Option<usize> {
    for change in changes {
        let (content, landed) = match change {
            Change::Remove { .. } => continue,
            Change::Write {
                content,
                landed,
                ..
            }
            | Change::Rename {
                content,
                landed,
                ..
            } => (content, landed),
        };
        let lines: Vec<&str> = content.lines().collect();
        if let Some(l) = landed.iter().find(|l| changed(l, &lines)) {
            return Some(if l.gave() == 0 {
                l.start.saturating_sub(1).max(1)
            } else {
                l.start
            });
        }
    }
    None
}
fn hunks(out: &mut String, old_path: &str, new_path: &str, old_content: &str, content: &str, landed: &[Landed]) {
    const CONTEXT: usize = 3;
    let old: Vec<&str> = old_content.lines().collect();
    let new: Vec<&str> = content.lines().collect();
    let changed: Vec<&Landed> = landed.iter().filter(|l| changed(l, &new)).collect();
    if changed.is_empty() {
        return;
    }
    out.push_str(&format!("--- a/{old_path}\n+++ b/{new_path}\n"));
    // Context rows are never shared between neighbouring hunks: each takes
    // what it can up to the next hunk's displaced rows, front to back.
    let mut ctx_end = 0usize;
    for (i, l) in changed.iter().enumerate() {
        let change_end = l.took_at - 1 + l.took.len();
        let next_start = changed.get(i + 1).map_or(usize::MAX, |n| n.took_at - 1);
        let pre = CONTEXT.min(l.took_at.saturating_sub(ctx_end + 1));
        let post = CONTEXT
            .min(next_start.saturating_sub(change_end))
            .min(old.len().saturating_sub(change_end));

        let old_start = l.took_at - pre;
        let new_start = l.start.saturating_sub(pre).max(1);
        let old_len = pre + l.took.len() + post;
        let new_len = pre + l.gave() + post;
        // A zero-length side names the line before the change: there is no
        // row for it to count, so the header points at where one would sit.
        let old_at = if old_len == 0 {
            old_start.saturating_sub(1)
        } else {
            old_start
        };
        let new_at = if new_len == 0 {
            new_start.saturating_sub(1)
        } else {
            new_start
        };
        out.push_str(&format!("@@ -{old_at},{old_len} +{new_at},{new_len} @@\n"));
        for t in &old[change_end - l.took.len() - pre..change_end - l.took.len()] {
            out.push(' ');
            out.push_str(t);
            out.push('\n');
        }
        for t in &l.took {
            out.push('-');
            out.push_str(t);
            out.push('\n');
        }
        for g in new.get(l.start.saturating_sub(1)..l.end).unwrap_or_default() {
            out.push('+');
            out.push_str(g);
            out.push('\n');
        }
        for t in &old[change_end..change_end + post] {
            out.push(' ');
            out.push_str(t);
            out.push('\n');
        }
        ctx_end = change_end + post;
    }
}

// Whether a hunk changes anything: a replacement whose body matches the lines
// it displaces is a write that wrote nothing, and no patch should name it.
fn changed(l: &Landed, lines: &[&str]) -> bool {
    let gave = lines
        .get(l.start.saturating_sub(1)..l.end)
        .unwrap_or_default();
    l.took.len() != gave.len()
        || l.took
            .iter()
            .zip(gave)
            .any(|(t, g)| t != g)
}
