//! One row of scrollback, the only place one can be made. `Kind` is
//! private, so the two producers (live stream, rebuild) can't disagree.

use std::cell::RefCell;

use llm::message::{ToolResult, ToolResultContent};
use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::block::{self, Form};
use crate::render::named;
use crate::render::{self, Paint};
use pi_core::core::resolve::Resolved;
use pi_store::icons;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedTool {
    pub name: String,
    pub preview: String,
    // What the step shows opened, under its line: the lines of the argument
    // past the first the line names (`asked` of them), then what came back.
    body: Vec<String>,
    asked: usize,
    open: bool,
}

impl FoldedTool {
    /// `asked` is the call's whole leading argument, `output` what it
    /// printed; both are what opening the step shows.
    pub fn new(name: &str, preview: &str, asked: &str, output: &str) -> Self {
        let mut body: Vec<String> = asked.lines().skip(1).map(str::to_string).collect();
        let asked = body.len();
        body.extend(pi_core::core::tools::shown(name, output).map(str::to_string));
        Self {
            name: name.to_string(),
            preview: preview.to_string(),
            body,
            asked,
            open: false,
        }
    }

    fn desc(&self) -> String {
        desc_of(&self.name, &self.preview)
    }

    // The lines the call printed, the count its line ends with.
    fn printed(&self) -> usize {
        self.body.len() - self.asked
    }
}

// A call still in flight, as the group that will fold it draws it: the
// row draws it from the moment it starts, so nothing jumps when it lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTool {
    /// The call's name and leading argument, read the way the live line read
    /// them — they already agree with the row the call will land as.
    pub name: String,
    pub preview: String,
    /// Whether the call has landed yet. A call that lands badly is never
    /// the row's — it's on its way to its own line.
    pub landed: bool,
    /// Whole seconds it has been out, once there are enough to be worth
    /// saying; `None` before that and once it has landed.
    pub secs: Option<u64>,
}

impl PendingTool {
    fn desc(&self) -> String {
        desc_of(&self.name, &self.preview)
    }
}

/// The frame a call in flight shows at spinner tick `tick`.
pub fn call_frame(tick: usize) -> &'static str {
    const TICKS_PER_FRAME: usize = 2;
    icons::CALL_FRAMES[tick / TICKS_PER_FRAME % icons::CALL_FRAMES.len()]
}

/// A call's text while it is out: muted, with a brighter band sweeping
/// across it one character a tick.
pub fn shimmer(text: &str, tick: usize, paint: &Paint) -> Vec<Span<'static>> {
    // How far past each end the band travels, so a sweep leaves the text at
    // rest for a moment before the next.
    const EDGE: usize = 4;
    if !paint.color {
        return vec![paint.span(&paint.theme.muted, text.to_string())];
    }
    let span = |lit: u8, run: String| match lit {
        2 => Span::styled(run, RStyle::default().add_modifier(Modifier::BOLD)),
        1 => Span::raw(run),
        _ => paint.span(&paint.theme.muted, run),
    };
    let at = (tick % (text.chars().count() + 2 * EDGE)) as isize - EDGE as isize;
    let mut spans = Vec::new();
    let (mut run, mut level) = (String::new(), 0);
    for (i, c) in text.chars().enumerate() {
        let lit = match (i as isize - at).unsigned_abs() {
            0 => 2,
            1 => 1,
            _ => 0,
        };
        if lit != level && !run.is_empty() {
            spans.push(span(level, std::mem::take(&mut run)));
        }
        level = lit;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(span(level, run));
    }
    spans
}

/// How long a call has been out, as its line says it: nothing for a call
/// quick enough to be over before the number could be read.
pub fn out_for(secs: Option<u64>) -> String {
    secs.map(|s| {
        format!(
            "{}{}",
            icons::PART_SEP,
            llm::figures::elapsed(std::time::Duration::from_secs(s))
        )
    })
    .unwrap_or_default()
}

// Whether the row still has a call out. Any of them, not the newest:
// the row is the only place a call in flight shows.
fn running(pending: &[PendingTool]) -> bool {
    pending.iter().any(|p| !p.landed)
}

// The lines of the block still streaming in the row, if one is.
fn thinking_now(steps: &[Step]) -> Option<usize> {
    steps.iter().find_map(|s| match s {
        Step::Thinking {
            lines, live: true, ..
        } => Some(lines.len()),
        _ => None,
    })
}

// How many calls the row speaks for: landed plus still out. Part of
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

// The one step of a group that holds nothing else. Such a group has no list
// to open: opening it opens the step.
fn lone<'a>(steps: &'a [Step], pending: &[PendingTool]) -> Option<&'a Step> {
    match (steps, pending) {
        ([step], []) => Some(step),
        _ => None,
    }
}

// Whether unfolding would show more than the row's own line.
fn expandable(steps: &[Step], pending: &[PendingTool]) -> bool {
    match lone(steps, pending) {
        Some(Step::Tool(t)) => !t.body.is_empty(),
        Some(Step::Thinking { lines, .. }) => !lines.is_empty(),
        None => steps.len() + pending.len() > 1,
    }
}

// The tool and its leading argument, named the one way every row that
// shows a tool does: name alone with nothing under it, dropped if redundant.
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

/// What an opened call's lines sit under its own line with.
pub const STEP_BODY_INDENT: &str = "    ";

/// One step of a group, in the order the run took it: a read-only call that
/// landed well, or a block of reasoning; each opens on its own.
pub enum Step {
    Tool(FoldedTool),
    Thinking {
        // The stream appends a block's lines to the row holding its id.
        block: u64,
        lines: Vec<Line<'static>>,
        open: bool,
        // Still streaming: the group is not done while it is.
        live: bool,
    },
}

pub struct Row(Kind, Height);

// The rows this row takes on screen at one width, wraps included —
// counted per logical line and cached until width or content changes.
#[derive(Default)]
struct Height(RefCell<Option<Measured>>);

struct Measured {
    width: usize,
    // One entry per logical line, `None` until wrapped. Kept per line so
    // a tall row is never wrapped whole just to answer for one line.
    lines: Vec<Option<usize>>,
}

impl Height {
    // Measurement at this width, room for `lines` of them. A moved width
    // restarts; a grown row keeps what's counted (lines can't shrink).
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

    // The row's count, once every line has been measured; a caller that
    // asks mid-measure fills the rest first (see `height`).
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
    // One logical line of a said prompt: border and body kept apart so
    // a wrap can repeat the border. `band` is the whole row's background.
    Said {
        // The rule and its column: `SAID_RULE` in the prompt colour.
        border: Line<'static>,
        // The line's text, in the input style, without the border.
        body: Line<'static>,
        band: Option<RStyle>,
    },
    // What the model answered, one row per markdown line. Its own kind,
    // not a notice — an answer is half the conversation, not screen-only.
    Answer(Line<'static>),
    // A closed mermaid block or a table, drawn for the width it is shown at:
    // a resize draws it again, or shows its source when it no longer fits.
    // One logical line whose `\n`s the wrap breaks.
    Block {
        form: Form,
        source: String,
        painted: RefCell<Option<(usize, Line<'static>)>>,
    },
    // A painted line the screen alone knows: banner, command output,
    // warning. `times` collapses an identical repeat into one row+count.
    Notice {
        text: Line<'static>,
        times: usize,
    },
    // A tool's result, kept as its parts; clipped only when a frame needs
    // it, since a row clipped to its landing width could never regrow.
    Result {
        ok: bool,
        name: String,
        preview: String,
        // The row count, so expandability and toggle don't re-walk the
        // preview string on every mouse move.
        preview_lines: usize,
        expanded: bool,
        hovered: bool,
        // Caches the last clip, keyed by width: painting the whole result
        // per row made redraws quadratic in the diff's size.
        painted: RefCell<Option<(usize, Vec<Line<'static>>)>>,
    },
    // The run's working behind one fold: read-only calls that landed
    // well and the reasoning around them, in order.
    Steps {
        steps: Vec<Step>,
        // Calls in flight this row draws: not in the scrollback yet, left
        // to the row by the live block, since this is where they'll land.
        pending: Vec<PendingTool>,
        folded: bool,
        // The line under the mouse, when it is one a click opens or closes.
        hovered: Option<usize>,
        // The tick the calls still out shimmer at.
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
        matches!(
            &self.0,
            Kind::Said { .. } | Kind::Answer(_) | Kind::Block { .. }
        )
    }

    /// Something only the screen ever knew (a tally line, a lane's
    /// answer, a warning), in the surface's own muted voice.
    pub fn notice(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
        })
    }

    /// Plain text as notice rows, one per line — shared by the live
    /// stream (which also files it) and a rebuild (from the archived text).
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

    /// A whole assistant text block, for a caller that has one. Its mermaid
    /// blocks are rows of their own, drawn at whatever width they get.
    pub fn answer(text: &str, paint: &Paint) -> Vec<Self> {
        let markdown = |text: &str| {
            render::render_markdown(text, paint)
                .into_iter()
                .map(|line| Self::new(Kind::Answer(line)))
        };
        if !paint.color {
            return markdown(text).collect();
        }
        let blank = || Self::new(Kind::Answer(Line::default()));
        let mut rows: Vec<Self> = Vec::new();
        for piece in block::pieces(text) {
            match piece {
                block::Piece::Text(text) => rows.extend(markdown(text)),
                block::Piece::Block(form, source) => {
                    if !rows.is_empty() {
                        rows.push(blank());
                    }
                    rows.push(Self::new(Kind::Block {
                        form,
                        source: source.trim_end().to_string(),
                        painted: RefCell::new(None),
                    }));
                    rows.push(blank());
                }
            }
        }
        if matches!(rows.last(), Some(Self(Kind::Answer(l), _)) if l.spans.is_empty()) {
            rows.pop();
        }
        rows
    }

    /// A group whose first step is `first`, folded the way it is born.
    pub fn steps(first: Step, folded: bool) -> Self {
        Self::new(Kind::Steps {
            steps: vec![first],
            pending: Vec::new(),
            folded,
            hovered: None,
            spinner: 0,
        })
    }

    /// A lone block of reasoning, as a group of one.
    #[cfg(test)]
    pub fn thinking(block: u64, lines: Vec<Line<'static>>, folded: bool) -> Self {
        Self::steps(
            Step::Thinking {
                block,
                lines,
                open: false,
                live: false,
            },
            folded,
        )
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

    /// The indent reasoning block `id` shows its lines under, if this group
    /// shows them; the block's unfinished line follows them there.
    pub fn shows_block(&self, id: u64) -> Option<&'static str> {
        let Kind::Steps {
            steps,
            pending,
            folded: false,
            ..
        } = &self.0
        else {
            return None;
        };
        let held = |s: &Step| matches!(s, Step::Thinking { block, .. } if *block == id);
        match lone(steps, pending) {
            Some(s) => held(s).then_some(""),
            None => steps
                .iter()
                .any(|s| held(s) && s.is_open())
                .then_some(STEP_INDENT),
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

    /// The line a click on line `i` opens or closes, if any: a result opens
    /// from anywhere on it, a group from its own line or a step's.
    pub fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        match &self.0 {
            Kind::Result { .. } => self.is_expandable().then_some(0),
            Kind::Steps {
                steps,
                pending,
                folded,
                ..
            } => {
                let listed = !*folded && lone(steps, pending).is_none();
                let step = || match unit_at(steps, pending, i - 1)? {
                    (Unit::Step(s), 0) => Some(s),
                    _ => None,
                };
                if i == 0 {
                    expandable(steps, pending).then_some(0)
                } else if listed && step().is_some_and(|s| s.opens(width)) {
                    Some(i)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Open or close what line `i` heads — see `click_line` — and say
    /// whether anything moved.
    pub fn toggle_at(&mut self, i: usize, width: usize) -> bool {
        if i == 0 || !self.is_steps() {
            return self.toggle_expand();
        }
        if self.click_line(i, width) != Some(i) {
            return false;
        }
        let Kind::Steps { steps, .. } = &mut self.0 else {
            return false;
        };
        let Some(k) = step_index(steps, i - 1) else {
            return false;
        };
        steps[k].flip();
        self.1.clear();
        true
    }

    /// Brings a row up to date before the frame draws: hover line, spinner
    /// tick, and the calls in flight this row draws.
    pub fn update_live(&mut self, hovered: Option<usize>, spin: usize, held: &[PendingTool]) {
        match &mut self.0 {
            Kind::Steps {
                pending,
                hovered: h,
                spinner,
                ..
            } => {
                // A call starting, ending or landing can change the row's
                // count; a tick alone can't — lines are clipped to width.
                if pending.as_slice() != held {
                    let reshaped = pending.len() != held.len()
                        || pending.iter().zip(held).any(|(a, b)| {
                            (&a.name, &a.preview, a.landed) != (&b.name, &b.preview, b.landed)
                        });
                    *pending = held.to_vec();
                    if reshaped {
                        self.1.clear();
                    }
                }
                // Neither moves a height: hover bolds, and bold is no wider;
                // a frame is one column like the next.
                *h = hovered;
                *spinner = spin;
            }
            Kind::Result {
                hovered: h,
                preview_lines,
                painted,
                ..
            } if *preview_lines > render::SKETCHED_ROWS && *h != hovered.is_some() => {
                *h = hovered.is_some();
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

    /// The row for a stored result: the tool's own sketch if it made one
    /// (`ToolResult` lacks it), else the content's first line.
    pub fn stored_result(r: &ToolResult, preview: Option<&str>) -> Self {
        let preview = preview.map(str::to_string).unwrap_or_else(|| {
            let body = result_text(r);
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

    /// The line a call that was never answered keeps: stopped before it landed.
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

    /// A prompt's lines as the stream echoed them: the input bar, on a
    /// band one step off where it was typed, so it reads as typed there.
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
            Kind::Answer(_) | Kind::Block { .. } | Kind::Notice { .. } | Kind::Said { .. } => 1,
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
                    return 1;
                }
                match lone(steps, pending) {
                    // The block is the group: its lines, with no line above.
                    Some(Step::Thinking { lines, .. }) => lines.len(),
                    Some(Step::Tool(t)) => 1 + t.body.len(),
                    None => 1 + steps.iter().map(Step::len).sum::<usize>() + pending.len(),
                }
            }
        }
    }

    /// Screen rows this row takes at `width`, wraps included, cached by
    /// width. The scrolled-up view's accounting reads these.
    pub fn height(&self, paint: &Paint, width: usize) -> usize {
        if let Some(total) = self.1.total(width) {
            return total;
        }
        (0..self.len())
            .map(|i| self.line_height(i, paint, width))
            .sum()
    }

    /// Screen rows logical line `i` takes, from the same cache `height`
    /// keeps — a line never shown is counted without being wrapped.
    pub fn line_height(&self, i: usize, paint: &Paint, width: usize) -> usize {
        if let Some(h) = self.1.line(width, i) {
            return h;
        }
        let (line, border) = self.line(i, paint, width);
        let h = super::screen::wrap(border.as_ref(), &line, width).len();
        self.1.set(width, i, h);
        h
    }

    /// What the screen renders for row `i` at this width: text, plus the
    /// border continuation rows repeat (a said row only; others have none).
    pub fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        match &self.0 {
            Kind::Said { border, body, .. } => (body.clone(), Some(border.clone())),
            Kind::Answer(text) => (text.clone(), None),
            Kind::Block {
                form,
                source,
                painted,
            } => {
                let mut held = painted.borrow_mut();
                if let Some((at, line)) = held.as_ref()
                    && *at == width
                {
                    return (line.clone(), None);
                }
                let line = block_at(*form, source, paint, width);
                *held = Some((width, line.clone()));
                (line, None)
            }
            Kind::Notice { text, times } if *times == 1 => (text.clone(), None),
            Kind::Notice { text, times } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning.
                let mut spans = super::screen::spans_of(text);
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
                let hover = |at: usize| *hovered == Some(at);
                let under = |indent: &'static str| Some(Line::from(indent));
                match lone(steps, pending) {
                    Some(Step::Thinking { lines, .. }) if open => {
                        let mut line = lines[i].clone();
                        // Bold as `span_hovered` bolds: only where there is colour.
                        if hover(i) && paint.color {
                            for span in &mut line.spans {
                                span.style = span.style.add_modifier(Modifier::BOLD);
                            }
                        }
                        (line, None)
                    }
                    Some(Step::Tool(t)) if open && i > 0 => {
                        (body_line(t, i - 1, paint), under(STEP_INDENT))
                    }
                    _ if !open || i == 0 => (
                        steps_header(steps, pending, open, hover(0), *spinner, paint, width),
                        None,
                    ),
                    _ => step_line(steps, pending, i - 1, hover(i), *spinner, paint, width),
                }
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

    /// Block `id` ended: folds to its first line if the group is folded,
    /// stays open if not; goes if it never had a line.
    pub fn end_block(&mut self, id: u64) -> bool {
        let Kind::Steps { steps, folded, .. } = &mut self.0 else {
            return false;
        };
        let hidden = *folded;
        steps.retain_mut(|s| match s {
            Step::Thinking {
                block,
                lines,
                open,
                live,
            } if *block == id => {
                *open = !hidden && *open;
                *live = false;
                !lines.is_empty()
            }
            _ => true,
        });
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

    /// What the screen opens with: version, endpoint, instruction files.
    /// Built fresh (not stored), so a reload theme change can repaint it.
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

/// What a result says in text, its other parts left out.
pub fn result_text(r: &ToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_start_line(name: &str, summary: &str) -> String {
    format!("{} {}", icons::STOPPED_MARK, named(name, summary))
}

/// "N thing(s)": the counts a line ends with.
pub(super) fn count(n: usize, thing: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {thing}{s}")
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

// `text` in `room` columns, closed by `…` when any of it was cut or `more`
// follows it: the mark sits on the text, never apart from it.
fn clipped(text: &str, room: usize, more: bool) -> String {
    if !more && UnicodeWidthStr::width(text) <= room {
        return text.to_string();
    }
    format!(
        "{}{}",
        clip_to(text, room.saturating_sub(1)),
        icons::ELLIPSIS
    )
}

// The mark a line naming a step or a call leads with.
#[derive(Clone, Copy)]
enum Mark {
    Done,
    // Out, turning and its text shimmering at this spinner tick.
    Out(usize),
    // Reasoning wears none: a call is the thing that gets a mark.
    Thought,
}

impl Mark {
    // The mark and the space after it, or nothing.
    fn spans(self, hovered: bool, paint: &Paint) -> Vec<Span<'static>> {
        let (glyph, style) = match self {
            Mark::Done => (icons::DONE_MARK, &paint.theme.status.ok),
            Mark::Out(tick) => (call_frame(tick), &paint.theme.muted),
            Mark::Thought => return Vec::new(),
        };
        vec![
            paint.span_hovered(hovered, style, glyph),
            paint.span_hovered(hovered, &paint.theme.muted, " "),
        ]
    }

    fn width(self) -> usize {
        match self {
            Mark::Thought => 0,
            _ => 2,
        }
    }
}

// What one line naming a step or a call says, before it is fitted to a width.
struct Head {
    mark: Mark,
    text: String,
    // What follows the text, a count or a time, kept whole when the text is cut.
    tail: String,
    // Whether more of the text is there than this line, whatever the width.
    more: bool,
}

impl Head {
    // The text's columns: the mark and its space, the tail, and a column of
    // air before the terminal edge.
    fn room(&self, width: usize) -> usize {
        width
            .saturating_sub(self.mark.width() + UnicodeWidthStr::width(self.tail.as_str()) + 1)
            .max(8)
    }

    fn cut(&self, width: usize) -> bool {
        UnicodeWidthStr::width(self.text.as_str()) > self.room(width)
    }

    // Opened, the text is whole and wraps, and what the tail said is below it.
    fn line(&self, open: bool, hovered: bool, paint: &Paint, width: usize) -> Line<'static> {
        if let Mark::Out(tick) = self.mark
            && !open
            && !hovered
        {
            let text = clipped(&self.text, self.room(width), self.more);
            let mut spans = self.mark.spans(false, paint);
            spans.extend(shimmer(&text, tick, paint));
            spans.push(paint.span(&paint.theme.muted, self.tail.clone()));
            return Line::from(spans);
        }
        let text = if open {
            self.text.clone()
        } else {
            let text = clipped(&self.text, self.room(width), self.more);
            format!("{text}{}", self.tail)
        };
        // The mark and the text are spans of their own, so hover bolds
        // instead of recolouring.
        let mut spans = self.mark.spans(hovered, paint);
        spans.push(paint.span_hovered(hovered, &paint.theme.muted, text));
        Line::from(spans)
    }
}

// " · N lines", or nothing for none.
fn lines_tail(n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    format!("{}{}", icons::PART_SEP, count(n, "line"))
}

fn pending_head(p: &PendingTool, tick: usize) -> Head {
    Head {
        mark: if p.landed {
            Mark::Done
        } else {
            Mark::Out(tick)
        },
        text: p.desc(),
        tail: out_for(p.secs),
        more: false,
    }
}

impl Step {
    fn is_open(&self) -> bool {
        match self {
            Step::Tool(t) => t.open,
            Step::Thinking { open, .. } => *open,
        }
    }

    fn flip(&mut self) {
        match self {
            Step::Tool(t) => t.open = !t.open,
            Step::Thinking { open, .. } => *open = !*open,
        }
    }

    // The lines an opened step shows under its own.
    fn body_len(&self) -> usize {
        match self {
            Step::Tool(t) => t.body.len(),
            Step::Thinking { lines, .. } => lines.len().saturating_sub(1),
        }
    }

    // The lines it takes in an unfolded group.
    fn len(&self) -> usize {
        1 + if self.is_open() { self.body_len() } else { 0 }
    }

    fn head(&self) -> Head {
        match self {
            Step::Tool(t) => Head {
                mark: Mark::Done,
                text: t.desc(),
                tail: lines_tail(t.printed()),
                more: t.asked > 0,
            },
            Step::Thinking { lines, .. } => Head {
                mark: Mark::Thought,
                text: lines
                    .first()
                    .map_or_else(|| super::THINKING.to_string(), super::screen::plain),
                tail: if lines.len() > 1 {
                    lines_tail(lines.len())
                } else {
                    String::new()
                },
                more: lines.len() > 1,
            },
        }
    }

    // Whether its line opens: there is more under it, or more of it than
    // the width shows.
    fn opens(&self, width: usize) -> bool {
        self.is_open()
            || self.body_len() > 0
            || self.head().cut(width.saturating_sub(STEP_INDENT.len()))
    }
}

// What line `i` of an unfolded group's list sits in, and how far into it.
enum Unit<'a> {
    Step(&'a Step),
    Pending(&'a PendingTool),
}

fn unit_at<'a>(
    steps: &'a [Step],
    pending: &'a [PendingTool],
    i: usize,
) -> Option<(Unit<'a>, usize)> {
    match walk(steps, i) {
        Ok((k, at)) => Some((Unit::Step(&steps[k]), at)),
        Err(rest) => pending.get(rest).map(|p| (Unit::Pending(p), 0)),
    }
}

// Which step line `i` of the list falls in and how far into it, or how far
// past the last step it lies.
fn walk(steps: &[Step], mut i: usize) -> Result<(usize, usize), usize> {
    for (k, step) in steps.iter().enumerate() {
        match i.checked_sub(step.len()) {
            Some(rest) => i = rest,
            None => return Ok((k, i)),
        }
    }
    Err(i)
}

// The step whose own line is line `i` of the list.
fn step_index(steps: &[Step], i: usize) -> Option<usize> {
    match walk(steps, i) {
        Ok((k, 0)) => Some(k),
        _ => None,
    }
}

// A group's own line: lists its steps, or (folded) names its newest
// call so the line holds still when the result lands.
fn steps_header(
    steps: &[Step],
    pending: &[PendingTool],
    open: bool,
    hovered: bool,
    tick: usize,
    paint: &Paint,
    width: usize,
) -> Line<'static> {
    let thought = thought(steps);
    let calls = calls(steps, pending);
    let muted = |text: String| paint.span_hovered(hovered, &paint.theme.muted, text);
    // The group's mark, not its newest call's: a block still streaming
    // after the last call landed means the group is not done.
    let live = thinking_now(steps).filter(|_| !running(pending));
    let mark = if running(pending) || live.is_some() {
        Mark::Out(tick)
    } else {
        Mark::Done
    };
    let thinking = |n: usize| match n {
        0 => super::THINKING.to_string(),
        n => format!("thinking {}", count(n, "line")),
    };
    if open && lone(steps, pending).is_none() {
        let mut parts: Vec<String> = Vec::new();
        if calls > 0 {
            parts.push(count(calls, "call"));
        }
        parts.extend(thought.map(thinking));
        let text = parts.join(icons::PART_SEP);
        if calls == 0 {
            return Line::from(muted(text));
        }
        let head = Head {
            mark,
            text,
            tail: String::new(),
            more: false,
        };
        return head.line(true, hovered, paint, width);
    }
    if let Some(n) = live
        && calls > 0
    {
        let mut tail = lines_tail(n);
        tail.push_str(&format!("{}{}", icons::PART_SEP, count(calls, "call")));
        let head = Head {
            mark,
            text: super::THINKING.to_string(),
            tail,
            more: false,
        };
        return head.line(open, hovered, paint, width);
    }
    let newest = pending.last().map(|p| pending_head(p, tick)).or_else(|| {
        steps
            .iter()
            .rev()
            .find(|s| matches!(s, Step::Tool(_)))
            .map(Step::head)
    });
    let Some(newest) = newest else {
        let text = match thought {
            Some(n) if n > 0 => format!("thinking{}{}", icons::PART_SEP, count(n, "line")),
            _ => super::THINKING.to_string(),
        };
        return Line::from(muted(text));
    };
    let mut tail = pending.last().map(|p| out_for(p.secs)).unwrap_or_default();
    if calls > 1 {
        tail.push_str(&format!("{}{}", icons::PART_SEP, count(calls, "call")));
    }
    if let Some(n) = thought {
        tail.push_str(&format!("{}{}", icons::PART_SEP, thinking(n)));
    }
    let head = Head {
        mark,
        tail,
        ..newest
    };
    head.line(open, hovered, paint, width)
}

// A line of what an opened call shows: the argument's own lines in the
// group's voice, then what came back in the plain one.
fn body_line(t: &FoldedTool, j: usize, paint: &Paint) -> Line<'static> {
    let text = t.body.get(j).cloned().unwrap_or_default();
    if j < t.asked {
        Line::from(paint.span(&paint.theme.muted, text))
    } else {
        Line::from(text)
    }
}

// Line `i` of an unfolded group's list, the calls in flight last, with the
// indent it sits under as the border a wrap repeats.
fn step_line(
    steps: &[Step],
    pending: &[PendingTool],
    i: usize,
    hovered: bool,
    tick: usize,
    paint: &Paint,
    width: usize,
) -> (Line<'static>, Option<Line<'static>>) {
    let room = width.saturating_sub(STEP_INDENT.len());
    let (line, indent) = match unit_at(steps, pending, i) {
        Some((Unit::Step(s), 0)) => (
            s.head().line(s.is_open(), hovered, paint, room),
            STEP_INDENT,
        ),
        Some((Unit::Step(Step::Tool(t)), j)) => (body_line(t, j - 1, paint), STEP_BODY_INDENT),
        // No mark to sit past: the block's lines run under its first.
        Some((Unit::Step(Step::Thinking { lines, .. }), j)) => (lines[j].clone(), STEP_INDENT),
        Some((Unit::Pending(p), _)) => (
            pending_head(p, tick).line(false, false, paint, room),
            STEP_INDENT,
        ),
        None => (Line::default(), STEP_INDENT),
    };
    (line, Some(Line::from(indent)))
}

#[cfg(test)]
mod conversation_tests {
    use super::*;

    // Browse mode is the whole reason the answer has a kind of its own:
    // it shows said/answered, nothing the conversation passed through.
    #[test]
    fn only_what_was_said_and_what_was_answered_is_the_conversation() {
        let paint = Paint::new(false);
        for row in Row::prompt("what is this", &paint)
            .into_iter()
            .chain(Row::answer("# it is this\nand that", &paint))
        {
            assert!(row.is_conversation());
        }
        let tool = Step::Tool(FoldedTool::new("read", "src/main.rs", "", ""));
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
        FoldedTool::new(name, preview, "", "")
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
            secs: None,
        }
    }

    fn calls_tail(n: usize) -> String {
        format!("{}{n} calls", icons::PART_SEP)
    }

    // Calls end in whatever order; the row is the only place one in
    // flight shows, so it stays pending however the named call ended.
    #[test]
    fn a_call_still_out_keeps_the_row_pending() {
        let paint = Paint::new(true);
        let mut row = bundle(vec![tool("read", "a.rs")]);

        use crate::tui::screen::plain;

        row.update_live(
            None,
            0,
            &[
                pending("bash", "cargo test", false),
                pending("read", "b.rs", true),
            ],
        );
        let line = plain(&row.line(0, &paint, 80).0);

        assert_eq!(
            line,
            format!("{} read b.rs{}", call_frame(0), calls_tail(3)),
            "it names the newest, and stays pending for the one still out"
        );
    }

    // A row can lose the call that made it unfoldable (a failure gets
    // its own line); with one call left it shows folded and won't toggle.
    #[test]
    fn a_row_left_with_one_call_shows_as_folded() {
        let mut row = bundle(vec![tool("read", "a.rs")]);
        row.update_live(None, 0, &[pending("grep", "match 1", false)]);
        assert!(row.is_expandable(), "two calls are a batch");
        assert!(row.toggle_expand());
        assert_eq!(row.len(), 3, "the header and both calls");

        row.update_live(None, 0, &[]);
        assert_eq!(row.len(), 1, "folded back to the one tool it holds");
        assert!(!row.is_expandable());
    }

    #[test]
    fn a_single_tool_with_nothing_under_it_never_unfolds() {
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(!row.is_expandable());
        assert!(!row.toggle_expand());
        assert_eq!(row.len(), 1);
    }

    fn think(block: u64, n: usize) -> Step {
        Step::Thinking {
            block,
            lines: (1..=n).map(|i| Line::from(format!("line {i}"))).collect(),
            open: false,
            live: false,
        }
    }

    // Each line as the screen draws it, border included.
    fn shown(row: &Row, width: usize) -> Vec<String> {
        use crate::tui::screen::plain;
        let paint = Paint::new(false);
        (0..row.len())
            .map(|i| {
                let (line, border) = row.line(i, &paint, width);
                border.map(|b| plain(&b)).unwrap_or_default() + &plain(&line)
            })
            .collect()
    }

    // Folded is one line; unfolded, a line per step under a counting
    // line — each step opens alone, named by the click's line.
    #[test]
    fn a_group_unfolds_to_its_steps_and_each_step_opens_alone() {
        let ran = FoldedTool::new("bash", "cat <<EOF", "cat <<EOF\nhi\nEOF", "hi\n");
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(row.join(think(1, 3)).is_ok());
        assert!(row.join(Step::Tool(ran)).is_ok());
        let (done, dots, sep) = (icons::DONE_MARK, icons::ELLIPSIS, icons::PART_SEP);
        assert_eq!(
            shown(&row, 80),
            [format!(
                "{done} bash cat <<EOF{dots}{}{sep}thinking 3 lines",
                calls_tail(2)
            )]
        );
        assert!(row.toggle_expand());
        let unfolded = [
            format!("{done} 2 calls{sep}thinking 3 lines"),
            format!("  {done} read a.rs"),
            format!("  line 1{dots}{sep}3 lines"),
            format!("  {done} bash cat <<EOF{dots}{sep}1 line"),
        ];
        assert_eq!(shown(&row, 80), unfolded);

        assert_eq!(row.click_line(1, 80), None, "nothing under the read");
        assert!(row.toggle_at(2, 80), "the reasoning opens");
        assert_eq!(row.len(), 6);
        assert!(row.toggle_at(5, 80), "the call past it still answers");
        assert_eq!(
            shown(&row, 80)[2..],
            [
                "  line 1".into(),
                "  line 2".into(),
                "  line 3".into(),
                format!("  {done} bash cat <<EOF"),
                "    hi".into(),
                "    EOF".into(),
                "    hi".into(),
            ]
        );
        assert_eq!(row.click_line(6, 80), None, "a body line opens nothing");
        assert!(row.toggle_at(2, 80) && row.toggle_at(3, 80));
        assert_eq!(shown(&row, 80), unfolded);
    }

    // A group of one step has no list to open: opening it opens the step.
    #[test]
    fn a_group_of_one_opens_its_step() {
        let mut row = Row::steps(think(1, 2), true);
        assert_eq!(
            shown(&row, 80),
            [format!("thinking{}2 lines", icons::PART_SEP)]
        );
        assert!(row.toggle_expand());
        assert_eq!(shown(&row, 80), ["line 1", "line 2"]);

        let mut one = bundle(vec![FoldedTool::new(
            "read",
            "a.rs",
            "a.rs",
            "fn main() {}",
        )]);
        assert!(one.toggle_at(0, 80));
        assert_eq!(
            shown(&one, 80),
            [
                format!("{} read a.rs", icons::DONE_MARK),
                "  fn main() {}".into()
            ]
        );

        // Before its first line lands there is nothing to unfold.
        let empty = Row::steps(think(2, 0), false);
        assert!(!empty.is_expandable());
        assert_eq!(shown(&empty, 80), [crate::tui::THINKING]);
    }

    // One call or many, the text runs up to what follows it, `…` on its end.
    #[test]
    fn a_long_call_fills_the_row_before_what_follows_it() {
        let long = "x".repeat(200);
        let mut row = bundle(vec![tool("bash", &long), tool("bash", &long)]);
        assert!(row.join(think(1, 1)).is_ok());
        let line = &shown(&row, 80)[0];
        let tail = format!(
            "{}{}{}thinking 1 line",
            icons::ELLIPSIS,
            calls_tail(2),
            icons::PART_SEP
        );
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

    // The window reads one line's height at a time (skipping rows a
    // scrolled view hides); both paths must agree on where a row ends.
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

// A block row at `width`: drawn when it fits, else its source under a badge
// saying what it would need. A mermaid block wears its badge either way.
fn block_at(form: Form, source: &str, paint: &Paint, width: usize) -> Line<'static> {
    use crate::block::{Drawn, draw, note};
    let joined = |lines: Vec<Line<'static>>| {
        let mut spans = Vec::new();
        for (at, line) in lines.into_iter().enumerate() {
            if at > 0 {
                spans.push(Span::raw("\n"));
            }
            spans.extend(line.spans);
        }
        Line::from(spans)
    };
    let source_under = |lang: &str, note: &str| {
        let mut spans = render::badge(lang, note, paint).spans;
        spans.push(Span::raw(format!("\n{source}")));
        Line::from(spans)
    };
    match form {
        Form::Mermaid => match draw(source, Some(width)) {
            Drawn::Fits(diagram) => {
                let mut spans = render::badge("mermaid", "", paint).spans;
                spans.push(Span::raw(format!("\n{diagram}")));
                Line::from(spans)
            }
            Drawn::Wide { needs, has } => source_under("mermaid", &note(needs, has)),
            Drawn::Unread => source_under("mermaid", ""),
        },
        Form::Table => {
            let lines = render::render_markdown(source, paint);
            let needs = lines.iter().map(Line::width).max().unwrap_or(0);
            if needs <= width {
                joined(lines)
            } else {
                source_under("table", &note(needs, width))
            }
        }
    }
}
