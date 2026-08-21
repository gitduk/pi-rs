use crate::{Body, Error, LinePos, Op, Patch, Section};

/// Where a body row must start. A model reaching for unified-diff habits writes
/// `-old` or a bare context line; both are rejected by name.
const ROW: char = '+';

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(s)
}

/// `N.=M` → (N, M). A single line is written `N.=N`.
fn range(spec: &str, line: usize) -> Result<(usize, usize), Error> {
    let (a, b) = spec.split_once(".=").ok_or_else(|| Error::Syntax {
        line,
        what: format!("`{spec}` is not a range; write `N.=M`, or `N.=N` for one line"),
    })?;
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

/// Split `PUT <target><rest>` into the target and whatever follows it.
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
                    what: "a `+` row must follow a header ending in `:`".into(),
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
            let (spec, tail) = split_target(rest.trim());
            let (start, end) = range(spec, no)?;
            Op::Cut {
                start,
                end,
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
                what: "this is not a unified diff: name the lines to delete in the range and \
                       give only the replacement as `+` rows"
                    .into(),
            });
        } else {
            // A bare range or target is the verb-less slip a model makes most;
            // naming the repair costs a token and saves a turn.
            let hint = if line.starts_with(['<', '>']) || line.contains(".=") {
                format!(" — did you mean `PUT {line}`?")
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
        } else {
            LinePos::At(t.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
                Error::Syntax {
                    line: no,
                    what: format!("`>{t}` needs a line number or `$` for the file tail"),
                }
            })?)
        };
        return Ok(Op::InsertAfter {
            at,
            body: placeholder(),
        });
    }
    if spec.contains('*') {
        return Err(Error::Syntax {
            line: no,
            what: "block ops (`N*`) are not supported yet; name the lines with `N.=M`".into(),
        });
    }
    let (start, end) = range(spec, no)?;
    Ok(Op::Replace {
        start,
        end,
        body: placeholder(),
    })
}
