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

/// One way to write an address, and what it names.
///
/// The one place the grammar is listed. The refusal below is built from it, the
/// tool description the model reads is built from it, and the tests walk it —
/// so a form cannot exist in one of those and be missing from the others.
///
/// `addr` still matches the suffixes itself rather than looping here, and that
/// is deliberate: each malformed form has its own sentence (`runs backwards`,
/// `needs both ends`, `not a line number`), and a table of constructors
/// returning `Option` would collapse all of them into one useless complaint.
/// The table owns the enumeration; the parser owns the diagnosis. What ties
/// them is `every_form_the_table_lists_is_one_the_parser_takes`.
pub struct Form {
    /// The suffix after the line number, with `M` standing for a second one.
    pub suffix: &'static str,
    /// What it names, for the model.
    pub means: &'static str,
}

pub const FORMS: &[Form] = &[
    Form {
        suffix: "-M",
        means: "lines N through M, inclusive. `3-3` is one line.",
    },
    Form {
        suffix: "<",
        means: "the gap above N. `1<` is the file head.",
    },
    Form {
        suffix: ">",
        means: "the gap below N. The last line's is the file tail.",
    },
    Form {
        suffix: "*",
        means: "the whole construct opening at N; its closing line is resolved \
                for you. Any decorator, attribute or doc comment above it \
                belongs to it: naming either their rows or N covers them all.",
    },
    Form {
        suffix: "*>",
        means: "the gap below where that construct closes.",
    },
];

/// What an address looks like, said whenever one does not.
///
/// One sentence answers every malformed address there is, including every
/// spelling this grammar used to accept: those are not special cases needing
/// their own advice, they are simply not addresses, and naming the forms says
/// so along with everything else.
fn forms() -> String {
    let spelt: Vec<String> = FORMS.iter().map(|f| format!("`N{}`", f.suffix)).collect();
    let (last, rest) = spelt.split_last().map_or(("", &[][..]), |(l, r)| (l, r));
    format!(
        "an address is a line number and a suffix — {} or {last}",
        rest.join(", ")
    )
}

/// One address, whatever shape it has.
///
/// Every form is a line number and a suffix, so the number is always first and
/// the suffix always says what to do with it. A bare number is not an address:
/// it is what each form looks like with the suffix left off, and accepting it
/// would make an omission mean `N-N` — which for a dropped `<` or `>` replaces
/// the line the model meant to insert beside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addr {
    /// `N-M` or `N*` — lines that exist.
    Target(Target),
    /// `N<` — the gap above N. `1<` is the file head, and the only address an
    /// empty file has.
    Before(usize),
    /// `N>` or `N*>` — the gap below a line, or below where a construct closes.
    After(LinePos),
}

impl Addr {
    /// Read one back with no op and no patch line to blame it on.
    ///
    /// For a caller asking only whether a string is an address — a view
    /// deciding whether a prefix it is looking at is one of its own.
    pub fn read(spec: &str) -> Option<Addr> {
        addr(spec, 0, "").ok()
    }
}

impl std::fmt::Display for Addr {
    /// The inverse of `addr`, so a view that prints an address and the parser
    /// that reads it back cannot drift into two grammars.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Addr::Target(Target::Range { start, end }) => write!(f, "{start}-{end}"),
            Addr::Target(Target::Block { line }) => write!(f, "{line}*"),
            Addr::Before(n) => write!(f, "{n}<"),
            Addr::After(LinePos::At(n)) => write!(f, "{n}>"),
            Addr::After(LinePos::AfterBlock(n)) => write!(f, "{n}*>"),
        }
    }
}

/// `verb` is the op the address belongs to. Without it a patch whose `PUT` and
/// `CUT` are both malformed draws the same complaint twice: the model corrects
/// the first, the identical message comes back about the second, and nothing it
/// can see says the fix landed.
fn addr(spec: &str, line: usize, verb: &str) -> Result<Addr, Error> {
    let spec = spec.trim();
    let bad = |what: String| Error::Syntax { line, what };
    // Digits are one byte each, so the first non-digit is a char boundary.
    let split = spec
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(spec.len());
    let (head, rest) = spec.split_at(split);

    let n = match head.parse::<usize>() {
        Ok(0) => return Err(bad(format!("`{verb} {spec}`: lines are numbered from 1"))),
        Ok(n) => n,
        Err(_) => return Err(bad(format!("`{verb} {spec}`: {}", forms()))),
    };

    match rest {
        "<" => Ok(Addr::Before(n)),
        ">" => Ok(Addr::After(LinePos::At(n))),
        "*" => Ok(Addr::Target(Target::Block { line: n })),
        "*>" => Ok(Addr::After(LinePos::AfterBlock(n))),
        _ => match rest.strip_prefix('-') {
            None => Err(bad(format!("`{verb} {spec}`: {}", forms()))),
            Some(m) if m.trim().is_empty() => Err(bad(format!(
                "`{verb} {spec}`: a range needs both ends, as in `{n}-{n}`"
            ))),
            Some(m) => {
                let what = format!("`{verb} {spec}`: `{m}` is not a line number");
                let m = number(m, line, &what)?;
                if n > m {
                    return Err(bad(format!("`{verb} {spec}` runs backwards")));
                }
                Ok(Addr::Target(Target::Range { start: n, end: m }))
            }
        },
    }
}

/// The addresses that name existing lines. `CUT` takes only these: a gap holds
/// nothing to cut.
fn target(spec: &str, line: usize, verb: &str) -> Result<Target, Error> {
    let instead = match addr(spec, line, verb)? {
        Addr::Target(target) => return Ok(target),
        Addr::Before(n) | Addr::After(LinePos::At(n)) => format!("{n}-{n}"),
        Addr::After(LinePos::AfterBlock(n)) => format!("{n}*"),
    };
    Err(Error::Syntax {
        line,
        what: format!(
            "`{verb} {spec}` names a gap between lines, and {verb} needs lines. \
             Did you mean `{verb} {instead}`?"
        ),
    })
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
/// Said when a body row sits under an op that takes none. A row in the wrong
/// place is a different mistake from a row of the wrong shape, and the model
/// that wrote each needs a different sentence.
const NO_BODY: &str = "`CUT`, `REM` and `MV` take no body rows: the range names what goes \
                       and nothing arrives. To write new content, use `PUT N-M:` with \
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
                    "a `-` at the start of a row is not a deletion: the address already \
                     names the lines that go, \
                     and the body is only what arrives. To delete lines and put nothing \
                     back, use `CUT N-M`. For a literal line that starts with `-`, \
                     prefix it: `+- item`."
                        .into()
                },
            });
        } else {
            // Two slips, told apart by what follows the colon: an address on
            // its own is an op that lost its verb, an address with the line's
            // own text after it is a row pasted straight out of a view.
            let (head, after) = line.split_once(':').unwrap_or((line, ""));
            let hint = if Addr::read(split_target(head).0).is_some() && after.trim().is_empty() {
                format!(" — did you mean `PUT {line}`?")
            } else if head.starts_with(|c: char| c.is_ascii_digit()) {
                // Any digit, not this grammar's addresses: recognising a
                // mistake is not accepting an address, and a recogniser tied to
                // the grammar stops firing the next time the grammar moves —
                // which is exactly what happened to the one this replaces.
                " — that is a line from a read, not an op. Name it in a range \
                 (`PUT N-N:`) or a block (`PUT N*:`), and put the new text in \
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

    let body = body.unwrap_or(Body::Lines(Vec::new()));

    Ok(match addr(spec, no, "PUT")? {
        Addr::Before(line) => Op::InsertBefore { line, body },
        Addr::After(at) => Op::InsertAfter { at, body },
        Addr::Target(target) => Op::Replace { target, body },
    })
}
