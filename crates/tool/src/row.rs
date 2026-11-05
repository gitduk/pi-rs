//! The rows under a result's head: one text, one writer and one reader.
//!
//! A tool's preview is the row its result draws — a head line, then these
//! rows. `render` writes them and `parse` reads them back, so the shape
//! exists once: a reader never has to guess whether a line a command printed
//! is one of the rows below it.

/// One row under a result's head, as its tool meant it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A line the command printed, shown as it came.
    Output(String),
    /// A file whose rows follow, with the counts its head states.
    File {
        path: String,
        plus: usize,
        minus: usize,
    },
    /// One row of that file, numbered on the side it reads in.
    Line { sign: Sign, n: usize, text: String },
    /// Rows left out of a long run.
    Gap,
}

/// Which side of a change a row is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Sign {
    /// A row the file kept.
    Kept,
    /// A row the change took out.
    Gone,
    /// A row the change put in.
    Come,
}

impl Sign {
    /// The mark the row carries: the one letter a diff reads by.
    pub fn mark(self) -> char {
        match self {
            Sign::Kept => ' ',
            Sign::Gone => '-',
            Sign::Come => '+',
        }
    }

    fn of(mark: char) -> Option<Self> {
        match mark {
            ' ' => Some(Sign::Kept),
            '-' => Some(Sign::Gone),
            '+' => Some(Sign::Come),
            _ => None,
        }
    }
}

// Output and left-out rows carry a mark no file row can: a row of a file
// starts with its sign and a number, so a command printing `+3 import os`
// stays output.
const OUTPUT: &str = "· ";
const GAP: char = '⋮';

/// The rows as the preview holds them: a head's rows, one text.
pub fn render(rows: &[Row]) -> String {
    let width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Line { n, .. } => Some(*n),
            _ => None,
        })
        .max()
        .map_or(1, |n| n.to_string().len());
    rows.iter()
        .map(|row| match row {
            Row::Output(text) => format!("{OUTPUT}{text}"),
            Row::File { path, plus, minus } => format!("{path} +{plus} -{minus}"),
            Row::Line { sign, n, text } => format!("{}{n:>width$} {text}", sign.mark()),
            Row::Gap => format!(" {:>width$}", GAP),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What `line` is, read in place: what a screen needs, without a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    Output(&'a str),
    File {
        path: &'a str,
        plus: usize,
        minus: usize,
    },
    Line {
        sign: Sign,
        n: usize,
        text: &'a str,
    },
    Gap,
}

/// The row `line` holds, as a value of its own.
pub fn parse(line: &str) -> Row {
    Row::from(kind(line))
}

impl From<Kind<'_>> for Row {
    fn from(kind: Kind<'_>) -> Self {
        match kind {
            Kind::Output(text) => Row::Output(text.to_string()),
            Kind::File { path, plus, minus } => Row::File {
                path: path.to_string(),
                plus,
                minus,
            },
            Kind::Line { sign, n, text } => Row::Line {
                sign,
                n,
                text: text.to_string(),
            },
            Kind::Gap => Row::Gap,
        }
    }
}

/// The row `line` holds, borrowed from it. A line of no known shape is a
/// command's output, the way a reader that does not recognise a row shows it.
pub fn kind(line: &str) -> Kind<'_> {
    if let Some(text) = line.strip_prefix(OUTPUT) {
        return Kind::Output(text);
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with(GAP) && trimmed.len() == GAP.len_utf8() {
        return Kind::Gap;
    }
    if let Some(file) = file(line) {
        return file;
    }
    line_of(line).unwrap_or(Kind::Output(line))
}

// `path +a -b`, read from the right: a path may hold both a plus and a space.
fn file(line: &str) -> Option<Kind<'_>> {
    let (head, minus) = line.rsplit_once(" -")?;
    let (path, plus) = head.rsplit_once(" +")?;
    if path.is_empty() {
        return None;
    }
    Some(Kind::File {
        path,
        plus: plus.parse().ok()?,
        minus: minus.parse().ok()?,
    })
}

// A sign, a right-aligned number, a space, then the row's text.
fn line_of(line: &str) -> Option<Kind<'_>> {
    let sign = Sign::of(line.chars().next()?)?;
    let rest = line[1..].trim_start_matches(' ');
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 || rest.as_bytes().get(digits) != Some(&b' ') {
        return None;
    }
    Some(Kind::Line {
        sign,
        n: rest[..digits].parse().ok()?,
        text: &rest[digits + 1..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        vec![
            Row::Output("1 file changed".into()),
            Row::Output("+3 import os".into()),
            Row::File {
                path: "src/主.rs".into(),
                plus: 2,
                minus: 1,
            },
            Row::Line {
                sign: Sign::Kept,
                n: 9,
                text: "let a = 1;".into(),
            },
            Row::Line {
                sign: Sign::Gone,
                n: 10,
                text: "let b = 2;".into(),
            },
            Row::Line {
                sign: Sign::Come,
                n: 10,
                text: "+ let c = 3;".into(),
            },
            Row::Gap,
            Row::Line {
                sign: Sign::Kept,
                n: 240,
                text: String::new(),
            },
        ]
    }

    // What one side writes, the other reads back: the shape has one owner.
    #[test]
    fn a_rendered_row_parses_back_to_itself() {
        let rows = rows();
        let text = render(&rows);
        let back: Vec<Row> = text.lines().map(parse).collect();
        assert_eq!(back, rows, "{text}");
    }

    // Counts line up to the widest number, so a three-digit row sits under a
    // one-digit one.
    #[test]
    fn numbers_share_one_column() {
        assert_eq!(render(&rows()[6..]), "   \u{22ee}\n 240 ");
    }

    // A command's own line keeps its shape: output carries the mark, so a row
    // that reads like a file's is read as one only when a tool wrote it so.
    #[test]
    fn a_command_prints_something_that_looks_like_a_row() {
        let printed = [Row::Output("+3 import os".into())];
        assert_eq!(render(&printed), "\u{b7} +3 import os");
        assert_eq!(parse(&render(&printed)), printed[0]);
        assert_eq!(
            parse("README.md +2 -1"),
            Row::File {
                path: "README.md".into(),
                plus: 2,
                minus: 1
            }
        );
        // A path may hold a plus and a space; the counts are read from the end.
        assert_eq!(
            parse("a + b.md +3 -0"),
            Row::File {
                path: "a + b.md".into(),
                plus: 3,
                minus: 0
            }
        );
    }
}
