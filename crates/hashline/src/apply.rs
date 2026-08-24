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
        let (lines, _) = split(content);
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

/// Turn every `N*` into the range it names. A construct that cannot be found is
/// an error, never a guess: guessing here rewrites code nobody looked at.
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

    let (lines, trailing) = split(content);
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
        |out: &mut Vec<String>, body: Vec<String>, took: Vec<String>, landed: &mut Vec<Landed>| {
            if body.is_empty() && took.is_empty() {
                return;
            }
            let start = out.len() + 1;
            out.extend(body);
            landed.push(Landed {
                start,
                end: out.len(),
                took,
            });
        };

    if len == 0 {
        record(
            &mut out,
            before.remove(&1).unwrap_or_default(),
            Vec::new(),
            &mut landed,
        );
    }
    let mut i = 1;
    while i <= len {
        record(
            &mut out,
            before.remove(&i).unwrap_or_default(),
            Vec::new(),
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
                record(&mut out, body.clone(), took, &mut landed);
                record(
                    &mut out,
                    after.remove(end).unwrap_or_default(),
                    Vec::new(),
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
            &mut landed,
        );
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
