use crate::{Error, Mark, Patch, Row, Section};

/// Parse the marker dialect: `[path]` sections, blank-line-separated
/// operations, rows of `-`/`=`/`+` marks, `*` construct anchors and `@`
/// construct scopes.
pub fn parse(input: &str) -> Result<Patch, Error> {
    let mut sections: Vec<Section> = Vec::new();
    let mut group: Vec<Row> = Vec::new();
    let mut group_at = 0usize;

    // A group belongs to the section it sits under; a header closes whatever
    // group was open, which is how a section's last operation is flushed.
    let flush = |sections: &mut Vec<Section>, group: &mut Vec<Row>, at| -> Result<(), Error> {
        if !group.is_empty() {
            validate(group, at)?;
            let section = sections.last_mut().ok_or_else(|| Error::Syntax {
                line: at,
                what: "rows came before any `[path]` header".into(),
            })?;
            section.groups.push(std::mem::take(group));
        }
        Ok(())
    };

    for (i, raw) in input.lines().enumerate() {
        let no = i + 1;

        if raw.trim().is_empty() {
            flush(&mut sections, &mut group, group_at)?;
            group_at = 0;
            continue;
        }
        if group.is_empty() {
            group_at = no;
        }

        if let Some(inner) = raw.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if !group.is_empty() {
                return Err(Error::Syntax {
                    line: no,
                    what: "a `[path]` header cannot sit inside an operation — end the \
                           operation with a blank line first"
                        .into(),
                });
            }
            if inner.is_empty() {
                return Err(Error::Syntax {
                    line: no,
                    what: "empty path in section header".into(),
                });
            }
            if let Some(at) = inner.find('#') {
                return Err(Error::Syntax {
                    line: no,
                    what: format!(
                        "a section header is `[path]` — no TAG any more; drop `#{}`",
                        &inner[at..]
                    ),
                });
            }
            sections.push(Section {
                path: inner.to_string(),
                groups: Vec::new(),
                line: no,
            });
            continue;
        }

        let row = match raw.chars().next() {
            Some('-') => Row::Mark(Mark::Del, raw[1..].to_string()),
            Some('=') => Row::Mark(Mark::Keep, raw[1..].to_string()),
            Some('+') => Row::Mark(Mark::Add, raw[1..].to_string()),
            Some('*') => Row::Star(raw[1..].to_string()),
            Some('@') => Row::At(raw[1..].to_string()),
            _ => {
                return Err(Error::Syntax {
                    line: no,
                    what: format!(
                        "`{}` is not a marked row: every row starts with `-`, `=`, \
                         `+`, `*` or `@`",
                        crop(raw)
                    ),
                });
            }
        };
        group.push(row);
    }

    flush(&mut sections, &mut group, group_at)?;

    if sections.is_empty() {
        return Err(Error::Empty);
    }
    Ok(Patch { sections })
}

// Structure is checked here, meaning at parse time: a `*` row is a point
// anchor that only `+` rows may hug, one per operation; `@` rows lead an
// operation as the scope stack the marks run under. Anything else is said
// plainly.
fn validate(group: &[Row], at: usize) -> Result<(), Error> {
    let hint = "a `*` row only anchors `+` rows above or below it; rows under a \
                construct are edited through `@`";
    if group.iter().filter(|r| matches!(r, Row::Star(..))).count() > 1 {
        return Err(Error::Syntax {
            line: at,
            what: "one `*` anchor per operation — split operations with a blank line".into(),
        });
    }
    if let Some(ix) = group.iter().position(|r| matches!(r, Row::Star(..))) {
        if group.len() == 1 {
            return Err(Error::Syntax {
                line: at,
                what: "the `*` row anchors nothing — put `+` rows above or below it".into(),
            });
        }
        let ok = group[..ix]
            .iter()
            .chain(group[ix + 1..].iter())
            .all(|r| matches!(r, Row::Mark(Mark::Add, _)));
        if !ok {
            return Err(Error::Syntax {
                line: at,
                what: hint.into(),
            });
        }
        return Ok(());
    }
    let mut in_scope = true;
    for r in group {
        match r {
            Row::At(..) if in_scope => {}
            Row::At(..) => {
                return Err(Error::Syntax {
                    line: at,
                    what: "an `@` scope comes first in its operation".into(),
                });
            }
            Row::Star(..) => {
                return Err(Error::Syntax {
                    line: at,
                    what: hint.into(),
                });
            }
            Row::Mark(..) => in_scope = false,
        }
    }
    if in_scope {
        return Err(Error::Syntax {
            line: at,
            what: "the `@` scope names no operation — add `-`/`=`/`+` rows under it".into(),
        });
    }
    Ok(())
}

fn crop(s: &str) -> String {
    let mut chars = s.chars();
    let mut t: String = chars.by_ref().take(40).collect();
    if chars.next().is_some() {
        t.push('…');
    }
    t
}
