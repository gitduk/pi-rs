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

mod steps;

pub use steps::{FoldedTool, PendingTool, Step, call_frame, out_for, shimmer};
use steps::{
    STEP_INDENT, Unit, body_line, expandable, lone, step_index, step_line, steps_header, unit_at,
};

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
    // A code block's badge, holding the block's text for a click to copy.
    Code {
        badge: Line<'static>,
        code: String,
        hovered: bool,
        copied: Option<String>,
    },
    // A closed mermaid block or a table, drawn for the width it is shown at:
    // a resize draws it again, or shows its source when it no longer fits.
    // One logical line whose `\n`s the wrap breaks; a click copies the source.
    Block {
        form: Form,
        source: String,
        // At which width, and whether a badge heads it.
        painted: RefCell<Option<(usize, Line<'static>, bool)>>,
        hovered: bool,
        copied: Option<String>,
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
    // A turn nobody typed — a `later` come due, a background subagent's
    // answer: one line naming it, its text under the line once opened.
    Relayed {
        label: String,
        body: Vec<Line<'static>>,
        open: bool,
        hovered: bool,
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
            Kind::Said { .. }
                | Kind::Answer(_)
                | Kind::Code { .. }
                | Kind::Block { .. }
                | Kind::Relayed { .. }
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

    /// A turn pi sent on the model's behalf, folded under `label`.
    pub fn relayed(label: &str, text: &str, paint: &Paint) -> Self {
        // A one-line text the label already says opens onto nothing new.
        let said = !text.trim().contains('\n') && label.contains(text.trim());
        Self::new(Kind::Relayed {
            label: label.to_string(),
            body: render::render_coded(text, paint)
                .into_iter()
                .map(|(line, _)| line)
                .filter(|_| !said)
                .collect(),
            open: false,
            hovered: false,
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
            render::render_coded(text, paint)
                .into_iter()
                .map(|(line, code)| match code {
                    Some(code) => Self::new(Kind::Code {
                        badge: line,
                        code,
                        hovered: false,
                        copied: None,
                    }),
                    None => Self::new(Kind::Answer(line)),
                })
        };
        if !paint.color {
            return markdown(text).collect();
        }
        let blank = || Self::new(Kind::Answer(Line::default()));
        let is_blank = |row: &Self| matches!(row, Self(Kind::Answer(l), _) if l.spans.is_empty());
        let mut rows: Vec<Self> = Vec::new();
        for piece in block::pieces(text) {
            match piece {
                block::Piece::Text(text) => rows.extend(markdown(text)),
                block::Piece::Block(form, source) => {
                    if rows.last().is_some_and(|r| !is_blank(r)) {
                        rows.push(blank());
                    }
                    rows.push(Self::new(Kind::Block {
                        form,
                        source: source.trim_end().to_string(),
                        painted: RefCell::new(None),
                        hovered: false,
                        copied: None,
                    }));
                    rows.push(blank());
                }
            }
        }
        if rows.last().is_some_and(is_blank) {
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
            Kind::Relayed { body, .. } => !body.is_empty(),
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
            Kind::Relayed { open, .. } => {
                *open = !*open;
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
            Kind::Result { .. } | Kind::Relayed { .. } => self.is_expandable().then_some(0),
            Kind::Code { .. } | Kind::Block { .. } => Some(0),
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
            // Bold is no wider, so no height moves.
            Kind::Code { hovered: h, .. }
            | Kind::Block { hovered: h, .. }
            | Kind::Relayed { hovered: h, .. } => {
                *h = hovered.is_some();
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

    /// Say `said` beside this row's first line, or stop saying it.
    pub fn say_copied(&mut self, said: Option<&str>) {
        if let Kind::Code { copied, .. } | Kind::Block { copied, .. } = &mut self.0
            && copied.as_deref() != said
        {
            *copied = said.map(str::to_string);
            self.1.clear();
        }
    }

    /// Whether a badge heads this row at `width`, for a note to stand beside.
    pub fn badged(&self, paint: &Paint, width: usize) -> bool {
        match &self.0 {
            Kind::Code { .. } => true,
            Kind::Block { painted, .. } => {
                self.line(0, paint, width);
                painted
                    .borrow()
                    .as_ref()
                    .is_some_and(|(.., badged)| *badged)
            }
            _ => false,
        }
    }

    /// What a click on this row copies: a code block's text, or the source
    /// of a diagram or table.
    pub fn code(&self) -> Option<&str> {
        match &self.0 {
            Kind::Code { code, .. } => Some(code),
            Kind::Block { source, .. } => Some(source),
            _ => None,
        }
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
            Kind::Answer(_)
            | Kind::Code { .. }
            | Kind::Block { .. }
            | Kind::Notice { .. }
            | Kind::Said { .. } => 1,
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
            Kind::Relayed { body, open, .. } => 1 + if *open { body.len() } else { 0 },
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
            Kind::Code {
                badge,
                hovered,
                copied,
                ..
            } => (
                beside(bolded(badge.clone(), *hovered), copied.as_deref(), paint),
                None,
            ),
            Kind::Block {
                form,
                source,
                painted,
                hovered,
                copied,
            } => {
                let mut held = painted.borrow_mut();
                let (line, badged) = match held.as_ref() {
                    Some((at, line, badged)) if *at == width => (line.clone(), *badged),
                    _ => {
                        let (line, badged) = block_at(*form, source, paint, width);
                        *held = Some((width, line.clone(), badged));
                        (line, badged)
                    }
                };
                let line = bolded(line, *hovered && badged);
                // Beside a table's border it could wrap and split the table.
                let copied = copied.as_deref().filter(|_| badged);
                (beside(line, copied, paint), None)
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
            Kind::Relayed {
                label,
                body,
                hovered,
                ..
            } if i == 0 => {
                let tail = match body.len() {
                    0 => String::new(),
                    n => format!("{}{}", icons::PART_SEP, count(n, "line")),
                };
                let room = width.saturating_sub(2 + UnicodeWidthStr::width(tail.as_str()) + 1);
                let text = pi_store::text::clip(label, room.max(8));
                (
                    Line::from(vec![
                        paint.span_hovered(
                            *hovered,
                            &paint.theme.prompt.color,
                            icons::RELAYED_MARK,
                        ),
                        paint.span_hovered(*hovered, &paint.theme.muted, format!(" {text}{tail}")),
                    ]),
                    None,
                )
            }
            Kind::Relayed { body, .. } => (
                body.get(i - 1).cloned().unwrap_or_default(),
                Some(Line::from(STEP_INDENT)),
            ),
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

    /// What the screen opens with: what the prompt stands on, and departures
    /// from the defaults. Built fresh, so a reload theme change repaints it.
    pub fn banner(resolved: &Resolved, paint: &Paint) -> Vec<Self> {
        let mut facts = Vec::new();
        if let Some(system) = &resolved.system {
            facts.push(("system prompt", tilde(system)));
        }
        if !resolved.context.is_empty() {
            facts.push(("content", resolved.context.join(", ")));
        }
        // By topic, a handful at most: the paths are in `/status`.
        const TOPICS: usize = 5;
        let topics: Vec<&str> = resolved
            .memory
            .iter()
            .filter_map(|p| std::path::Path::new(p).file_stem()?.to_str())
            .collect();
        if !topics.is_empty() {
            let mut shown = topics[..topics.len().min(TOPICS)].join(", ");
            if topics.len() > TOPICS {
                shown.push_str(&format!(", +{}", topics.len() - TOPICS));
            }
            facts.push(("memory", shown));
        }
        if !resolved.mcp.is_empty() {
            facts.push(("mcp", resolved.mcp.join(", ")));
        }
        if resolved.ceiling != tool::Tier::Exec {
            facts.push(("tier", format!("{:?}", resolved.ceiling).to_lowercase()));
        }
        // What the project file sets goes under its name, apart from yours.
        let (file, project) = match &resolved.project {
            Some((file, keys)) => (Some(file), keys.as_slice()),
            None => (None, &[][..]),
        };
        let width = facts
            .iter()
            .map(|(label, _)| label.len())
            .chain(project.iter().map(|(key, _)| key.len()))
            .max()
            .unwrap_or(0);
        let muted = |line: &str| Self::notice(Line::from(paint.span(&paint.theme.muted, line)));
        let fact = |label: &str, value: &str| muted(&format!("{label:width$}  {value}"));
        std::iter::once(muted(icons::VERSION_BANNER))
            .chain(facts.iter().map(|(label, value)| fact(label, value)))
            .chain(file.into_iter().flat_map(|file| [muted(""), muted(file)]))
            .chain(project.iter().map(|(key, value)| fact(key, value)))
            .collect()
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

// The home directory as `~`: the banner has no workspace to shorten against.
fn tilde(path: &std::path::Path) -> String {
    match agent::context::home().and_then(|h| Some(path.strip_prefix(h).ok()?.to_owned())) {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
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

// The badge a hovered row leads with, in bold: bold is no wider.
fn bolded(mut line: Line<'static>, hovered: bool) -> Line<'static> {
    if hovered && let Some(badge) = line.spans.first_mut() {
        badge.style = badge.style.add_modifier(Modifier::BOLD);
    }
    line
}

// `said`, muted, closing the first line, as a block's note does.
fn beside(mut line: Line<'static>, said: Option<&str>, paint: &Paint) -> Line<'static> {
    if let Some(said) = said {
        let end = line
            .spans
            .iter()
            .position(|s| s.content.starts_with('\n'))
            .unwrap_or(line.spans.len());
        line.spans
            .insert(end, paint.span(&paint.theme.muted, format!(" {said}")));
    }
    line
}

// A block row at `width`: drawn when it fits, else its source under a badge
// saying what it would need. Whether a badge heads it: a drawn table has none.
fn block_at(form: Form, source: &str, paint: &Paint, width: usize) -> (Line<'static>, bool) {
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
                (Line::from(spans), true)
            }
            Drawn::Wide { needs, has } => (source_under("mermaid", &note(needs, has)), true),
            Drawn::Unread => (source_under("mermaid", ""), true),
        },
        Form::Table => {
            let lines = render::render_markdown(source, paint);
            let needs = lines.iter().map(Line::width).max().unwrap_or(0);
            if needs <= width {
                (joined(lines), false)
            } else {
                (source_under("table", &note(needs, width)), true)
            }
        }
    }
}
