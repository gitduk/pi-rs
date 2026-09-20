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

use brain::message::{ToolResult, ToolResultContent};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::icons;
use crate::render::{self, Paint};
use crate::status::{self, Segment, Snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedTool {
    pub name: String,
    pub preview: String,
}

impl FoldedTool {
    pub fn desc(&self) -> String {
        let head = self.preview.lines().next().unwrap_or("").trim();
        if head.is_empty() {
            self.name.clone()
        } else if head.starts_with(&self.name)
            && head[self.name.len()..].starts_with(char::is_whitespace)
        {
            head.to_string()
        } else {
            format!("{} {head}", self.name)
        }
    }
}

/// The tools folded into one summary row. The tool that opened the row is
/// always in it, so an empty bundle has no representation and neither the
/// head nor the unfolded body has an empty case to answer.
pub struct FoldedTools(Vec<FoldedTool>);

impl FoldedTools {
    pub fn new(first: FoldedTool) -> Self {
        Self(vec![first])
    }

    /// The tool the head describes: the newest one folded in.
    pub fn last(&self) -> &FoldedTool {
        self.0.last().expect("a bundle holds a tool")
    }

    /// Whether one tool is the whole row. A batch reads as a count instead.
    pub fn is_single(&self) -> bool {
        self.0.len() == 1
    }

    pub fn count(&self) -> usize {
        self.0.len()
    }

    /// Every tool, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &FoldedTool> {
        self.0.iter()
    }

    pub fn push(&mut self, tool: FoldedTool) {
        self.0.push(tool);
    }
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

// The rendered rows of a tools summary, keyed by what they were painted at:
// width, and the running/spinner frame that decides the leading mark.
// Running rows must replace their frame as the spinner advances, which is
// why the key is more than the width alone.
type PaintedRows = Option<((usize, bool, usize), Vec<Line<'static>>)>;

enum Kind {
    // One logical line of a prompt the user said: the border and the body
    // kept apart, so wrapping can repeat the border on every screen row the
    // body spans. A single border in the text would be cut at the first
    // wrap — the bar would end mid-air and the rest of the line would run
    // flush against the left edge.
    Said {
        // The rule and its column: `SAID_RULE` in the prompt colour.
        border: Line<'static>,
        // The line's text, in the input style, without the border.
        body: Line<'static>,
    },
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
    // What a finished run left behind, kept as its numbers rather than as the
    // string they render to. The segments the config asks for and the theme
    // they are painted in both outlive the run, and a string frozen when it
    // ended answers to neither.
    Tally(Snapshot),
    // A block of reasoning that can be folded or unfolded.
    Reasoning {
        // Which block this row belongs to; the stream appends completed lines
        // to the open block's row and nothing else.
        block: u64,
        lines: Vec<Line<'static>>,
        folded: bool,
    },
    // A bundle of read-only tool results folded together into a summary row.
    ToolsSummary {
        tools: FoldedTools,
        folded: bool,
        running: bool,
        hovered: bool,
        spinner: usize,
        painted: RefCell<PaintedRows>,
    },
}

impl Row {
    // A row whose height is not measured yet; every constructor starts here.
    fn new(kind: Kind) -> Self {
        Self(kind, Height::default())
    }

    /// Something only the screen ever knew. Free-form on purpose — no session
    /// entry answers for it, so nothing can drift.
    pub fn notice(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
        })
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

    /// One reasoning line. A reasoning line lives inside a block's row, not
    /// beside it.
    pub fn reasoning_line(line: &str, paint: &Paint) -> Line<'static> {
        Line::from(paint.span(&paint.theme.muted, line))
    }

    /// A whole assistant text block, for a caller that has one.
    pub fn answer(text: &str, paint: &Paint) -> Vec<Self> {
        render::render_markdown(text, paint)
            .into_iter()
            .map(Self::notice)
            .collect()
    }

    /// The line a finished run ends on.
    pub fn tally(snap: Snapshot) -> Self {
        Self::new(Kind::Tally(snap))
    }

    /// A bundle of read-only tool results folded together into a summary row.
    pub fn tools_summary(tools: FoldedTools) -> Self {
        Self::new(Kind::ToolsSummary {
            tools,
            folded: true,
            running: false,
            hovered: false,
            spinner: 0,
            painted: RefCell::new(None),
        })
    }

    /// Add a tool to a tools summary row, if it is one.
    pub fn push_tool(&mut self, name: String, preview: String) -> bool {
        if let Kind::ToolsSummary { tools, painted, .. } = &mut self.0 {
            tools.push(FoldedTool { name, preview });
            *painted.borrow_mut() = None;
            self.1.clear();
            true
        } else {
            false
        }
    }

    /// Whether this row is a folded tools summary row.
    pub fn is_tools_summary(&self) -> bool {
        matches!(&self.0, Kind::ToolsSummary { .. })
    }

    /// Whether this row is an expandable row.
    pub fn is_expandable(&self) -> bool {
        match &self.0 {
            // One tool is its own summary line, so unfolding would only
            // repeat it.
            Kind::ToolsSummary { tools, .. } => !tools.is_single(),
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
            Kind::ToolsSummary {
                folded, painted, ..
            } => {
                *folded = !*folded;
                *painted.borrow_mut() = None;
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

    /// Update running, hovered, and spinner state for expandable rows.
    pub fn update_hover_state(&mut self, running: bool, hovered: bool, spin: usize) {
        match &mut self.0 {
            Kind::ToolsSummary {
                running: r,
                hovered: h,
                spinner: s,
                painted,
                ..
            } => {
                // The rendered rows key on (width, running, spinner);
                // hovered restyles, never re-measures — drop the painted
                // cache, keep the height.
                let hover_changed = *h != hovered;
                if *r != running || *h != hovered || *r && *s != spin {
                    *r = running;
                    *h = hovered;
                    *s = spin;
                    if hover_changed {
                        *painted.borrow_mut() = None;
                    }
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

    /// A reasoning block's first row. Later lines go in through `push_line`.
    pub fn reasoning(block: u64, lines: Vec<Line<'static>>, folded: bool) -> Self {
        Self::new(Kind::Reasoning {
            block,
            lines,
            folded,
        })
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

    // One logical line of a prompt the user said: the rule it wears, and the
    // text under it. The border lives apart from the body so the screen can
    // repeat it on every row the body wraps to — see `Kind::Said`.
    fn said(border: Line<'static>, body: Line<'static>) -> Self {
        Self::new(Kind::Said { border, body })
    }

    /// A prompt's lines as the stream echoed them: a `!` keeps its own mark
    /// and its continuation the plain indent, everything else wears the rule,
    /// unbroken down every line of it.
    pub fn prompt(text: &str, paint: &Paint) -> Vec<Self> {
        if text.starts_with('!') {
            // A `!` is a command, not something said: the bang takes the
            // prompt's place, and the lines under it keep the plain indent.
            let bang = paint.span(
                &paint.theme.prompt.color,
                format!("{} ", crate::icons::BANG_SIGIL),
            );
            let mut rows = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let (prefix, body) = if i == 0 {
                    (
                        bang.clone(),
                        line.strip_prefix('!').unwrap_or(line).trim_start(),
                    )
                } else {
                    (Span::raw("  "), line)
                };
                rows.push(Self::notice(Line::from(vec![
                    prefix,
                    paint.span(&paint.theme.input, body),
                ])));
            }
            return rows;
        }
        // Unbroken down every line said: the icon marks what is being typed,
        // and a landed line wearing it reads as another place to type. The
        // border is kept apart from the body so wrapping can repeat it.
        let border =
            Line::from(paint.span(&paint.theme.prompt.color, format!("{} ", icons::SAID_RULE)));
        text.lines()
            .map(|line| {
                Self::said(
                    border.clone(),
                    Line::from(paint.span(&paint.theme.input, line)),
                )
            })
            .collect()
    }

    /// How many logical lines the row is made of — the `i`s `line` answers.
    /// Wraps not counted; `height` is the screen-row count.
    pub fn len(&self) -> usize {
        match &self.0 {
            Kind::Notice { .. } | Kind::Said { .. } | Kind::Tally(_) => 1,
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
            Kind::Reasoning { lines, folded, .. } => {
                if *folded {
                    1
                } else {
                    lines.len()
                }
            }
            Kind::ToolsSummary { tools, folded, .. } => {
                if *folded {
                    1
                } else {
                    1 + tools.count()
                }
            }
        }
    }

    /// Forget the remembered height: the text this row renders from changed
    /// underneath it, without going through any constructor.
    pub fn clear_height(&mut self) {
        self.1.clear();
    }

    /// Screen rows this row takes at `width`, wraps included, remembered by
    /// width until the content changes. The scrolled-up view's accounting
    /// reads these; a wrap is a row the count has to know about.
    pub fn height(&self, paint: &Paint, done: &[Segment], width: usize) -> usize {
        if let Some(total) = self.1.total(width) {
            return total;
        }
        (0..self.len())
            .map(|i| self.line_height(i, paint, done, width))
            .sum()
    }

    /// Screen rows logical line `i` takes at `width`, from the same
    /// measurement `height` keeps. The window's walk reads one of these per
    /// line it passes, so a line it is not going to show is counted without
    /// being wrapped.
    pub fn line_height(&self, i: usize, paint: &Paint, done: &[Segment], width: usize) -> usize {
        if let Some(h) = self.1.line(width, i) {
            return h;
        }
        let (line, border) = self.line(i, paint, done, width);
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
        done: &[Segment],
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        match &self.0 {
            Kind::Said { border, body } => (body.clone(), Some(border.clone())),
            Kind::Notice { text, times } if *times == 1 => (text.clone(), None),
            Kind::Notice { text, times } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning and a tally.
                let mut spans = text.spans.clone();
                spans.push(paint.span(&paint.theme.muted, format!(" ×{times}")));
                (Line::from(spans), None)
            }
            Kind::Tally(snap) => (
                Line::from(paint.span(&paint.theme.muted, status::line(done, snap))),
                None,
            ),
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
            Kind::Reasoning { lines, folded, .. } => {
                let text = if *folded {
                    // The count row is synthesized at draw time, so it takes
                    // its muted styling here rather than from a painted row.
                    Line::from(paint.span(&paint.theme.muted, thinking_summary(lines.len())))
                } else {
                    lines[i].clone()
                };
                (text, None)
            }
            Kind::ToolsSummary {
                tools,
                folded,
                running,
                hovered,
                spinner,
                painted,
            } => {
                let mut painted = painted.borrow_mut();
                let rows = match &mut *painted {
                    Some((key, rows)) if *key == (width, *running, *spinner) => rows,
                    slot => {
                        let rows = tools_summary_rows(
                            tools, *folded, *running, *hovered, *spinner, paint, width,
                        );
                        &mut slot.insert(((width, *running, *spinner), rows)).1
                    }
                };
                (rows.get(i).cloned().unwrap_or_default(), None)
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
            self.1.clear();
        }
    }

    /// Whether this row is an empty reasoning block.
    pub fn is_empty_reasoning(&self) -> bool {
        match &self.0 {
            Kind::Reasoning { lines, .. } => lines.is_empty(),
            _ => false,
        }
    }

    /// Append a finished line to a reasoning block. A no-op on anything else,
    /// which no caller can reach: the only handle to a row is one found by
    /// `block()`, and only a reasoning row answers that.
    pub fn push_line(&mut self, line: Line<'static>) {
        if let Kind::Reasoning { lines, .. } = &mut self.0 {
            lines.push(line);
            self.1.clear();
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
        let muted = |line: &str| Self::notice(Line::from(paint.span(&paint.theme.muted, line)));
        let mut rows = vec![muted(icons::VERSION_BANNER)];
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
    format!("{} {}", icons::PENDING_MARK, named(name, summary))
}

// The count line a shut thinking block leaves in the scrollback.
fn thinking_summary(n: usize) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("thinking{}{n} line{s}", icons::PART_SEP)
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

pub fn tools_summary_header(
    tools: &FoldedTools,
    folded: bool,
    running: bool,
    hovered: bool,
    spinner: usize,
    paint: &Paint,
    width: usize,
) -> Line<'static> {
    // A finished batch wears a green check, an in-flight one spins — each
    // its own span, so hover bolds instead of recolouring. Folded keeps the
    // latest tool and its count; unfolded, the count would repeat the list.
    let body = if folded && !tools.is_single() {
        let digits = tools.count().to_string();
        let desc = tools.last().desc();
        // The ellipsis is glued to the count — `…12` — with a space before
        // it and a column of air before the terminal edge. One leading
        // space after the check.
        let sep_w = 1 + UnicodeWidthStr::width(icons::ELLIPSIS) + 1;
        let fixed_w = 1 + digits.len() + sep_w;
        let room = width.saturating_sub(fixed_w);
        let budget = room.clamp(8, 50);
        let desc_w = UnicodeWidthStr::width(desc.as_str());
        let shown = if desc_w > budget {
            clip_to(&desc, budget)
        } else {
            &desc
        };
        format!(" {shown} {}{digits}", icons::ELLIPSIS)
    } else {
        let room = width.saturating_sub(2).max(10);
        format!(" {}", clip_to(&tools.last().desc(), room))
    };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(2);
    if running {
        spans.push(Span::raw(
            icons::SPINNER_FRAMES[spinner % icons::SPINNER_FRAMES.len()],
        ));
    } else {
        spans.push(paint.span_hovered(hovered, &paint.theme.status.ok, icons::DONE_MARK));
    }
    // The body keeps its own leading space: the running row has no mark-span
    // to separate from it, and one space after the check is the look.
    spans.push(paint.span_hovered(hovered, &paint.theme.muted, body));
    Line::from(spans)
}

pub fn tools_summary_rows(
    tools: &FoldedTools,
    folded: bool,
    running: bool,
    hovered: bool,
    spinner: usize,
    paint: &Paint,
    width: usize,
) -> Vec<Line<'static>> {
    let header = tools_summary_header(tools, folded, running, hovered, spinner, paint, width);
    if folded {
        return vec![header];
    }
    let mut rows = Vec::with_capacity(1 + tools.count());
    rows.push(header);
    // The header already wears the check; the tools it lists need no
    // second one, and the indent keeps them under it.
    let room = width.saturating_sub(2).max(10);
    for tool in tools.iter() {
        rows.push(Line::from(paint.span(
            &paint.theme.muted,
            format!("  {}", clip_to(&tool.desc(), room)),
        )));
    }
    rows
}

#[cfg(test)]
mod tools_summary_tests {
    use super::*;

    fn tool(name: &str, preview: &str) -> FoldedTool {
        FoldedTool {
            name: name.to_string(),
            preview: preview.to_string(),
        }
    }

    fn bundle(tools: Vec<FoldedTool>) -> FoldedTools {
        let mut it = tools.into_iter();
        let first = it.next().expect("a bundle holds at least one tool");
        let mut bundle = FoldedTools::new(first);
        for tool in it {
            bundle.push(tool);
        }
        bundle
    }

    // The rendered rows are cached by (width, running, spinner), so a running
    // summary row must replace its frame as the spinner advances — a cache
    // keyed by width alone would freeze the row on its first frame.
    #[test]
    fn a_running_summary_row_advances_through_the_paint_cache() {
        let paint = Paint::new(true);
        let mut row = Row::tools_summary(bundle(vec![tool("read", "a.rs")]));

        use crate::tui::screen::plain;

        row.update_hover_state(true, false, 0);
        let f0 = plain(&row.line(0, &paint, &[], 80).0);
        row.update_hover_state(true, false, 1);
        let f1 = plain(&row.line(0, &paint, &[], 80).0);

        assert!(
            f0.starts_with(&format!("{} read a.rs", icons::SPINNER_FRAMES[0])),
            "got: {f0}"
        );
        assert!(
            f1.starts_with(&format!("{} read a.rs", icons::SPINNER_FRAMES[1])),
            "got: {f1}"
        );
        assert_ne!(f0, f1, "the running frame must advance, not freeze");
    }

    #[test]
    fn unfolded_tools_summary_shows_all_tools() {
        let tools = bundle(vec![
            tool("read", "crates/agent/src/session.rs"),
            tool("grep", "match 1"),
        ]);
        let mut row = Row::tools_summary(tools);
        assert_eq!(row.len(), 1);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 3);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 1);
    }

    #[test]
    fn a_single_tool_summary_never_unfolds() {
        let mut row = Row::tools_summary(bundle(vec![tool("read", "a.rs")]));
        assert!(!row.is_expandable());
        assert!(!row.toggle_expand());
        assert_eq!(row.len(), 1);
    }
    #[test]
    fn result_with_many_diff_lines_expands_and_collapses() {
        let mut preview = "crates/foo.rs +30 -0".to_string();
        for i in 1..=30 {
            preview.push_str(&format!("\n  {i} + line {i}"));
        }
        let mut row = Row::result(true, "edit", preview);
        assert_eq!(row.len(), 26);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 32);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 26);
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
        let done: Vec<Segment> = Vec::new();
        let row = Row::reasoning(1, vec![Line::from("abcdef"), Line::from("gh")], false);
        for width in [2usize, 4, 80] {
            let sum: usize = (0..row.len())
                .map(|i| row.line_height(i, &paint, &done, width))
                .sum();
            assert_eq!(sum, row.height(&paint, &done, width), "at width {width}");
        }
        // Asked a whole row at a time, then one line at a time: same answer.
        let fresh = Row::reasoning(1, vec![Line::from("abcdef"), Line::from("gh")], false);
        assert_eq!(fresh.height(&paint, &done, 2), 4, "three rows and one");
        assert_eq!(
            fresh.line_height(0, &paint, &done, 2),
            3,
            "abcdef wraps to three"
        );
        assert_eq!(fresh.line_height(1, &paint, &done, 2), 1);
        assert_eq!(fresh.height(&paint, &done, 2), 4);
    }
}
