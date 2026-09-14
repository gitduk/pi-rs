use std::collections::HashMap;

use crate::{Blocks, Error, Files, Mark, Patch, Row, Section};
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    /// Where the new lines sit in the file now.
    pub start: usize,
    pub end: usize,
    /// The original lines it displaced, in order. Empty for an insertion.
    pub took: Vec<String>,
    /// The 1-based line this hunk acted on in the *original* file.
    pub took_at: usize,
}

impl Landed {
    /// How many lines this put in the file.
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

// One lowered operation: the resolver's answer, in plain line ranges.
enum Op {
    Replace {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    InsertBefore {
        line: usize,
        body: Vec<String>,
    },
}

// Validate every section, then build the whole plan. Nothing reaches the
// caller unless all of it succeeds: a half-applied patch is worse than a
// rejected one.
//
// Sections for one path stack: each resolves against what the earlier ones
// left, and the plan carries a single Write per path. Sections built off the
// same original would each land alone on disk, the later overwriting the
// earlier with no error to show for it.
pub fn apply(patch: &Patch, files: &Files<'_>, blocks: &dyn Blocks) -> Result<Plan, Error> {
    let mut plan = Plan::default();
    // Where each path's Write sits in the plan.
    let mut slot: HashMap<String, usize> = HashMap::new();
    for section in &patch.sections {
        let at = *slot
            .entry(section.path.clone())
            .or_insert(plan.changes.len());
        let content = match plan.changes.get(at) {
            Some(Change::Write { content, .. }) => content.as_str(),
            _ => *files
                .get(section.path.as_str())
                .ok_or_else(|| Error::Missing {
                    path: section.path.clone(),
                })?,
        };
        let ops = resolve(section, content, blocks)?;
        let change = build(&section.path, &ops, content, blocks)?;
        if at < plan.changes.len() {
            let Change::Write {
                content, landed, ..
            } = change;
            let Change::Write {
                content: kept,
                landed: kept_landed,
                ..
            } = &mut plan.changes[at];
            *kept = content;
            kept_landed.extend(landed);
        } else {
            plan.changes.push(change);
        }
    }
    Ok(plan)
}

// Turn every group into lowered ops. Nothing is guessed: an anchor that
// matches nowhere, or in more than one place, rejects the patch before any
// edit is built.
fn resolve(section: &Section, content: &str, blocks: &dyn Blocks) -> Result<Vec<Op>, Error> {
    let lines: Vec<&str> = content.lines().collect();
    let len = lines.len();
    let mut ops = Vec::new();
    if len == 0 {
        return Err(Error::Syntax {
            line: section.line,
            what: "the file is empty — nothing to anchor on; create it with the \
                   write tool instead"
                .into(),
        });
    }

    for group in &section.groups {
        // A `*` row is a point anchor: `+` rows above it insert before the
        // construct, `+` rows below it insert after — a sandwich does both.
        if let Some(ix) = group.iter().position(|r| matches!(r, Row::Star(..))) {
            let star = match &group[ix] {
                Row::Star(s) => s.as_str(),
                _ => unreachable!("validated"),
            };
            let (cs, ce) = construct(section, star, 1, len, content, &lines, blocks)?;
            let add = |r: &Row| match r {
                Row::Mark(Mark::Add, t) => t.clone(),
                _ => unreachable!("validated"),
            };
            let before: Vec<String> = group[..ix].iter().map(add).collect();
            let after: Vec<String> = group[ix + 1..].iter().map(add).collect();
            if !before.is_empty() {
                ops.push(Op::InsertBefore {
                    line: cs,
                    body: before,
                });
            }
            if !after.is_empty() {
                ops.push(Op::InsertBefore {
                    line: ce + 1,
                    body: after,
                });
            }
            continue;
        }

        // Leading `@` rows are scopes; the rest is the operation.
        let mut scopes: Vec<&str> = Vec::new();
        let mut idx = 0usize;
        while let Some(Row::At(s)) = group.get(idx) {
            scopes.push(s.as_str());
            idx += 1;
        }
        let marks: Vec<(&Mark, &str)> = group[idx..]
            .iter()
            .map(|r| match r {
                Row::Mark(m, t) => (m, t.as_str()),
                _ => unreachable!("validated"),
            })
            .collect();

        // Pure `+` under a single `@`: the construct is replaced whole. The
        // outline names it, the adds become its body — the one edit an
        // outline-driven change needs without any read.
        if marks.iter().all(|(m, _)| matches!(m, Mark::Add)) {
            let (scope, adds) = match (scopes.first(), scopes.len()) {
                (Some(s), 1) => (*s, marks.iter().map(|(_, t)| t.to_string()).collect()),
                _ => {
                    return Err(Error::Syntax {
                        line: section.line,
                        what: "an operation made only of `+` rows needs exactly one \
                               `@` scope to anchor it, or an `=` row beside it"
                            .into(),
                    });
                }
            };
            let (start, end) = construct(section, scope, 1, len, content, &lines, blocks)?;
            ops.push(Op::Replace {
                start,
                end,
                body: adds,
            });
            continue;
        }

        // Only `=` rows keep what is already there: nothing changes, which
        // is almost always a forgotten `-` or `+` row.
        if marks.iter().all(|(m, _)| matches!(m, Mark::Keep)) {
            return Err(Error::Syntax {
                line: section.line,
                what: "an operation of only `=` rows changes nothing — add `-` or \
                       `+` rows, or drop the operation"
                    .into(),
            });
        }

        // The search window: nested scopes narrow it, innermost wins.
        let (mut lo, mut hi) = (1usize, len);
        for scope in &scopes {
            let (s, e) = construct(section, scope, lo, hi, content, &lines, blocks)?;
            lo = s;
            hi = e;
        }

        // A lone `-` row is a sweep: every row in the window containing its
        // text goes — construct-wide under a scope, file-wide without one.
        // A bare `-` (nothing to contain) under a scope deletes the whole
        // construct; with no scope it has nothing to mean.
        if let [Row::Mark(Mark::Del, pat)] = &group[idx..] {
            let needle = pat.trim();
            if needle.is_empty() {
                if scopes.is_empty() {
                    return Err(Error::Syntax {
                        line: section.line,
                        what: "a bare `-` deletes a whole construct — name it with an \
                               `@` scope, or anchor a blank row with `=` rows"
                            .into(),
                    });
                }
                ops.push(Op::Replace {
                    start: lo,
                    end: hi,
                    body: Vec::new(),
                });
                continue;
            }
            let hits: Vec<usize> = (lo..=hi)
                .filter(|n| lines.get(n - 1).is_some_and(|l| l.contains(needle)))
                .collect();
            if hits.is_empty() {
                let (line, text) = nearest(&lines, &[needle], lo, hi);
                return Err(Error::NoMatch {
                    path: section.path.clone(),
                    line,
                    text: crop(&text, 80),
                });
            }
            let mut runs: Vec<(usize, usize)> = Vec::new();
            for n in hits {
                match runs.last_mut() {
                    Some((_, e)) if *e + 1 == n => *e = n,
                    _ => runs.push((n, n)),
                }
            }
            for (s, e) in runs {
                ops.push(Op::Replace {
                    start: s,
                    end: e,
                    body: Vec::new(),
                });
            }
            continue;
        }

        // The delete/keep rows match the file contiguously; add rows do not
        // consume a row, they arrive between the ones that do.
        let seq: Vec<&str> = marks
            .iter()
            .filter(|(m, _)| matches!(m, Mark::Del | Mark::Keep))
            .map(|(_, t)| *t)
            .collect();
        let k = seq.len();
        let starts: Vec<usize> = if k > hi - lo + 1 {
            Vec::new()
        } else {
            (lo..=hi + 1 - k)
                .filter(|s| (0..k).all(|i| lines[s - 1 + i].trim() == seq[i].trim()))
                .collect()
        };
        match starts.len() {
            0 => {
                let (line, text) = nearest(&lines, &seq, lo, hi);
                return Err(Error::NoMatch {
                    path: section.path.clone(),
                    line,
                    text: crop(&text, 80),
                });
            }
            1 => {}
            n => {
                // Preceding rows join in until they tell the candidates apart:
                // identical neighbours are exactly when `@` and one `=` row
                // both fail.
                let above = |s: usize, d: usize| (s > d).then(|| lines[s - d - 1]);
                let mut depth = 1;
                while depth < 3
                    && starts
                        .iter()
                        .all(|s| above(*s, depth) == above(starts[0], depth))
                {
                    depth += 1;
                }
                let detail = starts
                    .iter()
                    .map(|s| {
                        let in_construct = match covering(&section.path, *s, content, blocks) {
                            Some(row) => format!(" in `{}`", crop(&row, 60)),
                            None => String::new(),
                        };
                        let mut before = String::new();
                        for d in 1..=depth {
                            if let Some(row) = above(*s, d) {
                                let label = if d == 1 { "preceded by" } else { "before that" };
                                before.push_str(&format!(", {label} `{}`", crop(row, 60)));
                            }
                        }
                        format!("  lines {}-{}{in_construct}{before}", s, s + k - 1)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                return Err(Error::Ambiguous {
                    path: section.path.clone(),
                    n,
                    detail,
                });
            }
        }
        let s0 = starts[0];
        let e0 = s0 + k - 1;

        // Walk the marked rows: keep emits the file's line, delete skips it,
        // add emits new content at its own position.
        let mut body: Vec<String> = Vec::new();
        let mut leading: Vec<String> = Vec::new();
        let mut fi = s0;
        let mut seen_core = false;
        for (m, t) in &marks {
            match m {
                Mark::Add if !seen_core => leading.push(t.to_string()),
                Mark::Add => body.push(t.to_string()),
                Mark::Keep => {
                    seen_core = true;
                    body.push(lines[fi - 1].to_string());
                    fi += 1;
                }
                Mark::Del => {
                    seen_core = true;
                    fi += 1;
                }
            }
        }
        if !leading.is_empty() {
            ops.push(Op::InsertBefore {
                line: s0,
                body: std::mem::take(&mut leading),
            });
        }
        ops.push(Op::Replace {
            start: s0,
            end: e0,
            body,
        });
    }
    Ok(ops)
}

// The construct a `*` row names: a unique opening line matched by prefix
// within the window, and the parser's answer for where it closes.
fn construct(
    section: &Section,
    star: &str,
    lo: usize,
    hi: usize,
    content: &str,
    lines: &[&str],
    blocks: &dyn Blocks,
) -> Result<(usize, usize), Error> {
    let in_window: Vec<usize> = blocks
        .openings(&section.path, content)
        .into_iter()
        .filter(|n| *n >= lo && *n <= hi)
        .collect();
    let cands: Vec<usize> = in_window
        .iter()
        .copied()
        .filter(|n| {
            lines
                .get(n - 1)
                .is_some_and(|l| l.trim().starts_with(star.trim()))
        })
        .collect();
    match cands.len() {
        0 => {
            // The fix is copying one of these into the `@` row, so the
            // refusal hands over the window's actual openings.
            let mut available: Vec<&str> = in_window
                .iter()
                .map(|n| lines.get(n - 1).copied().unwrap_or("").trim())
                .collect();
            available.dedup();
            let mut names: Vec<String> = available
                .iter()
                .take(5)
                .map(|r| format!("`{}`", crop(r, 60)))
                .collect();
            if available.len() > 5 {
                names.push(format!("… +{}", available.len() - 5));
            }
            let hint = if names.is_empty() {
                String::new()
            } else {
                format!(" — the window opens: {}", names.join(", "))
            };
            Err(Error::NoConstruct {
                path: section.path.clone(),
                what: format!("no construct opens with `{star}` in lines {lo}-{hi}{hint}"),
            })
        }
        1 => blocks
            .extent_of(&section.path, content, cands[0])
            .ok_or_else(|| Error::NoConstruct {
                path: section.path.clone(),
                what: format!(
                    "the row the anchor names (line {}) opens no resolvable construct",
                    cands[0]
                ),
            }),
        n => Err(Error::NoConstruct {
            path: section.path.clone(),
            what: format!(
                "`{star}` opens {n} constructs in lines {lo}-{hi} — add an outer \
                 `@` scope or more of the opening line"
            ),
        }),
    }
}

// The construct a candidate span sits in, innermost wins: the annotated
// opening row is exactly what the model's `@` fix copies.
fn covering(path: &str, at: usize, content: &str, blocks: &dyn Blocks) -> Option<String> {
    blocks
        .openings(path, content)
        .into_iter()
        .filter_map(|o| blocks.extent_of(path, content, o))
        .filter(|(s, e)| *s <= at && at <= *e)
        .min_by_key(|(s, e)| e - s)
        .map(|(s, _)| content.lines().nth(s - 1).unwrap_or("").trim().to_string())
}

// The closest real row to the marked rows, so a refusal can point at it
// instead of leaving the model to hunt. Exact match would have hit; this is
// char-overlap against the best-matching marked row.
fn nearest(lines: &[&str], seq: &[&str], lo: usize, hi: usize) -> (usize, String) {
    let mut best = (lo, lines.get(lo - 1).copied().unwrap_or("").to_string());
    let mut best_score = -1.0f64;
    for n in lo..=hi {
        let row = lines.get(n - 1).copied().unwrap_or("");
        let score = seq.iter().map(|s| sim(row, s)).fold(-1.0f64, f64::max);
        if score > best_score {
            best_score = score;
            best = (n, row.to_string());
        }
    }
    best
}

// Counted common characters over the longer of the two rows.
fn sim(a: &str, b: &str) -> f64 {
    let mut ca: Vec<char> = a.chars().collect();
    let mut cb: Vec<char> = b.chars().collect();
    ca.sort_unstable();
    cb.sort_unstable();
    let (mut i, mut j, mut common) = (0usize, 0usize, 0usize);
    while i < ca.len() && j < cb.len() {
        match ca[i].cmp(&cb[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let denom = ca.len().max(cb.len()).max(1);
    common as f64 / denom as f64
}

fn crop(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut t: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        t.push('…');
    }
    t
}

// The spans lowered into a whole file: sorted, overlap-checked, then swept
// into the new content with the anchored insertions filed around them.
fn build(path: &str, ops: &[Op], content: &str, blocks: &dyn Blocks) -> Result<Change, Error> {
    let (lines, trailing, crlf) = split(content);
    let len = lines.len();

    let mut spans: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut before: HashMap<usize, Vec<String>> = HashMap::new();
    let name = |at: usize| {
        covering(path, at, content, blocks)
            .map_or_else(String::new, |row| format!(" (in `{}`)", crop(&row, 60)))
    };

    for op in ops {
        match op {
            Op::Replace { start, end, body } => spans.push((*start, *end, body.clone())),
            Op::InsertBefore { line, body } => {
                before.entry(*line).or_default().extend(body.clone());
            }
        }
    }

    spans.sort_by_key(|(start, _, _)| *start);
    for pair in spans.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if a.1 >= b.0 {
            return Err(Error::Overlap {
                path: path.to_string(),
                a_start: a.0,
                a_end: a.1,
                b_start: b.0,
                b_end: b.1,
                overlap: b.0,
                a_in: name(a.0),
                b_in: name(b.0),
            });
        }
    }

    // An insertion buried inside a replaced span has no anchor left once the
    // span is gone; dropping it silently would apply a patch nobody wrote.
    for (start, end, _) in &spans {
        let inside_before = before.keys().find(|k| **k > *start && **k <= *end);
        if let Some(k) = inside_before {
            return Err(Error::Overlap {
                path: path.to_string(),
                a_start: *start,
                a_end: *end,
                b_start: *k,
                b_end: *k,
                overlap: *k,
                a_in: name(*start),
                b_in: name(*k),
            });
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(len);
    let mut landed: Vec<Landed> = Vec::new();

    let record = |out: &mut Vec<String>,
                  body: Vec<String>,
                  took: Vec<String>,
                  took_at: usize,
                  landed: &mut Vec<Landed>| {
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

    let mut i = 1;
    while i <= len {
        record(
            &mut out,
            before.remove(&i).unwrap_or_default(),
            Vec::new(),
            i,
            &mut landed,
        );
        match spans.iter().find(|(start, _, _)| *start == i) {
            Some((_, end, body)) => {
                let took: Vec<String> = lines
                    .get(i - 1..*end)
                    .unwrap_or_default()
                    .iter()
                    .map(|l| l.to_string())
                    .collect();
                record(&mut out, body.clone(), took, i, &mut landed);
                i = end + 1;
            }
            None => {
                out.push(lines[i - 1].to_string());
                i += 1;
            }
        }
    }

    // An after-insert targets the row past the file's end — the append form
    // of an insertion, which the loop above never reaches.
    if let Some(body) = before.remove(&(len + 1)) {
        record(&mut out, body, Vec::new(), len + 1, &mut landed);
    }

    Ok(Change::Write {
        path: path.to_string(),
        content: join(&out, trailing, crlf),
        landed,
    })
}

/// A standard unified patch of the changes, for readers that understand one.
/// Built from the hunks the applier already knows rather than a fresh diff:
/// the took/gave rows are the change, and a diff would only re-derive them.
/// Rows are LF; a file's own line ending is applied on write, not here.
///
/// Context lines come from the file's prior content, which `apply` no longer
/// has once a change has been built — the caller passes it in.
pub fn unified_patch(changes: &[Change], before: &HashMap<&str, &str>) -> String {
    let mut out = String::new();
    for change in changes {
        let (path, content, landed) = match change {
            Change::Write {
                path,
                content,
                landed,
            } => (path, content, landed),
        };
        let old = before.get(path.as_str()).copied().unwrap_or_default();
        hunks(&mut out, path, path, old, content, landed);
    }
    out
}

fn changed(l: &Landed, lines: &[&str]) -> bool {
    let gave = lines
        .get(l.start.saturating_sub(1)..l.end)
        .unwrap_or_default();
    l.took.len() != gave.len() || l.took.iter().zip(gave).any(|(t, g)| t != g)
}

fn hunks(
    out: &mut String,
    old_path: &str,
    new_path: &str,
    old_content: &str,
    content: &str,
    landed: &[Landed],
) {
    const CONTEXT: usize = 3;
    let old: Vec<&str> = old_content.lines().collect();
    let new: Vec<&str> = content.lines().collect();
    let changed: Vec<&Landed> = landed.iter().filter(|l| changed(l, &new)).collect();
    if changed.is_empty() {
        return;
    }
    out.push_str(&format!("--- a/{old_path}\n+++ b/{new_path}\n"));
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
        for g in new
            .get(l.start.saturating_sub(1)..l.end)
            .unwrap_or_default()
        {
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
