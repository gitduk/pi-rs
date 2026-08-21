use std::collections::HashMap;

use crate::{Body, Error, Files, LinePos, Op, Patch, Section, tag};

/// Where new content landed, in the file's *new* numbering. Reported back so a
/// second edit needs no re-read: the format only pays off if it removes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landed {
    pub start: usize,
    pub end: usize,
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

/// Keeps a file's trailing-newline state, which `lines()` silently discards.
fn split(content: &str) -> (Vec<&str>, bool) {
    if content.is_empty() {
        return (Vec::new(), true);
    }
    let trailing = content.ends_with('\n');
    let body = if trailing {
        &content[..content.len() - 1]
    } else {
        content
    };
    (body.split('\n').collect(), trailing)
}

fn join(lines: &[String], trailing: bool) -> String {
    let mut out = lines.join("\n");
    if trailing && !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Validate every section, then build the whole plan. Nothing reaches the caller
/// unless all of it succeeds: a half-applied patch is worse than a rejected one.
pub fn apply(patch: &Patch, files: &Files<'_>) -> Result<Plan, Error> {
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

    // Registers fill from original content, so a move reads the same bytes
    // whether its CUT and PUT sit in one section or in two files.
    let mut registers: HashMap<Option<String>, Vec<String>> = HashMap::new();
    for section in &patch.sections {
        let content = files[section.path.as_str()];
        let (lines, _) = split(content);
        for op in &section.ops {
            if let Op::Cut {
                start,
                end,
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
    for section in &patch.sections {
        plan.changes
            .push(build(section, files[section.path.as_str()], &registers)?);
    }
    Ok(plan)
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

fn resolve(
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
    content: &str,
    registers: &HashMap<Option<String>, Vec<String>>,
) -> Result<Change, Error> {
    let has_remove = section.ops.iter().any(|o| matches!(o, Op::Remove));
    if has_remove {
        if section.ops.len() > 1 {
            return Err(Error::RemoveWithOps {
                path: section.path.clone(),
            });
        }
        return Ok(Change::Remove {
            path: section.path.clone(),
        });
    }

    let (lines, trailing) = split(content);
    let len = lines.len();

    let mut spans: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut before: HashMap<usize, Vec<String>> = HashMap::new();
    let mut after: HashMap<usize, Vec<String>> = HashMap::new();
    let mut dest: Option<&str> = None;

    for op in &section.ops {
        match op {
            Op::Replace { start, end, body } => {
                bounds(section, *start, *end, len)?;
                spans.push((*start, *end, resolve(body, registers)?));
            }
            Op::Cut { start, end, .. } => spans.push((*start, *end, Vec::new())),
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
                    .extend(resolve(body, registers)?);
            }
            Op::InsertAfter { at, body } => {
                let n = match at {
                    LinePos::Tail => len,
                    LinePos::At(n) => {
                        bounds(section, *n, *n, len)?;
                        *n
                    }
                };
                after
                    .entry(n)
                    .or_default()
                    .extend(resolve(body, registers)?);
            }
            Op::Move { dest: d } => dest = Some(d),
            Op::Remove => unreachable!("handled above"),
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
    // span is gone; dropping it silently would apply a patch the model did not
    // write.
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
    let record = |out: &mut Vec<String>, body: Vec<String>, landed: &mut Vec<Landed>| {
        if body.is_empty() {
            return;
        }
        let start = out.len() + 1;
        out.extend(body);
        landed.push(Landed {
            start,
            end: out.len(),
        });
    };

    if len == 0 {
        record(&mut out, before.remove(&1).unwrap_or_default(), &mut landed);
    }
    let mut i = 1;
    while i <= len {
        record(&mut out, before.remove(&i).unwrap_or_default(), &mut landed);
        // Ranges name original lines, so the cursor jumps past the whole span
        // and later hunks keep their pre-patch numbering.
        match spans.iter().find(|(start, _, _)| *start == i) {
            Some((_, end, body)) => {
                record(&mut out, body.clone(), &mut landed);
                record(&mut out, after.remove(end).unwrap_or_default(), &mut landed);
                i = end + 1;
            }
            None => {
                out.push(lines[i - 1].to_string());
                record(&mut out, after.remove(&i).unwrap_or_default(), &mut landed);
                i += 1;
            }
        }
    }
    if len == 0 {
        record(&mut out, after.remove(&0).unwrap_or_default(), &mut landed);
    }

    let content = join(&out, trailing);
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
