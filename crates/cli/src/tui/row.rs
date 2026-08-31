//! One row of scrollback, and the only place one can be made.
//!
//! Two producers fill the scrollback and always will: the live stream, which
//! draws content the session does not hold yet, and a rebuild from the
//! transcript, which is the only way back after a rewind. What can be stopped
//! is the second half of that — the same kind of row having two ways to be
//! built. Four of those drifted before this module existed, each found by
//! someone noticing the screen looked different after `/resume` than it had a
//! minute earlier.
//!
//! So `Kind` is private. A row is made by calling one of these constructors,
//! or it is not made at all, and the two callers cannot disagree about what a
//! row of a given kind looks like — there is only one of each.

use std::borrow::Cow;
use std::cell::RefCell;

use brain::message::{ToolResult, ToolResultContent};

use crate::render::{self, Markdown, Paint};

const BANNER: &str = concat!("π ", env!("CARGO_PKG_VERSION"));

pub struct Row(Kind);

enum Kind {
    /// A painted line the screen alone knows about: the banner, a command's
    /// output, a warning. Colour does not depend on width, so painting it
    /// early costs nothing.
    Notice(String),
    /// A tool's result, kept as its parts. Clipping waits for the frame that
    /// needs it: a row clipped at the width it landed at can never grow back
    /// when the window does.
    ///
    /// `painted` holds the last frame's clipping, keyed by the width it was
    /// done at. The screen asks for one row at a time, and a result is as many
    /// rows as its diff has lines, so painting the whole result per row asked
    /// for made a redraw quadratic in the size of the diff — on every spinner
    /// tick, for as long as it stayed on screen.
    Result {
        ok: bool,
        name: String,
        preview: String,
        painted: RefCell<Option<(usize, Vec<String>)>>,
    },
    /// A block of reasoning that can be folded or unfolded.
    Reasoning {
        /// Which block this row belongs to; the stream appends completed lines
        /// to the open block's row and nothing else.
        block: u64,
        lines: Vec<String>,
        folded: bool,
    },
}

impl Row {
    /// Something only the screen ever knew. Free-form on purpose — no session
    /// entry answers for it, so nothing can drift.
    pub fn notice(line: impl Into<String>) -> Self {
        Row(Kind::Notice(line.into()))
    }

    /// One finished line of the answer, painted as markdown.
    ///
    /// `md` is the caller's because its state spans the block and only the
    /// caller knows where a block ends: the live stream resets at each close,
    /// a rebuild hands in a fresh one per block. The rest is stated here once
    /// instead of twice with a comment claiming the two match.
    ///
    /// Returns the string, not a row: the live stream paints a line before it
    /// knows where the line goes.
    pub fn answer_line(line: &str, md: &mut Markdown, paint: &Paint) -> String {
        let painted = md.line(line, paint);
        md.advance(line);
        painted
    }

    /// One reasoning line. Also a string — a reasoning line lives inside a
    /// block's row, not beside it.
    pub fn reasoning_line(line: &str, paint: &Paint) -> String {
        paint.on(&paint.theme.muted, line)
    }

    /// A whole assistant text block, for a caller that has one.
    pub fn answer(text: &str, md: &mut Markdown, paint: &Paint) -> Vec<Self> {
        text.lines()
            .map(|line| Self::notice(Self::answer_line(line, md, paint)))
            .collect()
    }

    /// A reasoning block's first row. Later lines go in through `push_line`.
    pub fn reasoning(block: u64, lines: Vec<String>, folded: bool) -> Self {
        Row(Kind::Reasoning {
            block,
            lines,
            folded,
        })
    }

    /// A tool result the screen already has in parts — the live path, which
    /// never holds a `ToolResult`.
    pub fn result(ok: bool, name: impl Into<String>, preview: impl Into<String>) -> Self {
        Row(Kind::Result {
            ok,
            name: name.into(),
            preview: preview.into(),
            painted: RefCell::new(None),
        })
    }

    /// The same row out of a stored result: the tool's own sketch when it made
    /// one — that is what the stored content does not hold — and otherwise the
    /// first line of that content, which is what `ToolOutput::preview` falls
    /// back to.
    pub fn stored_result(r: &ToolResult, preview: Option<&str>) -> Self {
        let preview = preview.map(str::to_string).unwrap_or_else(|| {
            let body: String = r
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            match body.split_once('\n') {
                Some((h, _)) => h.to_string(),
                None => body,
            }
        });
        Self::result(!r.is_error, r.name.clone(), preview)
    }

    /// A tool's start line: what an unanswered call keeps in the view.
    pub fn tool_start(name: &str, summary: &str, paint: &Paint) -> Self {
        Self::notice(paint.on(&paint.theme.muted, &tool_start_line(name, summary)))
    }

    /// A prompt's lines as the stream echoed them: the first with its gutter
    /// and the body in the input style, the rest indented to match.
    pub fn prompt(text: &str, gutter: &str, bang: &str, paint: &Paint) -> Vec<Self> {
        text.lines()
            .enumerate()
            .map(|(i, line)| {
                let (gutter, body) = if i == 0 {
                    match line.strip_prefix('!') {
                        Some(rest) => (bang, rest.trim_start()),
                        None => (gutter, line),
                    }
                } else {
                    ("  ", line)
                };
                let body = paint.on(&paint.theme.input, body);
                Self::notice(format!("{gutter}{body}"))
            })
            .collect()
    }

    /// How many screen rows this renders to.
    pub fn len(&self) -> usize {
        match &self.0 {
            Kind::Notice(_) => 1,
            Kind::Result { preview, .. } => preview.lines().count().max(1),
            Kind::Reasoning { lines, folded, .. } => {
                if *folded {
                    1
                } else {
                    lines.len()
                }
            }
        }
    }

    /// Row `i` of what this renders to, at this width.
    pub fn line<'a>(&'a self, i: usize, paint: &'a Paint, width: usize) -> Cow<'a, str> {
        match &self.0 {
            Kind::Notice(s) => Cow::Borrowed(s),
            Kind::Result {
                ok,
                name,
                preview,
                painted,
            } => {
                let mut painted = painted.borrow_mut();
                let rows = match &mut *painted {
                    Some((w, rows)) if *w == width => rows,
                    slot => {
                        let rows = render::result_rows(!*ok, name, preview, paint, width);
                        &mut slot.insert((width, rows)).1
                    }
                };
                // A `RefCell` cannot lend its contents out past the guard, and
                // one row is a line of text: cloning it is the cheap half of
                // what repainting the whole result would cost.
                Cow::Owned(rows[i].clone())
            }
            Kind::Reasoning { lines, folded, .. } => {
                if *folded {
                    // The count row is synthesized at draw time, so it takes
                    // its muted styling here rather than from a painted row.
                    Cow::Owned(paint.on(&paint.theme.muted, &thinking_summary(lines.len())))
                } else {
                    Cow::Borrowed(&lines[i])
                }
            }
        }
    }

    /// The reasoning block this row belongs to, if it is one.
    pub fn block(&self) -> Option<u64> {
        match &self.0 {
            Kind::Reasoning { block, .. } => Some(*block),
            _ => None,
        }
    }

    /// Whether this row is folded, and the handle to change it.
    pub fn folded(&self) -> Option<bool> {
        match &self.0 {
            Kind::Reasoning { folded, .. } => Some(*folded),
            _ => None,
        }
    }

    pub fn set_folded(&mut self, to: bool) {
        if let Kind::Reasoning { folded, .. } = &mut self.0 {
            *folded = to;
        }
    }

    /// Append a finished line to a reasoning block. A no-op on anything else,
    /// which no caller can reach: the only handle to a row is one found by
    /// `block()`, and only a reasoning row answers that.
    pub fn push_line(&mut self, painted: String) {
        if let Kind::Reasoning { lines, .. } = &mut self.0 {
            lines.push(painted);
        }
    }

    /// What the screen opens with: the version, and the instruction files this
    /// run stands on.
    ///
    /// Built rather than stored, so a `/reload` onto a new theme replaces the
    /// rows instead of repainting the strings inside them — there is no way to
    /// reach those. The files are shown here rather than said as a startup
    /// note: they are what the run is standing on, not news, and a note about
    /// them scrolls away while this stays at the top where it belongs.
    pub fn banner(context: &[String], paint: &Paint) -> Vec<Self> {
        let muted = |line: &str| Self::notice(paint.on(&paint.theme.muted, line));
        let mut rows = vec![muted(BANNER)];
        if !context.is_empty() {
            rows.push(muted("context:"));
            rows.extend(context.iter().map(|f| muted(&format!("- {f}"))));
        }
        rows
    }
}

/// A tool call named the way every row that shows one names it: the tool, and
/// its leading argument when it has one. The prefix is the caller's — a spinner
/// while it runs, an arrow once it is abandoned — and that is the only part
/// that differs.
pub fn named(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
}

// A tool's start line: what an unanswered call keeps in the view.
fn tool_start_line(name: &str, summary: &str) -> String {
    format!("→ {}", named(name, summary))
}

// The count line a shut thinking block leaves in the scrollback.
fn thinking_summary(n: usize) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("thinking · {n} line{s}")
}
