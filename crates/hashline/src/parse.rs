use crate::{Body, Error, LinePos, Op, Patch, Section, Target};

/// Where a body row must start. A model reaching for unified-diff habits writes
/// `-old` or a bare context line; both are rejected by name.
const ROW: char = '+';

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(s)
}

/// `N.=M` → (N, M), and a bare `N` → (N, N).
///
/// The short form is accepted because it has exactly one reading and models
/// keep reaching for it. Refusing it taught nothing and cost a turn each time;
/// `N.=N` stays the form the documentation gives.
///
/// `verb` is the op the range belongs to. Without it a patch whose `PUT` and
/// `CUT` are both malformed draws the same complaint twice: the model corrects
/// the first, the identical message comes back about the second, and nothing it
/// can see says the fix landed.
fn range(spec: &str, line: usize, verb: &str) -> Result<(usize, usize), Error> {
    let Some((a, b)) = spec.split_once(".=") else {
        // Zero parses; it just is not a line. Telling the model to write the
        // form it already wrote sends it looking in the wrong place.
        let what = if spec.trim().parse::<usize>() == Ok(0) {
            format!("`{verb} {}`: lines are numbered from 1", spec.trim())
        } else {
            format!(
                "`{verb} {spec}` is not a range; write `{verb} N.=M`, \
                 or `{verb} N` for one line"
            )
        };
        let n = number(spec, line, &what)?;
        return Ok((n, n));
    };
    let parse = |t: &str| -> Result<usize, Error> {
        t.trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Syntax {
                line,
                what: format!("`{}` is not a line number", t.trim()),
            })
    };
    let (a, b) = (parse(a)?, parse(b)?);
    if a > b {
        return Err(Error::Syntax {
            line,
            what: format!("range {a}.={b} runs backwards"),
        });
    }
    Ok((a, b))
}

fn register(rest: &str, line: usize) -> Result<Option<String>, Error> {
    let rest = rest.trim();
    if rest.is_empty() {
        return Ok(None);
    }
    match rest.strip_prefix('@') {
        Some(name) if !name.is_empty() => Ok(Some(name.to_string())),
        _ => Err(Error::Syntax {
            line,
            what: format!("expected `@name` or nothing, got `{rest}`"),
        }),
    }
}

/// Why the text before `*` is not a line number.
///
/// Said in the model's own terms where it can be: `N.*` is the two target forms
/// crossed, and a complaint that the line number is missing sends the model
/// looking at the one part it got right.
fn not_a_block(prefix: &str, spec: &str) -> String {
    let head = spec.trim_end_matches('*').trim();
    let bare = head.trim_end_matches('.');
    match bare.parse::<usize>() {
        Ok(0) => format!("`{prefix}{spec}`: lines are numbered from 1"),
        Ok(_) if bare != head => format!(
            "`{prefix}{spec}` has a stray `.`: a whole construct is \
             `{prefix}{bare}*`, and `.` only separates the ends of an `N.=M` range"
        ),
        _ => format!("`{prefix}{spec}` needs a line number before `*`"),
    }
}

/// `N*` or `N.=M`.
fn target(spec: &str, line: usize, verb: &str) -> Result<Target, Error> {
    match spec.strip_suffix('*') {
        Some(n) => {
            let n = n
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| Error::Syntax {
                    line,
                    what: not_a_block("", spec),
                })?;
            Ok(Target::Block { line: n })
        }
        None => {
            let (start, end) = range(spec, line, verb)?;
            Ok(Target::Range { start, end })
        }
    }
}

/// Split `PUT <target><rest>` into the target and whatever follows it.
/// Said when a body row sits under an op that takes none. A row in the wrong
/// place is a different mistake from a row of the wrong shape, and the model
/// that wrote each needs a different sentence.
const NO_BODY: &str = "`CUT`, `REM` and `MV` take no body rows: the range names what goes \
                       and nothing arrives. To write new content, use `PUT N.=M:` with \
                       `+` rows.";

/// Whether the op just parsed is one of those.
fn bodyless_before(sections: &[Section]) -> bool {
    matches!(
        sections.last().and_then(|s| s.ops.last()),
        Some(Op::Cut { .. } | Op::Remove | Op::Move { .. })
    )
}

fn split_target(spec: &str) -> (&str, &str) {
    match spec.find(|c: char| c.is_whitespace() || c == '@') {
        Some(i) => (&spec[..i], &spec[i..]),
        None => (spec, ""),
    }
}

pub fn parse(input: &str) -> Result<Patch, Error> {
    let mut sections: Vec<Section> = Vec::new();
    let mut pending: Option<(usize, Body)> = None;

    for (i, raw) in input.lines().enumerate() {
        let no = i + 1;

        if let Some(row) = raw.strip_prefix(ROW) {
            let Some((op_index, Body::Lines(lines))) = pending.as_mut() else {
                return Err(Error::Syntax {
                    line: no,
                    what: if bodyless_before(&sections) {
                        NO_BODY.into()
                    } else {
                        "a `+` row must follow a header ending in `:`".into()
                    },
                });
            };
            let _ = op_index;
            lines.push(row.to_string());
            continue;
        }

        if raw.trim().is_empty() {
            continue;
        }

        // A row that is not `+` closes whatever body was open.
        if let Some((op_index, body)) = pending.take() {
            let section = sections.last_mut().expect("a body implies a section");
            match &mut section.ops[op_index] {
                Op::Replace { body: slot, .. }
                | Op::InsertBefore { body: slot, .. }
                | Op::InsertAfter { body: slot, .. } => *slot = body,
                _ => unreachable!("only body-taking ops open a body"),
            }
        }

        let line = raw.trim();

        if let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let (path, tag) = inner.rsplit_once('#').ok_or_else(|| Error::Syntax {
                line: no,
                what: "a section header is `[path#TAG]`; the TAG comes from the last read".into(),
            })?;
            if path.is_empty() {
                return Err(Error::Syntax {
                    line: no,
                    what: "empty path in section header".into(),
                });
            }
            sections.push(Section {
                path: path.to_string(),
                tag: tag.to_ascii_uppercase(),
                ops: Vec::new(),
                line: no,
            });
            continue;
        }

        let Some(section) = sections.last_mut() else {
            return Err(Error::Syntax {
                line: no,
                what: format!("`{line}` came before any `[path#TAG]` header"),
            });
        };

        let op = if let Some(rest) = line.strip_prefix("PUT ") {
            parse_put(rest.trim(), no)?
        } else if let Some(rest) = line.strip_prefix("CUT ") {
            // A `:` introduces a body and `CUT` has none, so one written here
            // is noise rather than a second meaning. Tolerated, so that the
            // complaint lands on the real mistake — which, whenever a `:`
            // shows up on a `CUT`, is the body row the model wrote under it.
            let rest = rest.trim();
            let (spec, tail) = split_target(rest.strip_suffix(':').unwrap_or(rest));
            Op::Cut {
                target: target(spec, no, "CUT")?,
                register: register(tail, no)?,
            }
        } else if line == "REM" {
            Op::Remove
        } else if let Some(dest) = line.strip_prefix("MV ") {
            let dest = strip_quotes(dest);
            if dest.is_empty() {
                return Err(Error::Syntax {
                    line: no,
                    what: "MV needs a destination".into(),
                });
            }
            Op::Move {
                dest: dest.to_string(),
            }
        } else if line.starts_with('-') {
            return Err(Error::Syntax {
                line: no,
                what: if bodyless_before(&sections) {
                    NO_BODY.into()
                } else {
                    // The escape hatch matters as much as the rule: a Markdown
                    // bullet is a literal line that starts with `-`, and a
                    // model told only that `-` is invalid has nowhere to put
                    // one.
                    "`-` rows are not valid: the range already names the lines that go, \
                     and the body is only what arrives. To delete lines and put nothing \
                     back, use `CUT N.=M`. For a literal line that starts with `-`, \
                     prefix it: `+- item`."
                        .into()
                },
            });
        } else {
            // A bare range or target is the verb-less slip a model makes most;
            // naming the repair costs a token and saves a turn.
            let numbered = line
                .split_once(':')
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
            let hint = if line.starts_with(['<', '>']) || line.contains(".=") {
                format!(" — did you mean `PUT {line}`?")
            } else if numbered {
                // A line pasted straight out of read or grep output.
                " — that is a line from a read, not an op. Name it in a range \
                 (`PUT N.=N:`) or a block (`PUT N*:`), and put the new text in \
                 `+` rows."
                    .into()
            } else {
                String::new()
            };
            return Err(Error::Syntax {
                line: no,
                what: format!(
                    "`{line}` is not an op; every op line starts with PUT, CUT, MV or REM{hint}"
                ),
            });
        };

        let takes_body = matches!(
            op,
            Op::Replace {
                body: Body::Lines(_),
                ..
            } | Op::InsertBefore {
                body: Body::Lines(_),
                ..
            } | Op::InsertAfter {
                body: Body::Lines(_),
                ..
            }
        );
        section.ops.push(op);
        if takes_body {
            pending = Some((section.ops.len() - 1, Body::Lines(Vec::new())));
        }
    }

    if let Some((op_index, body)) = pending.take() {
        let section = sections.last_mut().expect("a body implies a section");
        match &mut section.ops[op_index] {
            Op::Replace { body: slot, .. }
            | Op::InsertBefore { body: slot, .. }
            | Op::InsertAfter { body: slot, .. } => *slot = body,
            _ => unreachable!("only body-taking ops open a body"),
        }
    }

    if sections.is_empty() {
        return Err(Error::Syntax {
            line: 1,
            what: "the patch is empty".into(),
        });
    }
    Ok(Patch { sections })
}

fn number(text: &str, line: usize, what: &str) -> Result<usize, Error> {
    text.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Syntax {
            line,
            what: what.to_string(),
        })
}

fn parse_put(rest: &str, no: usize) -> Result<Op, Error> {
    // `:` marks a header whose content arrives as `+` rows; without it the
    // content comes from a register.
    let (spec, body) = match rest.strip_suffix(':') {
        Some(spec) => (spec.trim(), None),
        None => {
            let (spec, tail) = split_target(rest);
            (spec, Some(Body::Register(register(tail, no)?)))
        }
    };

    let placeholder = || body.clone().unwrap_or(Body::Lines(Vec::new()));

    if let Some(t) = spec.strip_prefix('<') {
        let line = t
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Syntax {
                line: no,
                what: format!("`<{t}` needs a line number, e.g. `<1`"),
            })?;
        return Ok(Op::InsertBefore {
            line,
            body: placeholder(),
        });
    }
    if let Some(t) = spec.strip_prefix('>') {
        let t = t.trim();
        let at = if t == "$" {
            LinePos::Tail
        } else if let Some(n) = t.strip_suffix('*') {
            LinePos::AfterBlock(number(n, no, &not_a_block(">", t))?)
        } else {
            LinePos::At(number(
                t,
                no,
                "`>N` needs a line number or `$` for the file tail",
            )?)
        };
        return Ok(Op::InsertAfter {
            at,
            body: placeholder(),
        });
    }
    Ok(Op::Replace {
        target: target(spec, no, "PUT")?,
        body: placeholder(),
    })
}
