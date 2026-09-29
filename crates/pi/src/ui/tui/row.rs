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

use std::cell::RefCell;

use llm::message::{ToolResult, ToolResultContent};
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::core::resolve::Resolved;
use crate::store::icons;
use crate::ui::render::named;
use crate::ui::render::{self, Paint};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedTool {
    pub name: String,
    pub preview: String,
}

impl FoldedTool {
    fn desc(&self) -> String {
        desc_of(&self.name, &self.preview)
    }
}

// A call still in flight, as the group that will fold it draws it: the
// row is where the call lands, so it draws the call from the moment it starts.
// The live block has no line of it then, and nothing jumps up when the result
// arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTool {
    /// The call's name and leading argument, read the way the live line read
    /// them — they already agree with the row the call will land as.
    pub name: String,
    pub preview: String,
    /// Whether the call has landed yet. A call that lands badly is never the
    /// row's — it is on its way to a line of its own — so what the row holds
    /// is always a call it will keep.
    pub landed: bool,
}

impl PendingTool {
    fn desc(&self) -> String {
        desc_of(&self.name, &self.preview)
    }
}

// Whether the row still has a call out, which is when it spins. Any of them,
// not the newest: the row is the only place a call in flight shows, so its
// mark cannot settle while one is still running.
fn spinning(pending: &[PendingTool]) -> bool {
    pending.iter().any(|p| !p.landed)
}

// How many calls the row speaks for: the ones landed in it and the ones it
// holds while they are out. One is its own line, so the count is part of
// what decides whether it has a body to unfold.
fn calls(steps: &[Step], pending: &[PendingTool]) -> usize {
    steps.iter().filter(|s| matches!(s, Step::Tool(_))).count() + pending.len()
}

// The reasoning lines the row holds, or `None` when it holds no block.
fn thought(steps: &[Step]) -> Option<usize> {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Thinking { lines, .. } => Some(lines.len()),
            Step::Tool(_) => None,
        })
        .reduce(|a, b| a + b)
}

// Whether unfolding would show more than the row's own line.
fn expandable(steps: &[Step], pending: &[PendingTool]) -> bool {
    calls(steps, pending) > 1 || thought(steps).is_some_and(|n| n > 0)
}

// The tool and its leading argument, named the one way every row that shows a
// tool names it: the name alone when there is nothing under it, and the name
// dropped when the line already carries it.
fn desc_of(name: &str, preview: &str) -> String {
    let head = preview.lines().next().unwrap_or("").trim();
    if head.is_empty() {
        name.to_string()
    } else if head.starts_with(name) && head[name.len()..].starts_with(char::is_whitespace) {
        head.to_string()
    } else {
        format!("{name} {head}")
    }
}

/// What an unfolded group's steps sit under its line with.
pub const STEP_INDENT: &str = "  ";

/// One step of a group, in the order the run took it: a read-only call that
/// landed well, or a block of reasoning.
pub enum Step {
    Tool(FoldedTool),
    Thinking {
        // The stream appends a block's lines to the row holding its id.
        block: u64,
        lines: Vec<Line<'static>>,
    },
}

pub struct Row(Kind, Height);

// The rows this row takes on screen at one width, wraps included, counted one
// logical line at a time and remembered until the width or the content
// changes.
#[derive(Default)]
struct Height(RefCell<Option<Measured>>);

struct Measured {
    width: usize,
    // One entry per logical line, `None` until that line is wrapped. Kept per
    // line rather than as one total so a tall row is never wrapped in full to
    // answer for one of its lines.
    lines: Vec<Option<usize>>,
}

impl Height {
    // The measurement at this width, with room for `lines` of them. A width
    // that has moved starts again; a row that has grown since keeps what was
    // already counted, since nothing else can make its lines shorter.
    fn at(&self, width: usize, lines: usize) -> std::cell::RefMut<'_, Measured> {
        let mut held = self.0.borrow_mut();
        match held.as_mut() {
            Some(m) if m.width == width => {
                if m.lines.len() < lines {
                    m.lines.resize(lines, None);
                }
            }
            _ => {
                *held = Some(Measured {
                    width,
                    lines: vec![None; lines],
                });
            }
        }
        std::cell::RefMut::map(held, |h| h.as_mut().expect("filled above"))
    }

    fn line(&self, width: usize, i: usize) -> Option<usize> {
        self.0
            .borrow()
            .as_ref()
            .filter(|m| m.width == width)
            .and_then(|m| m.lines.get(i).copied().flatten())
    }

    // The row's count, once every line of it has been. Nothing to answer with
    // while a line is still unmeasured, so a caller that asks then measures
    // what is missing — see `height`.
    fn total(&self, width: usize) -> Option<usize> {
        let held = self.0.borrow();
        let m = held.as_ref().filter(|m| m.width == width)?;
        m.lines
            .iter()
            .all(Option::is_some)
            .then(|| m.lines.iter().flatten().sum())
    }

    fn set(&self, width: usize, i: usize, height: usize) {
        self.at(width, i + 1).lines[i] = Some(height);
    }

    fn clear(&self) {
        *self.0.borrow_mut() = None;
    }
}

enum Kind {
    // One logical line of a prompt the user said: the border and the body kept
    // apart, so a wrap can repeat the border.
    //
    // `band` is the band the whole screen row sits in: `prompt.panel.said`.
    Said {
        // The rule and its column: `SAID_RULE` in the prompt colour.
        border: Line<'static>,
        // The line's text, in the input style, without the border.
        body: Line<'static>,
        band: Option<RStyle>,
    },
    // What the model answered, one row per line the markdown handed over.
    // Its own kind rather than a notice — a notice is a thing only the screen
    // knew, and an answer is half of what the conversation is.
    Answer(Line<'static>),
    // A painted line the screen alone knows about: the banner, a command's
    // output, a warning. Colour does not depend on width, so painting it
    // early costs nothing.
    //
    // `times` counts the same notice landing again with nothing between it
    // and the last one: a key held down, or a refusal repeated. It renders as
    // one row with a count rather than as a column of identical lines.
    Notice {
        text: Line<'static>,
        times: usize,
    },
    // A tool's result, kept as its parts. Clipping waits for the frame that
    // needs it: a row clipped at the width it landed at can never grow back
    // when the window does.
    //
    // `painted` holds the last frame's clipping, keyed by the width it was
    // done at. The screen asks for one row at a time, and a result is as many
    // rows as its diff has lines, so painting the whole result per row asked
    // for made a redraw quadratic in the size of the diff — on every spinner
    // tick, for as long as it stayed on screen.
    Result {
        ok: bool,
        name: String,
        preview: String,
        // The row count, so expandability and toggle don't re-walk the
        // preview string on every mouse move.
        preview_lines: usize,
        expanded: bool,
        hovered: bool,
        painted: RefCell<Option<(usize, Vec<Line<'static>>)>>,
    },
    // The run's working behind one fold: read-only calls that landed well
    // and the reasoning around them, in order.
    //
    // `pending` is the calls in flight this row draws: they are not in the
    // scrollback yet, and the live block leaves them to the row, which is
    // where they will land.
    Steps {
        steps: Vec<Step>,
        pending: Vec<PendingTool>,
        folded: bool,
        hovered: bool,
        spinner: usize,
    },
}

impl Row {
    // A row whose height is not measured yet; every constructor starts here.
    fn new(kind: Kind) -> Self {
        Self(kind, Height::default())
    }

    /// Whether this row is part of the conversation: what was said and what
    /// was answered, which is what browse mode is for.
    pub fn is_conversation(&self) -> bool {
        matches!(&self.0, Kind::Said { .. } | Kind::Answer(_))
    }

    /// Something only the screen ever knew, in the muted voice everything the
    /// surface says for itself is said in: the tally line a run ends on, what
    /// a lane answered, a warning about the turn. Free-form on purpose — a
    /// session entry answers for one only where `push_screen` filed it, and
    /// that entry holds the text, not the row.
    pub fn notice(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
        })
    }

    /// A run of plain text as notice rows, one per line: what the live stream
    /// draws for a row it also files, and what a rebuild draws from the entry.
    /// The two have to come out the same, so they come out of here — the
    /// archived half is text, and nothing else knows how a screen row reads.
    pub fn notice_lines(text: &str, paint: &Paint) -> Vec<Self> {
        text.lines()
            .map(|l| Self::notice(Line::from(paint.span(&paint.theme.muted, l))))
            .collect()
    }

    /// Fold a repeat into this row, if it is the same notice: the scrollback
    /// then shows `line ×2` where a second row would have gone.
    ///
    /// Compares the spans, which is what makes it safe: two notices that
    /// read alike but are styled differently are different rows, and a row of
    /// any other kind never folds.
    pub fn repeated(&mut self, line: &Line<'_>) -> bool {
        match &mut self.0 {
            Kind::Notice { text, times } if *text == *line => {
                *times += 1;
                self.1.clear();
                true
            }
            _ => false,
        }
    }

    /// One reasoning line. A reasoning line lives inside its group, not
    /// beside it.
    pub fn reasoning_line(line: &str, paint: &Paint) -> Line<'static> {
        Line::from(paint.span(&paint.theme.muted, line))
    }

    /// A whole assistant text block, for a caller that has one.
    pub fn answer(text: &str, paint: &Paint) -> Vec<Self> {
        render::render_markdown(text, paint)
            .into_iter()
            .map(|text| Self::new(Kind::Answer(text)))
            .collect()
    }

    /// A group whose first step is `first`, folded the way it is born.
    pub fn steps(first: Step, folded: bool) -> Self {
        Self::new(Kind::Steps {
            steps: vec![first],
            pending: Vec::new(),
            folded,
            hovered: false,
            spinner: 0,
        })
    }

    /// A lone block of reasoning, as a group of one.
    #[cfg(test)]
    pub fn thinking(block: u64, lines: Vec<Line<'static>>, folded: bool) -> Self {
        Self::steps(Step::Thinking { block, lines }, folded)
    }

    /// Add a step to this row if it is a group; the step comes back if not.
    pub fn join(&mut self, step: Step) -> Result<(), Step> {
        if let Kind::Steps { steps, .. } = &mut self.0 {
            steps.push(step);
            self.1.clear();
            Ok(())
        } else {
            Err(step)
        }
    }

    /// Whether this row is a group.
    pub fn is_steps(&self) -> bool {
        matches!(&self.0, Kind::Steps { .. })
    }

    /// Whether this row is a group that names calls, and so draws a line of
    /// its own above its steps when unfolded.
    pub fn has_calls(&self) -> bool {
        match &self.0 {
            Kind::Steps { steps, pending, .. } => calls(steps, pending) > 0,
            _ => false,
        }
    }

    /// Whether this row is an expandable row.
    pub fn is_expandable(&self) -> bool {
        match &self.0 {
            Kind::Steps { steps, pending, .. } => expandable(steps, pending),
            Kind::Result { preview_lines, .. } => *preview_lines > render::SKETCHED_ROWS,
            _ => false,
        }
    }

    /// Toggle expand state, returning true if toggled.
    pub fn toggle_expand(&mut self) -> bool {
        if !self.is_expandable() {
            return false;
        }
        match &mut self.0 {
            Kind::Steps { folded, .. } => {
                *folded = !*folded;
                self.1.clear();
                true
            }
            Kind::Result {
                expanded, painted, ..
            } => {
                *expanded = !*expanded;
                *painted.borrow_mut() = None;
                self.1.clear();
                true
            }
            _ => false,
        }
    }

    /// Bring a row up to date before the frame is drawn: which row the mouse
    /// is over, where the spinner is, and the calls in flight this row draws
    /// for. A group spins for the calls it holds, so the two arrive together
    /// rather than as a flag beside them.
    pub fn update_live(&mut self, hovered: bool, spin: usize, held: &[PendingTool]) {
        match &mut self.0 {
            Kind::Steps {
                pending,
                hovered: h,
                spinner: s,
                ..
            } => {
                // A call starting, ending or landing rewrites the row's line
                // and can change how many rows it counts for.
                if pending.as_slice() != held {
                    *pending = held.to_vec();
                    self.1.clear();
                }
                // Neither moves a height: the line is clipped to the width.
                *h = hovered;
                if spinning(pending) {
                    *s = spin;
                }
            }
            Kind::Result {
                hovered: h,
                preview_lines,
                painted,
                ..
            } if *preview_lines > render::SKETCHED_ROWS && *h != hovered => {
                *h = hovered;
                *painted.borrow_mut() = None;
            }
            _ => {}
        }
    }

    /// A tool result the screen already has in parts — the live path, which
    /// never holds a `ToolResult`.
    pub fn result(ok: bool, name: impl Into<String>, preview: impl Into<String>) -> Self {
        let preview = preview.into();
        let preview_lines = preview.lines().count();
        Self::new(Kind::Result {
            ok,
            name: name.into(),
            preview,
            preview_lines,
            expanded: false,
            hovered: false,
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

    /// The tool's name if this row is a tool result.
    pub fn tool_name(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The tool's preview if this row is a tool result.
    pub fn tool_preview(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result { preview, .. } => Some(preview),
            _ => None,
        }
    }

    /// Whether this tool result succeeded.
    pub fn ok(&self) -> Option<bool> {
        match &self.0 {
            Kind::Result { ok, .. } => Some(*ok),
            _ => None,
        }
    }

    /// A tool's start line: what an unanswered call keeps in the view.
    pub fn tool_start(name: &str, summary: &str, paint: &Paint) -> Self {
        Self::notice(Line::from(
            paint.span(&paint.theme.muted, tool_start_line(name, summary)),
        ))
    }

    // One logical line of a prompt the user said: the rule it wears, the text
    // under it, and the panel both sit in. The border lives apart from the body.
    fn said(border: Line<'static>, body: Line<'static>, band: Option<RStyle>) -> Self {
        Self::new(Kind::Said { border, body, band })
    }

    /// A prompt's lines as the stream echoed them: the same bar the input
    /// line wears, on a band one step off the one it was typed on, so a
    /// landed line reads as the line that was typed.
    pub fn prompt(text: &str, paint: &Paint) -> Vec<Self> {
        let band = paint.band(&paint.theme.prompt.panel.said);
        if text.starts_with('!') {
            // A `!` is a command, not something said: the bang takes the
            // prompt's place, and the lines under it keep the plain indent.
            let bang = paint.span(&paint.theme.prompt.color, icons::bar(icons::BANG_SIGIL));
            // Measured, never assumed: the continuation rows indent under the
            // bang by the columns the bang really takes.
            let indent = Span::raw(" ".repeat(bang.width()));
            let mut rows = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let (prefix, body) = if i == 0 {
                    (
                        bang.clone(),
                        line.strip_prefix('!').unwrap_or(line).trim_start(),
                    )
                } else {
                    (indent.clone(), line)
                };
                rows.push(Self::said(
                    Line::from(prefix),
                    Line::from(paint.span(&paint.theme.input, body)),
                    band,
                ));
            }
            return rows;
        }
        // Unbroken down every line said: the icon marks what is being typed,
        // and a landed line wearing it reads as another place to type.
        let border =
            Line::from(paint.span(&paint.theme.prompt.color, icons::bar(icons::SAID_RULE)));
        text.lines()
            .map(|line| {
                Self::said(
                    border.clone(),
                    Line::from(paint.span(&paint.theme.input, line)),
                    band,
                )
            })
            .collect()
    }

    /// How many logical lines the row is made of — the `i`s `line` answers.
    /// Wraps not counted; `height` is the screen-row count.
    pub fn len(&self) -> usize {
        match &self.0 {
            Kind::Answer(_) | Kind::Notice { .. } | Kind::Said { .. } => 1,
            Kind::Result {
                preview_lines,
                expanded,
                ..
            } => {
                if *preview_lines <= render::SKETCHED_ROWS {
                    (*preview_lines).max(1)
                } else if *expanded {
                    *preview_lines + 1
                } else {
                    render::SKETCHED_ROWS + 1
                }
            }
            Kind::Steps {
                steps,
                pending,
                folded,
                ..
            } => {
                if *folded || !expandable(steps, pending) {
                    1
                } else {
                    let header = usize::from(calls(steps, pending) > 0);
                    header + steps.iter().map(Step::len).sum::<usize>() + pending.len()
                }
            }
        }
    }

    /// Screen rows this row takes at `width`, wraps included, remembered by
    /// width until the content changes. The scrolled-up view's accounting
    /// reads these; a wrap is a row the count has to know about.
    pub fn height(&self, paint: &Paint, width: usize) -> usize {
        if let Some(total) = self.1.total(width) {
            return total;
        }
        (0..self.len())
            .map(|i| self.line_height(i, paint, width))
            .sum()
    }

    /// Screen rows logical line `i` takes at `width`, from the same
    /// measurement `height` keeps. The window's walk reads one of these per
    /// line it passes, so a line it is not going to show is counted without
    /// being wrapped.
    pub fn line_height(&self, i: usize, paint: &Paint, width: usize) -> usize {
        if let Some(h) = self.1.line(width, i) {
            return h;
        }
        let (line, border) = self.line(i, paint, width);
        let h = super::screen::wrap(border.as_ref(), &line, width).len();
        self.1.set(width, i, h);
        h
    }

    /// What the screen renders for row `i` of this row, at this width: the
    /// text, and the border its continuation rows must repeat. A said row
    /// keeps the rule apart from the body so a line wider than the terminal
    /// can carry it to every row it wraps to; anything else is a single text
    /// with no border to keep.
    pub fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        match &self.0 {
            Kind::Said { border, body, .. } => (body.clone(), Some(border.clone())),
            Kind::Answer(text) => (text.clone(), None),
            Kind::Notice { text, times } if *times == 1 => (text.clone(), None),
            Kind::Notice { text, times } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning.
                let mut spans = text.spans.clone();
                spans.push(paint.span(&paint.theme.muted, format!(" ×{times}")));
                (Line::from(spans), None)
            }
            Kind::Result {
                ok,
                name,
                preview,
                expanded,
                hovered,
                painted,
                ..
            } => {
                let mut painted = painted.borrow_mut();
                let rows = match &mut *painted {
                    Some((w, rows)) if *w == width => rows,
                    slot => {
                        let rows = render::result_rows(
                            !*ok, name, preview, *expanded, *hovered, paint, width,
                        );
                        &mut slot.insert((width, rows)).1
                    }
                };
                (rows.get(i).cloned().unwrap_or_default(), None)
            }
            Kind::Steps {
                steps,
                pending,
                folded,
                hovered,
                spinner,
            } => {
                let open = !*folded && expandable(steps, pending);
                let header = calls(steps, pending) > 0;
                let head = || steps_header(steps, pending, open, *hovered, *spinner, paint, width);
                if !open || (header && i == 0) {
                    return (head(), None);
                }
                let border = header.then(|| Line::from(STEP_INDENT));
                (
                    step_line(steps, pending, i - usize::from(header), paint, width),
                    border,
                )
            }
        }
    }

    /// The panel this row's screen rows sit in, if it sits in one: the colour
    /// goes behind the row whole, past the text's own end.
    pub fn band(&self) -> Option<RStyle> {
        match &self.0 {
            Kind::Said { band, .. } => *band,
            _ => None,
        }
    }

    /// Whether this row is the group holding reasoning block `id`.
    pub fn holds_block(&self, id: u64) -> bool {
        match &self.0 {
            Kind::Steps { steps, .. } => steps
                .iter()
                .any(|s| matches!(s, Step::Thinking { block, .. } if *block == id)),
            _ => false,
        }
    }

    /// Whether this row is folded, and the handle to change it.
    pub fn folded(&self) -> Option<bool> {
        match &self.0 {
            Kind::Steps { folded, .. } => Some(*folded),
            _ => None,
        }
    }

    pub fn set_folded(&mut self, to: bool) {
        if let Kind::Steps { folded, .. } = &mut self.0 {
            *folded = to;
            self.1.clear();
        }
    }

    /// Drop block `id` if it ended without a line, and say whether the group
    /// is left with no step at all.
    pub fn drop_if_empty(&mut self, id: u64) -> bool {
        let Kind::Steps { steps, .. } = &mut self.0 else {
            return false;
        };
        steps.retain(
            |s| !matches!(s, Step::Thinking { block, lines } if *block == id && lines.is_empty()),
        );
        self.1.clear();
        steps.is_empty()
    }

    /// Append a finished line to reasoning block `id`. A no-op anywhere else,
    /// which no caller can reach: the row is found by `holds_block`.
    pub fn push_line(&mut self, id: u64, line: Line<'static>) {
        if let Kind::Steps { steps, .. } = &mut self.0
            && let Some(Step::Thinking { lines, .. }) = steps
                .iter_mut()
                .rev()
                .find(|s| matches!(s, Step::Thinking { block, .. } if *block == id))
        {
            lines.push(line);
            self.1.clear();
        }
    }

    /// What the screen opens with: the version, the endpoint requests go to,
    /// and the instruction files this run stands on.
    ///
    /// Built rather than stored, so a `/reload` onto a new theme replaces the
    /// rows instead of repainting the strings inside them — there is no way to
    /// reach those. The files are shown here rather than said as a startup
    /// note: they are what the run is standing on, not news, and a note about
    /// them scrolls away while this stays at the top where it belongs.
    pub fn banner(resolved: &Resolved, paint: &Paint) -> Vec<Self> {
        let muted = |line: &str| Self::notice(Line::from(paint.span(&paint.theme.muted, line)));
        let mut rows = vec![muted(icons::VERSION_BANNER)];
        rows.extend(resolved.endpoint.as_deref().map(muted));
        let context = &resolved.context;
        if !context.is_empty() {
            rows.push(muted("context:"));
            rows.extend(context.iter().map(|f| muted(&format!("- {f}"))));
        }
        rows
    }
}

// A tool's start line: what an unanswered call keeps in the view.
fn tool_start_line(name: &str, summary: &str) -> String {
    format!("{} {}", icons::PENDING_MARK, named(name, summary))
}

// "N line(s)", the count a folded block of reasoning shows.
fn line_count(n: usize) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} line{s}")
}

fn clip_to(s: &str, max_cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max_cols {
            return s[..i].trim_end();
        }
        used += w;
    }
    s.trim_end()
}

impl Step {
    // The lines it takes in an unfolded group.
    fn len(&self) -> usize {
        match self {
            Step::Tool(_) => 1,
            Step::Thinking { lines, .. } => lines.len(),
        }
    }
}

// A group's own line: its newest call, in flight or not, so the line holds
// still when the result lands; folded, also what it hides.
fn steps_header(
    steps: &[Step],
    pending: &[PendingTool],
    open: bool,
    hovered: bool,
    spinner: usize,
    paint: &Paint,
    width: usize,
) -> Line<'static> {
    let thought = thought(steps);
    let newest = pending.last().map(PendingTool::desc).or_else(|| {
        steps.iter().rev().find_map(|s| match s {
            Step::Tool(t) => Some(t.desc()),
            Step::Thinking { .. } => None,
        })
    });
    let Some(desc) = newest else {
        let text = match thought {
            Some(n) if n > 0 => format!("thinking{}{}", icons::PART_SEP, line_count(n)),
            _ => super::THINKING.to_string(),
        };
        return Line::from(paint.span_hovered(hovered, &paint.theme.muted, text));
    };
    let mut tail = String::new();
    if !open {
        let count = calls(steps, pending);
        if count > 1 {
            tail.push_str(&format!(" {}{count}", icons::ELLIPSIS));
        }
        match thought {
            Some(0) => tail.push_str(&format!("{}{}", icons::PART_SEP, super::THINKING)),
            Some(n) => tail.push_str(&format!("{}thinking {}", icons::PART_SEP, line_count(n))),
            None => {}
        }
    }
    // The mark and the space after it, the tail, and a column of air before
    // the terminal edge: the call's text takes whatever is left.
    let room = width
        .saturating_sub(2 + UnicodeWidthStr::width(tail.as_str()) + 1)
        .max(8);
    // Each half is its own span, so hover bolds instead of recolouring.
    Line::from(vec![
        summary_mark(pending, spinner, hovered, paint),
        paint.span_hovered(
            hovered,
            &paint.theme.muted,
            format!(" {}{tail}", clip_to(&desc, room)),
        ),
    ])
}

// The mark a group leads with: the frame while one of the calls it holds is
// still out, and the check once they have all landed.
fn summary_mark(
    pending: &[PendingTool],
    spinner: usize,
    hovered: bool,
    paint: &Paint,
) -> Span<'static> {
    // The frame is the row's animation, so it wears no style.
    if spinning(pending) {
        return Span::raw(icons::SPINNER_FRAMES[spinner % icons::SPINNER_FRAMES.len()]);
    }
    paint.span_hovered(hovered, &paint.theme.status.ok, icons::DONE_MARK)
}

// Line `i` of an unfolded group's steps, the calls in flight last. The indent
// is `line`'s border, so a wrapped line of reasoning keeps it.
fn step_line(
    steps: &[Step],
    pending: &[PendingTool],
    mut i: usize,
    paint: &Paint,
    width: usize,
) -> Line<'static> {
    let call = |desc: String| {
        let room = width.saturating_sub(STEP_INDENT.len()).max(10);
        Line::from(paint.span(&paint.theme.muted, clip_to(&desc, room).to_string()))
    };
    for step in steps {
        let n = step.len();
        if i >= n {
            i -= n;
            continue;
        }
        return match step {
            Step::Tool(t) => call(t.desc()),
            Step::Thinking { lines, .. } => lines[i].clone(),
        };
    }
    pending.get(i).map(|p| call(p.desc())).unwrap_or_default()
}

#[cfg(test)]
mod conversation_tests {
    use super::*;

    // Browse mode is the whole reason the answer has a kind of its own: it
    // shows what was said and what was answered, and nothing the conversation
    // passed through on its way.
    #[test]
    fn only_what_was_said_and_what_was_answered_is_the_conversation() {
        let paint = Paint::new(false);
        for row in Row::prompt("what is this", &paint)
            .into_iter()
            .chain(Row::answer("# it is this\nand that", &paint))
        {
            assert!(row.is_conversation());
        }
        let tool = Step::Tool(FoldedTool {
            name: "read".into(),
            preview: "src/main.rs".into(),
        });
        for row in [
            Row::notice("a command printed this"),
            Row::thinking(1, vec![Line::from("thinking")], false),
            Row::result(true, "read", "src/main.rs"),
            Row::steps(tool, true),
        ] {
            assert!(!row.is_conversation());
        }
    }
}

#[cfg(test)]
mod steps_tests {
    use super::*;

    fn tool(name: &str, preview: &str) -> FoldedTool {
        FoldedTool {
            name: name.to_string(),
            preview: preview.to_string(),
        }
    }

    // A folded group of these calls, in order.
    fn bundle(tools: Vec<FoldedTool>) -> Row {
        let mut it = tools.into_iter().map(Step::Tool);
        let mut row = Row::steps(it.next().expect("a group holds a step"), true);
        for step in it {
            assert!(row.join(step).is_ok());
        }
        row
    }

    // A call in flight, as the row is handed one: `landed` says whether its
    // result is in yet.
    fn pending(name: &str, preview: &str, landed: bool) -> PendingTool {
        PendingTool {
            name: name.to_string(),
            preview: preview.to_string(),
            landed,
        }
    }

    // The rendered rows are cached by (width, spinner), so a spinning summary
    // row must replace its frame as the spinner advances — a cache keyed by
    // width alone would freeze the row on its first frame.
    #[test]
    fn a_spinning_summary_row_advances_through_the_paint_cache() {
        let paint = Paint::new(true);
        let mut row = bundle(vec![tool("read", "a.rs")]);
        let held = [pending("read", "b.rs", false)];

        use crate::ui::tui::screen::plain;

        row.update_live(false, 0, &held);
        let f0 = plain(&row.line(0, &paint, 80).0);
        row.update_live(false, 1, &held);
        let f1 = plain(&row.line(0, &paint, 80).0);

        // The call it holds leads, and the count is the batch it is already
        // part of: one landed, one still out.
        assert_eq!(
            f0,
            format!(
                "{} read b.rs {}{}",
                icons::SPINNER_FRAMES[0],
                icons::ELLIPSIS,
                2
            )
        );
        assert_eq!(
            f1,
            format!(
                "{} read b.rs {}{}",
                icons::SPINNER_FRAMES[1],
                icons::ELLIPSIS,
                2
            )
        );
        assert_ne!(f0, f1, "the running frame must advance, not freeze");
    }

    // Calls end in whatever order they end in, and the row is the only place
    // one in flight shows: a call still out keeps the spinner on however the
    // call it names ended.
    #[test]
    fn a_call_still_out_keeps_the_row_spinning() {
        let paint = Paint::new(true);
        let mut row = bundle(vec![tool("read", "a.rs")]);

        use crate::ui::tui::screen::plain;

        row.update_live(
            false,
            0,
            &[
                pending("bash", "cargo test", false),
                pending("read", "b.rs", true),
            ],
        );
        let line = plain(&row.line(0, &paint, 80).0);

        assert_eq!(
            line,
            format!(
                "{} read b.rs {}{}",
                icons::SPINNER_FRAMES[0],
                icons::ELLIPSIS,
                3
            ),
            "it names the newest, and spins for the one still out"
        );
    }

    // A call that has landed but is not adopted yet wears the mark it will
    // land with, so the row it is drawn in does not change when it does.
    #[test]
    fn a_landed_batch_wears_its_own_mark() {
        let paint = Paint::new(true);
        let mut row = bundle(vec![tool("read", "a.rs")]);

        use crate::ui::tui::screen::plain;

        row.update_live(false, 0, &[pending("read", "b.rs", true)]);
        let landed = plain(&row.line(0, &paint, 80).0);

        assert_eq!(
            landed,
            format!("{} read b.rs {}{}", icons::DONE_MARK, icons::ELLIPSIS, 2)
        );
    }

    // A row the user unfolded can lose the call that made it unfoldable — a
    // failure leaves for a line of its own, and the row stops being the last
    // one in the scrollback — so it shows as folded: with one call left,
    // `toggle` refuses and the body would repeat the header.
    #[test]
    fn a_row_left_with_one_call_shows_as_folded() {
        let mut row = bundle(vec![tool("read", "a.rs")]);
        row.update_live(false, 0, &[pending("grep", "match 1", false)]);
        assert!(row.is_expandable(), "two calls are a batch");
        assert!(row.toggle_expand());
        assert_eq!(row.len(), 3, "the header and both calls");

        row.update_live(false, 0, &[]);
        assert_eq!(row.len(), 1, "folded back to the one tool it holds");
        assert!(!row.is_expandable());
    }

    // The row's line and its body both count what it holds: a body that
    // listed only the landed tools would be shorter than the header's count.
    #[test]
    fn unfolding_a_summary_row_lists_the_calls_it_holds() {
        let paint = Paint::new(false);
        let mut row = bundle(vec![tool("read", "a.rs")]);

        use crate::ui::tui::screen::plain;

        row.update_live(false, 0, &[pending("grep", "match 1", false)]);
        assert!(row.is_expandable(), "one landed and one out is a batch");
        assert!(row.toggle_expand());
        assert_eq!(row.len(), 3, "the header and both calls");
        let body = plain(&row.line(1, &paint, 80).0);
        let held = plain(&row.line(2, &paint, 80).0);
        assert_eq!(body.trim(), "read a.rs");
        assert_eq!(held.trim(), "grep match 1");
    }

    #[test]
    fn unfolded_tools_summary_shows_all_tools() {
        let tools = bundle(vec![
            tool("read", "crates/agent/src/session.rs"),
            tool("grep", "match 1"),
        ]);
        let mut row = tools;
        assert_eq!(row.len(), 1);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 3);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 1);
    }

    #[test]
    fn a_single_tool_summary_never_unfolds() {
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(!row.is_expandable());
        assert!(!row.toggle_expand());
        assert_eq!(row.len(), 1);
    }

    fn think(block: u64, n: usize) -> Step {
        Step::Thinking {
            block,
            lines: (1..=n).map(|i| Line::from(format!("line {i}"))).collect(),
        }
    }

    // Each line as the screen draws it, border included.
    fn shown(row: &Row, width: usize) -> Vec<String> {
        use crate::ui::tui::screen::plain;
        let paint = Paint::new(false);
        (0..row.len())
            .map(|i| {
                let (line, border) = row.line(i, &paint, width);
                border.map(|b| plain(&b)).unwrap_or_default() + &plain(&line)
            })
            .collect()
    }

    // Reasoning between calls no longer splits them: one line folded, every
    // step in order unfolded.
    #[test]
    fn a_group_folds_its_calls_and_thinking_into_one_line() {
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(row.join(think(1, 3)).is_ok());
        assert!(row.join(Step::Tool(tool("read", "b.rs"))).is_ok());
        assert_eq!(
            shown(&row, 80),
            [format!(
                "{} read b.rs {}2{}thinking 3 lines",
                icons::DONE_MARK,
                icons::ELLIPSIS,
                icons::PART_SEP
            )]
        );
        assert!(row.toggle_expand());
        assert_eq!(
            shown(&row, 80),
            [
                format!("{} read b.rs", icons::DONE_MARK),
                "  read a.rs".into(),
                "  line 1".into(),
                "  line 2".into(),
                "  line 3".into(),
                "  read b.rs".into(),
            ]
        );
    }

    // A block with no call around it keeps the look it always had: its
    // count folded, its lines unfolded, nothing to sit under.
    #[test]
    fn a_lone_block_reads_as_its_count() {
        let mut row = Row::steps(think(1, 2), true);
        assert_eq!(
            shown(&row, 80),
            [format!("thinking{}2 lines", icons::PART_SEP)]
        );
        assert!(row.toggle_expand());
        assert_eq!(shown(&row, 80), ["line 1", "line 2"]);

        // Before its first line lands there is nothing to unfold.
        let empty = Row::steps(think(2, 0), false);
        assert!(!empty.is_expandable());
        assert_eq!(shown(&empty, 80), [crate::ui::tui::THINKING]);
    }

    // One call or many, the text runs up to what follows it; a group's once
    // stopped at 50 columns while a lone call ran to the edge.
    #[test]
    fn a_long_call_fills_the_row_before_what_follows_it() {
        let long = "x".repeat(200);
        let mut row = bundle(vec![tool("bash", &long), tool("bash", &long)]);
        assert!(row.join(think(1, 1)).is_ok());
        let line = &shown(&row, 80)[0];
        let tail = format!(" {}2{}thinking 1 line", icons::ELLIPSIS, icons::PART_SEP);
        assert!(line.ends_with(&tail), "{line}");
        assert_eq!(UnicodeWidthStr::width(line.as_str()), 79, "{line}");

        let one = bundle(vec![tool("bash", &long)]);
        assert_eq!(UnicodeWidthStr::width(shown(&one, 80)[0].as_str()), 79);
    }

    #[test]
    fn result_with_many_diff_lines_expands_and_collapses() {
        let mut preview = "crates/foo.rs +30 -0".to_string();
        for i in 1..=30 {
            preview.push_str(&format!("\n  {i} + line {i}"));
        }
        let mut row = Row::result(true, "edit", preview);
        assert_eq!(row.len(), render::SKETCHED_ROWS + 1);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 32);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), render::SKETCHED_ROWS + 1);
    }
}

#[cfg(test)]
mod height_tests {
    use super::*;

    // The window reads one line's height at a time to pass over the rows a
    // scrolled view does not show, while the view's own accounting reads the
    // row whole. The two have to agree about where a row ends, whichever was
    // asked first and at whichever width.
    #[test]
    fn a_rows_height_is_the_sum_of_its_lines() {
        let paint = Paint::new(false);
        let row = Row::thinking(1, vec![Line::from("abcdef"), Line::from("gh")], false);
        for width in [2usize, 4, 80] {
            let sum: usize = (0..row.len())
                .map(|i| row.line_height(i, &paint, width))
                .sum();
            assert_eq!(sum, row.height(&paint, width), "at width {width}");
        }
        // Asked a whole row at a time, then one line at a time: same answer.
        let fresh = Row::thinking(1, vec![Line::from("abcdef"), Line::from("gh")], false);
        assert_eq!(fresh.height(&paint, 2), 4, "three rows and one");
        assert_eq!(fresh.line_height(0, &paint, 2), 3, "abcdef wraps to three");
        assert_eq!(fresh.line_height(1, &paint, 2), 1);
        assert_eq!(fresh.height(&paint, 2), 4);
    }
}
