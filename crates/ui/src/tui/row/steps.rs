//! The calls and reasoning of a run, each step opening on its own.

use ratatui::text::{Line, Span};

use super::{Fold, Opened, Opening, page_of};
use crate::render::{self, Outcome, Paint};
use crate::tui::browse::PageLine;
use crate::tui::{THINKING, screen};
use pi_store::icons;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedTool {
    pub name: String,
    pub preview: String,
    // What the step shows opened, under its line: the argument's lines the
    // line doesn't read already (`asked` of them), then what came back.
    body: Vec<String>,
    asked: usize,
    open: bool,
    place: Option<String>,
    // Done, or warned: a failed call is never a step.
    outcome: Outcome,
    took: Option<std::time::Duration>,
}

impl FoldedTool {
    /// `asked` is the call's whole leading argument, `output` what it
    /// printed; both are what opening the step shows.
    pub fn new(name: &str, preview: &str, asked: &str, output: &str) -> Self {
        let recovered;
        let preview = if preview.trim() == name {
            recovered = serde_json::from_str(asked)
                .map(|args| tool::summary(&args).into_owned())
                .unwrap_or_else(|_| asked.to_string());
            &recovered
        } else {
            preview
        };
        // Keep an argument line unless the head already shows it.
        let first = asked.lines().next().unwrap_or_default().trim();
        let read = preview.lines().next().is_some_and(|h| h.contains(first));
        let skip = usize::from(read);
        let mut body: Vec<String> = asked.lines().skip(skip).map(str::to_string).collect();
        let asked = body.len();
        body.extend(pi_core::core::tools::shown(name, output).map(str::to_string));
        Self {
            name: name.to_string(),
            preview: preview.to_string(),
            body,
            asked,
            open: false,
            place: None,
            outcome: Outcome::Done,
            took: None,
        }
    }

    /// Where the call ran, as `render::place` reads it off its arguments.
    pub fn at(mut self, place: Option<String>) -> Self {
        self.place = place;
        self
    }

    /// How the call came out: done, or ran to the end but asks for a look.
    pub fn ended(mut self, outcome: Outcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// How long the call ran, if known.
    pub fn timed(mut self, took: Option<std::time::Duration>) -> Self {
        self.took = took;
        self
    }

    fn desc(&self) -> String {
        desc_of(&self.name, &self.preview)
    }

    fn placed(&self) -> String {
        render::placed(self.place.as_deref())
    }
}

// A call still in flight, as the group that will fold it draws it: the
// row draws it from the moment it is queued, so nothing jumps when it lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTool {
    /// The call's name and leading argument, read the way the live line read
    /// them — they already agree with the row the call will land as.
    pub name: String,
    pub preview: String,
    /// How far the call has got. A call that lands badly is never the
    /// row's — it's on its way to its own line.
    pub stage: Stage,
    /// Whole seconds it has been out, or ran once landed; `None` while
    /// queued. Whole, so a frame within the same second compares equal.
    pub secs: Option<u64>,
    /// Where it runs, as `render::place` reads it off its arguments.
    pub place: Option<String>,
}

/// Where a call in flight stands, each stage holding what only it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Taken into its batch, waiting for its turn to run.
    Queued,
    /// With what it last said of how far it has got.
    Running {
        progress: Option<String>,
    },
    Landed(Outcome),
}

impl Stage {
    /// The mark its line leads with at spinner tick `tick`.
    pub fn mark(&self, tick: usize) -> StepMark {
        match self {
            Stage::Queued => StepMark::Queued,
            Stage::Running { .. } => StepMark::Out(tick),
            Stage::Landed(outcome) => StepMark::Ended(*outcome),
        }
    }

    /// What it last said of how far it has got, while it runs.
    pub fn progress(&self) -> Option<&str> {
        match self {
            Stage::Running { progress } => progress.as_deref(),
            Stage::Queued | Stage::Landed(_) => None,
        }
    }
}

impl PendingTool {
    fn desc(&self) -> String {
        desc_of(&self.name, &self.preview)
    }

    fn landed(&self) -> bool {
        matches!(self.stage, Stage::Landed(_))
    }
}

// The frame a call in flight shows at spinner tick `tick`.
fn call_frame(tick: usize) -> &'static str {
    const TICKS_PER_FRAME: usize = 3;
    icons::CALL_FRAMES[tick / TICKS_PER_FRAME % icons::CALL_FRAMES.len()]
}

// The frame reasoning still streaming shows at spinner tick `tick`.
fn think_frame(tick: usize) -> &'static str {
    const TICKS_PER_FRAME: usize = 2;
    icons::THINK_FRAMES[tick / TICKS_PER_FRAME % icons::THINK_FRAMES.len()]
}

/// What a call's line ends with: its progress, if it reports any, then how
/// long it has been out or ran, as `render::ran_for` says it.
pub fn out_for(progress: Option<&str>, secs: Option<u64>) -> String {
    let progress = progress.map_or_else(String::new, |p| format!("{}{p}", icons::PART_SEP));
    progress + &render::ran_for(secs.map(std::time::Duration::from_secs))
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

/// The indent aligning detail lines and list controls with step text.
pub const STEP_INDENT: &str = "  ";

/// One step of a group, in the order the run took it: a read-only call that
/// landed well, or a block of reasoning; each opens on its own.
pub enum Step {
    Tool(FoldedTool),
    Thinking {
        // The stream appends a block's lines to the row holding its id.
        block: u64,
        lines: Vec<Line<'static>>,
        // Those lines as they read opened, rendered once the block ends;
        // till then the lines themselves.
        shown: Vec<render::Rendered>,
        open: bool,
        // Still streaming: the group is not done while it is.
        live: bool,
    },
}

/// The mark a line naming a step or a call leads with.
#[derive(Clone, Copy)]
pub enum StepMark {
    /// Landed, as it came out.
    Ended(Outcome),
    /// Out, turning at this spinner tick.
    Out(usize),
    /// Reasoning still streaming, turning at this spinner tick.
    Thinking(usize),
    /// Waiting its turn: still, since nothing runs yet.
    Queued,
    /// Reasoning that ended.
    Thought,
}

impl StepMark {
    /// The glyph alone, for a line drawn in one style.
    pub fn glyph(self) -> &'static str {
        match self {
            StepMark::Ended(outcome) => render::outcome_glyph(outcome),
            StepMark::Out(tick) => call_frame(tick),
            StepMark::Thinking(tick) => think_frame(tick),
            StepMark::Queued => icons::PENDING_MARK,
            StepMark::Thought => icons::THINK_MARK,
        }
    }

    // The mark and the space after it.
    fn spans(self, hovered: bool, paint: &Paint) -> Vec<Span<'static>> {
        let reasoning = paint.reasoning_style();
        let style = match self {
            StepMark::Ended(outcome) => render::outcome_style(outcome, &paint.theme),
            StepMark::Thinking(_) | StepMark::Thought => &reasoning,
            StepMark::Out(_) | StepMark::Queued => &paint.theme.muted,
        };
        vec![
            paint.span_hovered(hovered, style, self.glyph()),
            paint.span_hovered(hovered, &paint.theme.muted, " "),
        ]
    }
}

// The columns a mark and the space after it take.
const MARK_WIDTH: usize = 2;

// What one line naming a step or a call says, before it is fitted to a width.
struct Head {
    mark: StepMark,
    text: String,
    // Present only when this text names a tool, for its colours.
    tool: Option<String>,
    // What follows the text, a count or a time, kept whole when the text is cut.
    tail: String,
    // Whether more of the text is there than this line, whatever the width.
    more: bool,
}

impl Head {
    // The text's columns: the mark and its space, the tail, and a column of
    // air before the terminal edge.
    fn room(&self, width: usize) -> usize {
        render::title_room(width, MARK_WIDTH, &format!("{} ", self.tail)).max(8)
    }

    fn cut(&self, width: usize) -> bool {
        pi_store::text::width(&self.text) > self.room(width)
    }

    // Opened, the text is whole and wraps, and what the tail said is below it.
    fn content(&self, open: bool, hovered: bool, paint: &Paint, width: usize) -> Line<'static> {
        let text = if open {
            self.text.clone()
        } else {
            pi_store::text::clip_more(&self.text, self.room(width), self.more)
        };
        let mut spans = Vec::new();
        let reasoning = paint.reasoning_style();
        let style = match self.mark {
            StepMark::Thinking(_) | StepMark::Thought => &reasoning,
            StepMark::Ended(_) | StepMark::Out(_) | StepMark::Queued => &paint.theme.muted,
        };
        if let Some(name) = &self.tool {
            spans.extend(paint.tool_call(name, &text, hovered));
        } else {
            spans.push(paint.span_hovered(hovered, style, text));
        }
        if !open {
            spans.push(paint.span_hovered(hovered, style, &self.tail));
        }
        Line::from(spans)
    }
    fn drawn(
        &self,
        open: bool,
        hovered: bool,
        paint: &Paint,
        width: usize,
        lead: Option<Line<'static>>,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let mut border = lead.unwrap_or_default();
        let room = width.saturating_sub(border.width());
        border.spans.extend(self.mark.spans(hovered, paint));
        (self.content(open, hovered, paint, room), Some(border))
    }
}

// Progress and elapsed time stay on the call's own line.
fn pending_head(p: &PendingTool, tick: usize) -> Head {
    let place = render::placed(p.place.as_deref());
    Head {
        mark: p.stage.mark(tick),
        text: p.desc(),
        tool: Some(p.name.clone()),
        tail: place + &out_for(p.stage.progress(), p.secs),
        more: false,
    }
}

// A block's lines as they read opened: markdown, muted as its lines are, so
// it still steps back from what was said.
fn shown_of(lines: &[Line<'static>], paint: &Paint) -> Vec<render::Rendered> {
    let text: Vec<String> = lines.iter().map(screen::plain).collect();
    let mut shown = render::render_coded(&text.join("\n"), paint);
    if paint.color {
        let reasoning = crate::sgr::style_to_ratatui(&paint.reasoning_style());
        for span in shown.iter_mut().flat_map(|r| r.line.spans.iter_mut()) {
            span.style = span.style.patch(reasoning);
        }
    }
    shown
}

impl Step {
    /// A block of reasoning, `live` while it still streams.
    pub fn thinking(block: u64, lines: Vec<Line<'static>>, live: bool, paint: &Paint) -> Self {
        Step::Thinking {
            block,
            shown: shown_of(&lines, paint),
            lines,
            open: false,
            live,
        }
    }

    fn is_open(&self) -> bool {
        match self {
            Step::Tool(t) => t.open,
            Step::Thinking { open, .. } => *open,
        }
    }

    fn set_open(&mut self, to: bool) {
        match self {
            Step::Tool(t) => t.open = to && !t.body.is_empty(),
            Step::Thinking { open, .. } => *open = to,
        }
    }

    fn flip(&mut self) {
        match self {
            Step::Tool(t) => t.open = !t.open,
            Step::Thinking { open, .. } => *open = !*open,
        }
    }

    // The lines it shows opened: its own, then what it holds.
    fn opened_len(&self) -> usize {
        match self {
            Step::Tool(t) => 1 + t.body.len(),
            Step::Thinking {
                lines, live: true, ..
            } => lines.len().max(1),
            Step::Thinking { shown, .. } => shown.len().max(1),
        }
    }

    // Line `j` opened, whole, less the border its place gives: its own line,
    // then what it holds; a finished block's lines as markdown.
    fn opened_content(&self, j: usize, hovered: bool, tick: usize, paint: &Paint) -> PageLine {
        let mut page: PageLine = match (self, j) {
            (Step::Tool(t), 1..) => body_line(t, j - 1, paint).into(),
            (
                Step::Thinking {
                    lines, live: true, ..
                },
                _,
            ) if j < lines.len() => lines[j].clone().into(),
            (
                Step::Thinking {
                    shown, live: false, ..
                },
                _,
            ) if j < shown.len() => shown[j].clone().into(),
            _ => self
                .head(tick)
                .content(true, false, paint, usize::MAX)
                .into(),
        };
        // Bold as `span_hovered` bolds: only where there is colour.
        if hovered && paint.color {
            for span in &mut page.line.spans {
                span.style = Paint::lit(span.style);
            }
        }
        page
    }

    fn drawn(
        &self,
        j: usize,
        hovered: bool,
        tick: usize,
        paint: &Paint,
    ) -> (PageLine, Option<Line<'static>>) {
        let mut page = self.opened_content(j, hovered, tick, paint);
        let mut border = opened_border(j).unwrap_or_default();
        if j == 0 {
            border
                .spans
                .extend(self.head(tick).mark.spans(hovered, paint));
        }
        page.prepend(render::Prefix::hanging(border));
        let first = page.prefix.as_ref().map(|p| p.first.clone());
        (page, first)
    }

    // Line `j` of a block, its first wearing the mark as a call's own line does.
    fn marked(&self, j: usize, mut page: PageLine, tick: usize, paint: &Paint) -> PageLine {
        if j > 0 {
            return page;
        }
        page.prepend(render::Prefix::hanging(Line::from(
            self.head(tick).mark.spans(false, paint),
        )));
        page
    }

    // The lines it currently takes in the list.
    fn len(&self) -> usize {
        if self.is_open() { self.opened_len() } else { 1 }
    }

    // `tick` turns the mark of reasoning still streaming.
    fn head(&self, tick: usize) -> Head {
        match self {
            Step::Tool(t) => Head {
                mark: StepMark::Ended(t.outcome),
                text: t.desc(),
                tool: Some(t.name.clone()),
                tail: t.placed() + &render::ran_for(t.took),
                more: t.asked > 0,
            },
            Step::Thinking { lines, live, .. } => Head {
                tool: None,
                mark: match live {
                    true => StepMark::Thinking(tick),
                    false => StepMark::Thought,
                },
                text: lines
                    .first()
                    .map_or_else(|| THINKING.to_string(), screen::plain),
                tail: String::new(),
                more: lines.len() > 1,
            },
        }
    }

    // Whether its line opens: there is more under it, or more of it than
    // the width shows.
    fn opens(&self, width: usize) -> bool {
        if matches!(self, Step::Thinking { lines, .. } if lines.is_empty()) {
            return false;
        }
        self.is_open()
            || matches!(self, Step::Thinking { lines, .. } if !lines.is_empty())
            || self.opened_len() > 1
            || self.head(0).cut(width)
    }
}

// What line `i` of the list sits in, and how far into it.
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

// Detail lines align with the text after the step's mark.
fn opened_border(j: usize) -> Option<Line<'static>> {
    (j > 0).then(|| Line::from(STEP_INDENT))
}

// Line `i` of the list, the calls in flight last, with the
// indent it sits under as the border a wrap repeats.
fn step_line(
    steps: &[Step],
    pending: &[PendingTool],
    i: usize,
    hovered: bool,
    tick: usize,
    paint: &Paint,
    width: usize,
) -> (PageLine, Option<Line<'static>>) {
    let lead = None;
    let (line, border) = match unit_at(steps, pending, i) {
        Some((Unit::Step(s), j)) if s.is_open() => return s.drawn(j, hovered, tick, paint),
        Some((Unit::Step(s), _)) => s.head(tick).drawn(false, hovered, paint, width, lead),
        Some((Unit::Pending(p), _)) => {
            pending_head(p, tick).drawn(false, false, paint, width, lead)
        }
        None => (Line::default(), lead),
    };
    (line.into(), border)
}

/// The run's working: read-only calls that landed well and
/// the reasoning around them, in order.
pub(super) struct Group {
    steps: Vec<Step>,
    // Calls in flight this row draws: not in the scrollback yet, left
    // to the row by the live block, since this is where they'll land.
    pending: Vec<PendingTool>,
    pub(super) folded: bool,
    pub(super) all: bool,
    // The line under the mouse, when it is one a click opens or closes.
    hovered: Option<usize>,
    // The tick the marks of calls still out turn at.
    spinner: usize,
}

impl Group {
    pub(super) fn new(mut first: Step, folded: bool) -> Self {
        first.set_open(!folded);
        Self {
            steps: vec![first],
            pending: Vec::new(),
            folded,
            all: !folded,
            hovered: None,
            spinner: 0,
        }
    }

    pub(super) fn join(&mut self, step: Step) {
        self.steps.push(step);
    }

    pub(super) fn set_folded(&mut self, to: bool) {
        self.folded = to;
        self.all = !to;
        for step in &mut self.steps {
            step.set_open(!to);
        }
    }

    pub(super) fn has_tools(&self) -> bool {
        !self.pending.is_empty() || self.steps.iter().any(|step| matches!(step, Step::Tool(_)))
    }

    // The step line `i` falls in, head or not: a footer cut inside a step
    // opens that step from wherever the cut left off.
    fn page_step(&self, i: usize) -> Option<&Step> {
        walk(&self.steps, i)
            .ok()
            .and_then(|(k, _)| self.steps.get(k))
    }

    pub(super) fn len(&self) -> usize {
        self.steps.iter().map(Step::len).sum::<usize>() + self.pending.len()
    }

    pub(super) fn sections(&self) -> Vec<std::ops::Range<usize>> {
        let mut sections = Vec::with_capacity(self.steps.len() + self.pending.len());
        let mut at = 0;
        for step in &self.steps {
            sections.push(at..at + step.len());
            at += step.len();
        }
        sections.extend((at..at + self.pending.len()).map(|i| i..i + 1));
        sections
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let (mut page, border) = self.drawn(i, paint, width);
        if let Some(prefix) = &page.prefix {
            page.line = prefix.fit_body(page.line, width);
        }
        (page.line, border)
    }

    // What in line `i` reads as one, as the line `line` gives holds it.
    pub(super) fn units(&self, i: usize, paint: &Paint) -> crate::unit::Units {
        self.drawn(i, paint, usize::MAX).0.units
    }

    pub(super) fn continuation(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> Option<Line<'static>> {
        self.drawn(i, paint, width).0.prefix.map(|p| p.rest)
    }

    // Line `i` as drawn, with what in it reads as one, and its border.
    fn drawn(&self, i: usize, paint: &Paint, width: usize) -> (PageLine, Option<Line<'static>>) {
        let hovered = self.hovered == Some(i);
        step_line(
            &self.steps,
            &self.pending,
            i,
            hovered,
            self.spinner,
            paint,
            width,
        )
    }

    // Hover line, spinner tick and the calls in flight it draws; whether
    // its count may have moved — a tick alone can't, lines are clipped.
    pub(super) fn update(
        &mut self,
        hovered: Option<usize>,
        spin: usize,
        held: &[PendingTool],
    ) -> bool {
        self.hovered = hovered;
        self.spinner = spin;
        if self.pending.as_slice() == held {
            return false;
        }
        let reshaped = self.pending.len() != held.len()
            || self.pending.iter().zip(held).any(|(a, b)| {
                (&a.name, &a.preview, a.landed()) != (&b.name, &b.preview, b.landed())
            });
        self.pending = held.to_vec();
        reshaped
    }

    pub(super) fn holds_block(&self, id: u64) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Thinking { block, .. } if *block == id))
    }

    pub(super) fn shows_block(&self, id: u64) -> Option<&'static str> {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Thinking { block, open: true, .. } if *block == id))
            .then_some(STEP_INDENT)
    }

    pub(super) fn push_line(&mut self, id: u64, line: Line<'static>) {
        if let Some(Step::Thinking { lines, .. }) = self
            .steps
            .iter_mut()
            .rev()
            .find(|s| matches!(s, Step::Thinking { block, .. } if *block == id))
        {
            lines.push(line);
        }
    }

    // Block `id` ended: whether the group is left with no step.
    pub(super) fn end_block(&mut self, id: u64, paint: &Paint) -> bool {
        self.steps.retain_mut(|s| match s {
            Step::Thinking {
                block,
                lines,
                shown,
                live,
                ..
            } if *block == id => {
                *live = false;
                *shown = shown_of(lines, paint);
                !lines.is_empty()
            }
            _ => true,
        });
        self.steps.is_empty()
    }

    // The step whose own line is line `i` of the group, while it lists them.
    fn listed_step(&self, i: usize) -> Option<usize> {
        step_index(&self.steps, i)
    }
}

impl Fold for Group {
    fn continuation(&self, i: usize, paint: &Paint, width: usize) -> Option<Line<'static>> {
        Group::continuation(self, i, paint, width)
    }

    fn more_border(&self, _heading: usize, _paint: &Paint, _width: usize) -> Option<Line<'static>> {
        Some(Line::from(STEP_INDENT))
    }

    fn is_open(&self) -> bool {
        true
    }

    fn expandable(&self) -> bool {
        self.steps.len() + self.pending.len() > 5 || self.steps.iter().any(|s| s.opens(usize::MAX))
    }

    fn flip(&mut self) {
        self.all = !self.all;
    }

    fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        self.listed_step(i)
            .filter(|&k| self.steps[k].opens(width))
            .map(|_| i)
    }

    fn toggle_at(&mut self, i: usize, width: usize) -> bool {
        if self.click_line(i, width) != Some(i) {
            return false;
        }
        let Some(k) = self.listed_step(i) else {
            return false;
        };
        self.steps[k].flip();
        self.folded = !self.steps.iter().any(Step::is_open);
        true
    }

    // The step headed by line `i`.
    fn opened(&self, i: usize, paint: &Paint) -> Option<Opened> {
        let step = self.page_step(i)?;
        let lines = (0..step.opened_len()).map(|j| {
            (
                step.marked(j, step.opened_content(j, false, 0, paint), 0, paint),
                opened_border(j),
            )
        });
        Some(lines.collect())
    }

    fn opening(&self, i: usize, paint: &Paint) -> Option<Opening> {
        let step = self.page_step(i)?;
        Some(step_page(step, paint))
    }
}

fn step_page(step: &Step, paint: &Paint) -> Opening {
    let Step::Thinking { lines, .. } = step else {
        return page_of(
            (0..step.opened_len())
                .map(|j| {
                    (
                        step.marked(j, step.opened_content(j, false, 0, paint), 0, paint),
                        opened_border(j),
                    )
                })
                .collect(),
        );
    };
    // Pages use normal Markdown contrast instead of muted reasoning colours.
    let text = lines
        .iter()
        .map(screen::plain)
        .collect::<Vec<_>>()
        .join("\n");
    let opened = render::render_coded(&text, paint)
        .into_iter()
        .enumerate()
        .map(|(j, row)| (step.marked(j, row.into(), 0, paint), opened_border(j)))
        .collect();
    page_of(opened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::row::Row;

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
            stage: if landed {
                Stage::Landed(Outcome::Done)
            } else {
                Stage::Running { progress: None }
            },
            secs: None,
            place: None,
        }
    }

    // Calls end in whatever order; the row is the only place one in
    // flight shows, so it stays pending however the named call ended.
    #[test]
    fn a_call_still_out_keeps_the_row_pending() {
        let mut row = bundle(vec![tool("read", "a.rs")]);

        row.update_live(
            None,
            0,
            &[
                pending("bash", "cargo test", false),
                pending("read", "b.rs", true),
            ],
        );
        let super::super::Kind::Steps(group) = &row.0 else {
            panic!("expected a group");
        };
        assert!(
            group.pending.iter().any(|p| !p.landed()),
            "an unfinished call keeps the group pending"
        );
    }

    // Calls in flight occupy their own lines and can leave independently.
    #[test]
    fn a_pending_call_has_its_own_line_until_it_leaves() {
        let mut row = bundle(vec![tool("read", "a.rs")]);
        row.update_live(None, 0, &[pending("grep", "match 1", false)]);
        assert_eq!(row.len(), 2, "one line per call, without a group header");
        assert!(shown(&row, 80)[1].contains("grep match 1"));

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
        let lines = (1..=n).map(|i| Line::from(format!("line {i}"))).collect();
        Step::thinking(block, lines, false, &Paint::new(false))
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

    // What line `i` opens onto on a page is what opening it there shows, the
    // indent it sat under aside; a long command included, whole.
    #[test]
    fn a_page_reads_what_opening_in_place_shows() {
        use crate::tui::screen::plain;
        let paint = Paint::new(false);
        let opens_as_in_place = |mut row: Row, i: usize| {
            let page: Vec<String> = (row.opening(i, &paint).expect("it opens").iter())
                .map(|l| plain(&l.line))
                .collect();
            assert!(row.toggle_at(i, 80));
            let drawn: Vec<String> = (i..i + page.len())
                .map(|at| plain(&row.line(at, &paint, 80).0))
                .collect();
            assert_eq!(page, drawn);
        };
        let long = format!(
            "grep -rn banner {}",
            "crates/ui/src/tui/tests/*.rs ".repeat(4)
        );
        let ran = FoldedTool::new("bash", &long, &long, "one\ntwo\n");
        let mut listed = bundle(vec![tool("read", "a.rs")]);
        assert!(listed.join(think(1, 3)).is_ok());
        assert!(listed.join(Step::Tool(ran)).is_ok());
        opens_as_in_place(listed, 2);
        opens_as_in_place(
            Row::thinking(
                2,
                (1..=4).map(|i| Line::from(format!("l{i}"))).collect(),
                true,
            ),
            0,
        );
        opens_as_in_place(Row::note_lines("first\nsecond\nthird").remove(0), 0);
    }

    // Every step is visible and answers only to its own heading.
    #[test]
    fn steps_are_visible_and_each_opens_alone() {
        let ran = FoldedTool::new("bash", "cat <<EOF", "cat <<EOF\nhi\nEOF", "hi\n")
            .timed(Some(std::time::Duration::from_secs(5)));
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(row.join(think(1, 3)).is_ok());
        assert!(row.join(Step::Tool(ran)).is_ok());
        assert_eq!(row.len(), 3);
        let unfolded = shown(&row, 80);
        assert_eq!(unfolded.len(), 3, "one line per step");

        assert_eq!(row.click_line(0, 80), None, "nothing under the read");
        assert!(row.toggle_at(1, 80), "the reasoning opens");
        assert_eq!(row.len(), 5);
        assert!(row.toggle_at(4, 80), "the call past it still answers");
        assert_eq!(
            row.len(),
            8,
            "its own line, then its two asked lines and one back"
        );
        assert_eq!(row.click_line(5, 80), None, "a body line opens nothing");
        assert!(row.toggle_at(1, 80) && row.toggle_at(2, 80));
        assert_eq!(shown(&row, 80), unfolded);
    }

    // A group of one step has no list to open: opening it opens the step,
    // its lines and no header above them.
    #[test]
    fn a_group_of_one_opens_its_step() {
        let mut row = Row::steps(think(1, 2), true);
        assert!(row.toggle_expand());
        assert_eq!(row.len(), 2);

        let mut one = bundle(vec![FoldedTool::new(
            "read",
            "a.rs",
            "a.rs",
            "fn main() {}",
        )]);
        assert!(one.toggle_at(0, 80));
        assert_eq!(one.len(), 2);

        // Before its first line lands there is nothing to unfold.
        let empty = Row::steps(think(2, 0), false);
        assert!(!empty.is_expandable());
    }

    // One call or many, the text runs up to what follows it, `…` on its end.
    #[test]
    fn a_long_call_fills_the_row_before_what_follows_it() {
        let long = "x".repeat(200);
        let mut row = Row::steps(think(1, 1), true);
        assert!(row.join(Step::Tool(tool("bash", &long))).is_ok());
        assert!(row.join(Step::Tool(tool("bash", &long))).is_ok());
        let line = &shown(&row, 80)[1];
        assert!(line.ends_with(icons::ELLIPSIS), "{line}");
        assert_eq!(pi_store::text::width(line), 79, "{line}");

        let one = bundle(vec![tool("bash", &long)]);
        assert_eq!(pi_store::text::width(&shown(&one, 80)[0]), 79);
    }
}
