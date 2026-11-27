//! The calls and reasoning of a run, each step opening on its own.

use std::collections::BTreeMap;

use ratatui::text::{Line, Span};

use super::call::Call;
use super::{Fold, Opening, page_of};
use crate::render::{self, Outcome, Paint};
use crate::tui::browse::PageLine;
use crate::tui::{THINKING, screen};
use pi_store::icons;

// A call still in flight, as the group that will fold it draws it: the
// row draws it from the moment it is queued, so nothing jumps when it lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTool {
    /// The call's name and leading argument, read the way the live line read
    /// them — they already agree with the row the call will land as.
    pub name: String,
    /// The one-line summary of its arguments the live line reads.
    pub summary: String,
    /// How far the call has got. A call that lands badly is never the
    /// row's — it's on its way to its own line.
    pub stage: Stage,
    /// Whole seconds it has been out, or ran once landed; `None` while
    /// queued. Whole, so a frame within the same second compares equal.
    pub secs: Option<u64>,
    /// Where it runs, as `render::cwd` reads it off its arguments.
    pub cwd: Option<String>,
    /// The file it works on, when it names one: what a right click opens.
    pub file: Option<std::path::PathBuf>,
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
    const TICKS_PER_FRAME: usize = 4;
    icons::THINK_FRAMES[tick / TICKS_PER_FRAME % icons::THINK_FRAMES.len()]
}

/// What a call's line ends with: its progress, if it reports any, then how
/// long it has been out or ran, as `render::ran_for` says it.
pub fn out_for(progress: Option<&str>, secs: Option<u64>) -> String {
    let progress = progress.map_or_else(String::new, |p| format!("{}{p}", icons::PART_SEP));
    progress + &render::ran_for(secs.map(std::time::Duration::from_secs))
}

/// The indent aligning detail lines and list controls with step text.
pub const STEP_INDENT: &str = "  ";

// `line` cut to `room` columns with a mark on its end, the window's rows
// being one screen row each; the page reads what is cut whole.
fn cut_line(line: Line<'static>, room: usize) -> Line<'static> {
    if pi_store::text::width(&screen::plain(&line)) <= room {
        return line;
    }
    screen::cut(&line, room, Some(Span::raw(icons::ELLIPSIS)))
}

// An opened block of reasoning still streaming shows this many of its first
// and newest lines, with a count of the rest between.
const FOLD_HEAD: usize = 2;
const FOLD_TAIL: usize = 3;
// The rows that takes: the head, the count, the tail.
const FOLD_ROWS: usize = FOLD_HEAD + 1 + FOLD_TAIL;

// Whether a block of `n` lines has a middle to fold, the count's row standing
// in for it.
fn folds_middle(n: usize) -> bool {
    n > FOLD_ROWS
}

/// One step of a group, in the order the run took it: a read-only call that
/// landed well, or a block of reasoning; each opens on its own.
pub enum Step {
    Tool(Call),
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
    /// A model still thinking — reasoning streaming, or yet to answer —
    /// turning at this spinner tick.
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
    pub(super) fn spans(self, hovered: bool, paint: &Paint) -> Vec<Span<'static>> {
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

// The columns a line's text has: the mark and its space, the tail, and a
// column of air before the terminal edge.
pub(super) fn text_room(width: usize, tail: &str) -> usize {
    render::title_room(width, MARK_WIDTH, &format!("{tail} ")).max(8)
}

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
    fn cut(&self, width: usize) -> bool {
        pi_store::text::width(&self.text) > text_room(width, &self.tail)
    }

    // Opened, the text is whole and wraps, and what the tail said is below it.
    fn content(&self, open: bool, hovered: bool, paint: &Paint, width: usize) -> Line<'static> {
        let text = if open {
            self.text.clone()
        } else {
            pi_store::text::clip_more(&self.text, text_room(width, &self.tail), self.more)
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
    ) -> (Line<'static>, Option<Line<'static>>) {
        let border = Line::from(self.mark.spans(hovered, paint));
        (self.content(open, hovered, paint, width), Some(border))
    }
}

// Progress and elapsed time stay on the call's own line.
fn pending_head(p: &PendingTool, tick: usize) -> Head {
    Head {
        mark: p.stage.mark(tick),
        text: tool::label(&p.name, &p.summary),
        tool: Some(p.name.clone()),
        tail: render::tail(p.cwd.as_deref(), &out_for(p.stage.progress(), p.secs)),
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
            Step::Tool(call) => call.open(),
            Step::Thinking { open, .. } => *open,
        }
    }

    fn set_open(&mut self, to: bool) {
        match self {
            Step::Tool(call) => call.open_to(to),
            Step::Thinking { open, .. } => *open = to,
        }
    }

    fn flip(&mut self) {
        match self {
            Step::Tool(call) => call.open_to(!call.open()),
            Step::Thinking { open, .. } => *open = !*open,
        }
    }

    // The lines it shows opened: its own, then what it holds. A live block
    // keeps its head and newest lines around a count, not every line.
    fn opened_len(&self) -> usize {
        match self {
            Step::Tool(call) => call.height(),
            Step::Thinking {
                lines, live: true, ..
            } if folds_middle(lines.len()) => FOLD_ROWS,
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
            (
                Step::Thinking {
                    lines, live: true, ..
                },
                _,
            ) if folds_middle(lines.len()) => {
                debug_assert!(j < FOLD_ROWS, "a folded row asks for its own line");
                let n = lines.len();
                match j {
                    j if j < FOLD_HEAD => lines[j].clone().into(),
                    j if j == FOLD_HEAD => Line::from(paint.span(
                        &paint.theme.muted,
                        format!("{} {} more", icons::ELLIPSIS, n - FOLD_HEAD - FOLD_TAIL),
                    ))
                    .into(),
                    j => lines[n - (FOLD_ROWS - j)].clone().into(),
                }
            }
            (
                Step::Thinking {
                    lines, live: true, ..
                },
                _,
            ) => lines
                .get(j)
                .cloned()
                .map_or_else(|| self.head_line(tick, paint), PageLine::from),
            (
                Step::Thinking {
                    shown, live: false, ..
                },
                _,
            ) if j < shown.len() => shown[j].clone().into(),
            _ => self.head_line(tick, paint),
        };
        if hovered {
            paint.lit_spans(&mut page.line.spans);
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

    // Whether opening it fits a window `rows` high, less the row its bar
    // takes. A call lays itself out inside the rows it is given, so it
    // always fits; a block does not.
    fn fits(&self, rows: usize) -> bool {
        self.opened_len() < rows
    }

    // Whether its lines show inside the window: open, and no taller than
    // the window holds.
    fn shows_open(&self, rows: usize) -> bool {
        self.is_open() && self.fits(rows)
    }

    // Whether a click on it reads it on a page: read in place it would be
    // cut, being more lines than the rows it may take, or a line wider than
    // the row. One window row is one screen row, so nothing wraps there.
    fn opens_on_a_page(&self, rows: usize, width: usize) -> bool {
        (self.opened_len() > 1 && !self.fits(rows)) || self.cut(width)
    }

    // Whether its line says less than it holds: more of it than the width
    // shows.
    fn cut(&self, width: usize) -> bool {
        match self {
            Step::Tool(call) => call.cut(width),
            Step::Thinking { .. } => self.head(0).cut(width),
        }
    }

    // The lines it takes in the list, `rows` being what the window may show:
    // one when it is closed or taller than the window.
    fn len_in(&self, rows: usize) -> usize {
        match self.shows_open(rows) {
            true => self.opened_len(),
            false => 1,
        }
    }

    // Its own line, opened and whole, as the head of what it shows.
    fn head_line(&self, tick: usize, paint: &Paint) -> PageLine {
        self.head(tick)
            .content(true, false, paint, usize::MAX)
            .into()
    }

    // The line of a block, its first; a call draws its own line.
    fn head(&self, tick: usize) -> Head {
        match self {
            Step::Tool(_) => unreachable!("a call draws its own line"),
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

    // Whether line `j` of it reads the rest on a page: the count under what
    // it holds.
    fn pages_row(&self, j: usize) -> bool {
        match self {
            Step::Tool(call) => call.page_row(j, usize::MAX),
            Step::Thinking { .. } => false,
        }
    }

    // Whether line `j` of it answers a click: its own line opens or closes
    // it, the count under it reads the rest.
    fn opens_at(&self, j: usize, width: usize) -> bool {
        match self {
            Step::Tool(call) => call.opens_at(j, usize::MAX, width),
            Step::Thinking { .. } => j == 0 && self.opens(width),
        }
    }

    // Whether its line opens: there is more under it, or more of it than
    // the width shows.
    fn opens(&self, width: usize) -> bool {
        // A call says itself whether a click on its line opens it.
        if let Step::Tool(call) = self {
            return call.click_line(0, width).is_some();
        }
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
    rows: usize,
    i: usize,
) -> Option<(Unit<'a>, usize)> {
    match walk(steps, rows, i) {
        Ok((k, at)) => Some((Unit::Step(&steps[k]), at)),
        Err(rest) => pending.get(rest).map(|p| (Unit::Pending(p), 0)),
    }
}

// Which step line `i` of the list falls in and how far into it, or how far
// past the last step it lies.
fn walk(steps: &[Step], rows: usize, mut i: usize) -> Result<(usize, usize), usize> {
    for (k, step) in steps.iter().enumerate() {
        match i.checked_sub(step.len_in(rows)) {
            Some(rest) => i = rest,
            None => return Ok((k, i)),
        }
    }
    Err(i)
}

// The step whose own line is line `i` of the list.
fn step_index(steps: &[Step], rows: usize, i: usize) -> Option<usize> {
    match walk(steps, rows, i) {
        Ok((k, 0)) => Some(k),
        _ => None,
    }
}

// Detail lines align with the text after the step's mark.
fn opened_border(j: usize) -> Option<Line<'static>> {
    (j > 0).then(|| Line::from(STEP_INDENT))
}

// A group's viewport, counted in content lines: `rows` is what the window
// may take, the bar's row off; `top` the first shown; `follow` the newest.
struct Window {
    top: usize,
    follow: bool,
    rows: usize,
}

// The rows a group's window takes: fixed, so a run's working reads the same
// whatever the terminal's height. The bar, when one is drawn, sits under it.
const WINDOW_ROWS: usize = 7;

impl Default for Window {
    // Before a frame has said: no ceiling, so a row asked to draw itself
    // whole shows all of it.
    fn default() -> Self {
        Self {
            top: 0,
            follow: true,
            rows: usize::MAX,
        }
    }
}

// What the window shows this frame: `body` content rows from `top` on,
// `past` lines kept for the wheel, and whether the bar is drawn.
struct Framed {
    body: usize,
    top: usize,
    past: usize,
    bar: bool,
}

impl Framed {
    // The rows drawn: the content, and the bar under it.
    fn len(&self) -> usize {
        self.body + usize::from(self.bar)
    }

    // Where in the content window row `i` falls; the bar row holds none.
    fn at(&self, i: usize) -> Option<usize> {
        (i < self.body).then_some(self.top + i)
    }
}

fn bar_row(framed: &Framed, i: usize) -> bool {
    framed.bar && i == framed.body
}

/// The run's working: read-only calls that landed well and
/// the reasoning around them, in order.
pub(super) struct Group {
    steps: Vec<Step>,
    // Calls in flight this row draws: not in the scrollback yet, left
    // to the row by the live block, since this is where they'll land.
    pending: Vec<PendingTool>,
    // The window over the steps' lines: every line is there, the frame
    // shows a slice of them.
    window: Window,
    // Every line shows, not the frame's slice of them.
    whole: bool,
    // The rows the frame's window grew to when the reader opened a step in a
    // group that fit it: the group shows whole rather than clip a line away
    // for a bar. Zero until then, and again once the group fits the frame's.
    grown: usize,
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
            window: Window::default(),
            whole: false,
            grown: 0,
            hovered: None,
            spinner: 0,
        }
    }

    /// Whether every step is closed. Answered from the steps: a group is
    /// folded while none of them shows open, however they came to be closed.
    pub(super) fn folded(&self) -> bool {
        let rows = self.height();
        !self.steps.iter().any(|step| step.shows_open(rows))
    }

    pub(super) fn join(&mut self, step: Step) {
        self.steps.push(step);
    }

    pub(super) fn set_folded(&mut self, to: bool) {
        for step in &mut self.steps {
            step.set_open(!to);
        }
        // Folded is the frame's slice again, however the reader left it.
        self.whole = false;
        self.grown = 0;
        self.follow_newest();
    }

    // Start over at the newest line, keeping the height the frame allowed.
    fn follow_newest(&mut self) {
        self.window = Window {
            rows: self.window.rows,
            ..Window::default()
        };
    }

    // Show every line, or the frame's slice of them again. The bar stays as
    // the way back.
    fn toggle_whole(&mut self) {
        self.whole = !self.whole;
        self.follow_newest();
    }

    // The height the frame allows the group; true when it moved. The bar's
    // row comes off it, and a step too tall for what is left closes.
    pub(super) fn fit(&mut self, rows: usize) -> bool {
        let rows = WINDOW_ROWS.min(rows.saturating_sub(1).max(1));
        let moved = self.window.rows != rows;
        self.window.rows = rows;
        if moved {
            let rows = self.height();
            for step in &mut self.steps {
                if !step.fits(rows) {
                    step.set_open(false);
                }
            }
        }
        moved
    }

    pub(super) fn has_tools(&self) -> bool {
        !self.pending.is_empty() || self.steps.iter().any(|step| matches!(step, Step::Tool(_)))
    }

    // The step content line `i` falls in, head or not.
    fn page_step(&self, i: usize) -> Option<&Step> {
        walk(&self.steps, self.height(), i)
            .ok()
            .and_then(|(k, _)| self.steps.get(k))
    }

    // The page that reads window row `i`, when the window cannot show what
    // the line opens: the step stays closed and the page reads it whole.
    pub(super) fn page_at(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
        screen: usize,
    ) -> Option<Opening> {
        // The count under a call reads the rest of it whole; a step's own
        // line reads a step the window cannot hold. Anything else under it is
        // read where it stands, or by the window's bar.
        let (k, j) = self.row_at(i)?;
        let step = &self.steps[k];
        if step.pages_row(j) {
            return Some(step_page(step, paint));
        }
        if j != 0 || step.shows_open(self.height()) {
            return None;
        }
        if !step.opens_on_a_page(self.page_rows(screen), width) {
            return None;
        }
        Some(step_page(step, paint))
    }

    // The step whose own line window row `i` is, the bar row naming none.
    fn step_at(&self, i: usize) -> Option<usize> {
        step_index(&self.steps, self.height(), self.framed().at(i)?)
    }

    // The file the call on window row `i` works on, when it names one.
    pub(super) fn file(&self, i: usize) -> Option<&std::path::Path> {
        let at = self.framed().at(i)?;
        match unit_at(&self.steps, &self.pending, self.height(), at)? {
            (Unit::Step(Step::Tool(call)), 0) => call.file(),
            (Unit::Pending(tool), _) => tool.file.as_deref(),
            _ => None,
        }
    }

    // The step window row `i` falls in, and how far into its lines it lies.
    fn row_at(&self, i: usize) -> Option<(usize, usize)> {
        walk(&self.steps, self.height(), self.framed().at(i)?).ok()
    }

    // The rows the window may show: opened whole, every line; otherwise what
    // the frame gave, and always one.
    fn height(&self) -> usize {
        match self.whole {
            true => usize::MAX,
            // Grown to hold what the reader opened, and no lower than the
            // slice the frame gave.
            false => self.window.rows.max(1).max(self.grown),
        }
    }

    // The rows a step may take before a click reads it on a page instead:
    // its window's, or half the screen when the group has no window.
    fn page_rows(&self, screen: usize) -> usize {
        match self.whole {
            true => (screen / 2).max(1),
            false => self.height(),
        }
    }

    // The lines every step and call takes, one each unless a step opens
    // small enough to sit whole in a window `rows` high.
    fn content_len_in(&self, rows: usize) -> usize {
        self.steps
            .iter()
            .map(|step| step.len_in(rows))
            .sum::<usize>()
            + self.pending.len()
    }

    fn content_len(&self) -> usize {
        self.content_len_in(self.height())
    }

    // What the reader opened is closed again: a group grown to hold it keeps
    // holding what is left of it, and one that was windowed stays windowed —
    // the frame's slice, its bar the way back to the rest. Growing a windowed
    // group here took its bar away with nothing left to say it held lines.
    fn settle(&mut self) {
        let rows = self.window.rows.max(1);
        let total = self.content_len_in(rows.max(self.grown));
        self.grown = match self.grown {
            0 => 0,
            _ if total > rows => total,
            _ => 0,
        };
    }

    // Scroll so step `k` and everything it opens read whole.
    fn reveal(&mut self, k: usize) {
        let rows = self.height();
        self.window.top = self.steps[..k].iter().map(|s| s.len_in(rows)).sum();
        self.window.follow = false;
    }

    // What the window shows this frame.
    fn framed(&self) -> Framed {
        let total = self.content_len();
        let rows = self.height();
        // The bar is the row under the window, not one of its rows.
        let body = total.min(rows);
        let past = total - body;
        let top = match self.window.follow {
            true => past,
            false => self.window.top.min(past),
        };
        // Under a window opened whole the bar is the way back, so it stays
        // with nothing past it.
        Framed {
            body,
            top,
            past,
            bar: past > 0 || self.whole,
        }
    }

    /// Move the window `step` lines; false when it had nowhere to go, so a
    /// wheel can hand the turn to the transcript.
    pub(super) fn scroll(&mut self, up: bool, step: usize) -> bool {
        let framed = self.framed();
        let past = framed.past;
        if past == 0 {
            self.window.top = 0;
            self.window.follow = true;
            return false;
        }
        let top = match up {
            true => framed.top.saturating_sub(step),
            false => (framed.top + step).min(past),
        };
        let moved = top != framed.top;
        self.window.top = top;
        self.window.follow = top >= past;
        moved
    }

    pub(super) fn len(&self) -> usize {
        self.framed().len()
    }

    // Whether window row `i` of `framed` is the bar: the row that shows where
    // the window sits, which a click opens or closes the whole of.
    pub(super) fn is_bar(&self, i: usize) -> bool {
        bar_row(&self.framed(), i)
    }

    // Whether the wheel over the bar has a window to turn.
    pub(super) fn turns_window(&self) -> bool {
        self.framed().past > 0
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let framed = self.framed();
        if bar_row(&framed, i) {
            let mut bar = self.bar_line(&framed, paint);
            if self.hovered == Some(i) {
                paint.lit_spans(&mut bar.spans);
            }
            return (cut_line(bar, width), None);
        }
        let Some(at) = framed.at(i) else {
            return (Line::default(), None);
        };
        let (mut page, border) = self.drawn(i, at, paint, width);
        if let Some(prefix) = &page.prefix {
            page.line = prefix.fit_body(page.line, width);
        }
        let edge = border.as_ref().map_or(0, Line::width);
        // One window row is one screen row: what overruns is cut, and the
        // page reads it whole.
        (cut_line(page.line, width.saturating_sub(edge)), border)
    }

    // The row under the steps: a bar for where the window sits, and what
    // it holds.
    fn bar_line(&self, framed: &Framed, paint: &Paint) -> Line<'static> {
        const CELLS: usize = 10;
        let past = framed.past;
        // Nothing past the window — the whole of it shown — lights every cell.
        let spent = (past.saturating_sub(framed.top) * CELLS)
            .div_ceil(past.max(1))
            .min(CELLS);
        let lit = CELLS - spent;
        let mut spans = Vec::with_capacity(4);
        if lit > 0 {
            spans.push(paint.span(&paint.theme.heading, icons::BAR_CELL.repeat(lit)));
        }
        if spent > 0 {
            spans.push(paint.span(&paint.theme.muted, icons::BAR_CELL.repeat(spent)));
        }
        spans.push(Span::raw(" "));
        spans.push(paint.span(&paint.theme.muted, self.tally()));
        Line::from(spans)
    }

    // What the group holds: each tool with how often it ran, the heaviest
    // first, then the reasoning. A tool's name is never pluralised.
    fn tally(&self) -> String {
        let mut tools: BTreeMap<&str, usize> = BTreeMap::new();
        let mut lines = 0;
        for step in &self.steps {
            match step {
                Step::Tool(tool) => *tools.entry(&tool.name).or_default() += 1,
                Step::Thinking { lines: held, .. } => lines += held.len(),
            }
        }
        for tool in &self.pending {
            *tools.entry(&tool.name).or_default() += 1;
        }
        // Heaviest first; the map has read the equal ones by name already.
        let mut tools: Vec<(&str, usize)> = tools.into_iter().collect();
        tools.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let mut said: Vec<String> = tools
            .into_iter()
            .map(|(name, n)| format!("{n} {name}"))
            .collect();
        if lines > 0 {
            said.push(super::count(lines, "thinking line"));
        }
        said.join(icons::PART_SEP)
    }

    // What in window row `i` reads as one, as the line `line` gives holds it.
    pub(super) fn units(&self, i: usize, paint: &Paint) -> crate::unit::Units {
        self.framed()
            .at(i)
            .map_or_else(Vec::new, |at| self.drawn(i, at, paint, usize::MAX).0.units)
    }

    pub(super) fn continuation(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> Option<Line<'static>> {
        let at = self.framed().at(i)?;
        self.drawn(i, at, paint, width).0.prefix.map(|p| p.rest)
    }

    // Window row `i` at content line `at`, with what in it reads as one,
    // and its border.
    fn drawn(
        &self,
        i: usize,
        at: usize,
        paint: &Paint,
        width: usize,
    ) -> (PageLine, Option<Line<'static>>) {
        let rows = self.height();
        let tick = self.spinner;
        let hovered = self.hovered == Some(i);
        let (line, border) = match unit_at(&self.steps, &self.pending, rows, at) {
            // A call draws its own line, open or closed, and the rows under it.
            Some((Unit::Step(Step::Tool(call)), j)) => {
                call.line(j, usize::MAX, hovered.then_some(j), paint, width)
            }
            Some((Unit::Step(s), j)) if s.shows_open(rows) => {
                return s.drawn(j, hovered, tick, paint);
            }
            Some((Unit::Step(s), _)) => s.head(tick).drawn(false, hovered, paint, width),
            Some((Unit::Pending(p), _)) => pending_head(p, tick).drawn(false, false, paint, width),
            None => (Line::default(), None),
        };
        (line.into(), border)
    }

    // Hover line, spinner tick and the calls in flight it draws; whether
    // its count may have moved — a tick alone can't, lines are clipped.
    pub(super) fn update(
        &mut self,
        hovered: Option<usize>,
        spin: usize,
        held: Vec<PendingTool>,
    ) -> bool {
        self.hovered = hovered;
        self.spinner = spin;
        if self.pending == held {
            return false;
        }
        let reshaped = self.pending.len() != held.len()
            || self.pending.iter().zip(&held).any(|(a, b)| {
                (&a.name, &a.summary, a.landed()) != (&b.name, &b.summary, b.landed())
            });
        self.pending = held;
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
}

impl Fold for Group {
    fn continuation(&self, i: usize, paint: &Paint, width: usize) -> Option<Line<'static>> {
        Group::continuation(self, i, paint, width)
    }

    fn is_open(&self) -> bool {
        true
    }

    fn expandable(&self) -> bool {
        self.content_len() > 1 || self.steps.iter().any(|s| s.opens(usize::MAX))
    }

    fn flip(&mut self) {
        self.set_folded(!self.folded());
    }

    fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        if self.is_bar(i) {
            return Some(i);
        }
        let (k, j) = self.row_at(i)?;
        self.steps[k].opens_at(j, width).then_some(i)
    }

    fn toggle_at(&mut self, i: usize, width: usize, screen: usize) -> bool {
        if self.is_bar(i) {
            self.toggle_whole();
            return true;
        }
        // Only a step's own line opens or closes it: the count under it is
        // the page's, which `page_at` answers.
        let Some(k) = self.step_at(i) else {
            return false;
        };
        if !self.steps[k].opens(width) {
            return false;
        }
        if self.steps[k].is_open() {
            self.steps[k].flip();
            self.settle();
            return true;
        }
        // Taller than it may open here: the page reads it, `page_at` answers.
        let rows = self.page_rows(screen);
        if self.steps[k].opens_on_a_page(rows, width) {
            return false;
        }
        // A group that fit its window holds no line back for what the reader
        // opens: the window grows to hold it, the bar's row included. A group
        // already past its window keeps the slice it had, and the bar says so.
        let window = self.window.rows.max(1);
        let whole = self.grown > 0 || self.content_len_in(window) <= window;
        self.steps[k].flip();
        if whole {
            self.grown = self.content_len_in(window.max(self.grown));
        }
        self.reveal(k);
        true
    }

    fn opening(&self, i: usize, paint: &Paint) -> Option<Opening> {
        let step = self.page_step(self.framed().at(i)?)?;
        Some(step_page(step, paint))
    }
}

fn step_page(step: &Step, paint: &Paint) -> Opening {
    let lines = match step {
        // A call reads its own rows whole, the way it drew them.
        Step::Tool(call) => return call.page(paint),
        Step::Thinking { lines, .. } => lines,
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

    use crate::tui::row::test_call as call;

    fn tool(name: &str, screen: &str) -> Call {
        call(name, screen, "", "")
    }

    // A folded group of these calls, in order.
    fn bundle(tools: Vec<Call>) -> Row {
        let mut it = tools.into_iter().map(Step::Tool);
        let mut row = Row::steps(it.next().expect("a group holds a step"), true);
        for step in it {
            assert!(row.join(step).is_ok());
        }
        row
    }

    // A call in flight, as the row is handed one: `landed` says whether its
    // result is in yet.
    fn pending(name: &str, summary: &str, landed: bool) -> PendingTool {
        PendingTool {
            name: name.to_string(),
            summary: summary.to_string(),
            stage: if landed {
                Stage::Landed(Outcome::Done)
            } else {
                Stage::Running { progress: None }
            },
            secs: None,
            cwd: None,
            file: None,
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
            vec![
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
        row.update_live(None, 0, vec![pending("grep", "match 1", false)]);
        assert_eq!(row.len(), 2, "one line per call, without a group header");
        assert!(shown(&row, 80)[1].contains("grep match 1"));

        row.update_live(None, 0, Vec::new());
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

    // Folding is answered from the steps rather than kept beside them, so a
    // group whose steps are all closed is folded however they came to be.
    #[test]
    fn a_group_answers_folded_from_its_steps() {
        let paint = Paint::new(false);
        // Opened whole, then the only step closes on a click on its own line.
        let opened = call("read", "read a.rs", r#"{"path":"a.rs"}"#, "");
        let mut row = bundle(vec![opened]);
        row.set_folded(false);
        assert_eq!(row.folded(), Some(false), "opened whole");
        assert!(
            row.toggle_at(0, &paint, 80, usize::MAX),
            "a call with an argument opens"
        );
        assert_eq!(row.folded(), Some(true), "closed by its own line");

        // The step that leaves the group takes its openness with it: what is
        // left has nothing open, so nothing is unfolded.
        let mut row = Row::steps(think(7, 0, false), false);
        assert_eq!(row.folded(), Some(false), "opened whole");
        assert!(row.end_block(7, &paint), "an empty block leaves no step");
        assert_eq!(row.folded(), Some(true), "nothing is open");
    }

    fn think(block: u64, n: usize, live: bool) -> Step {
        let lines = (1..=n).map(|i| Line::from(format!("line {i}"))).collect();
        Step::thinking(block, lines, live, &Paint::new(false))
    }

    // Opened, a live block keeps its head and newest lines around a count:
    // the latest thinking is what the person waits to read.
    #[test]
    fn an_opened_live_block_keeps_its_head_and_newest_lines() {
        let row = Row::steps(think(7, 10, true), false);
        assert_eq!(row.len(), FOLD_ROWS);
        let lines = shown(&row, 80);
        assert!(lines[0].contains("line 1"), "{lines:?}");
        assert!(lines[1].contains("line 2"), "{lines:?}");
        assert!(
            lines[2].contains(&format!("{} {} more", icons::ELLIPSIS, 5)),
            "{lines:?}"
        );
        assert!(lines[3].contains("line 8"), "{lines:?}");
        assert!(lines[4].contains("line 9"), "{lines:?}");
        assert!(lines[5].contains("line 10"), "{lines:?}");

        // As many lines as the opened rows hold, none of them is folded away.
        let all = shown(&Row::steps(think(7, FOLD_ROWS, true), false), 80);
        assert!(all[FOLD_ROWS - 1].contains("line 6"), "{all:?}");
    }

    // Closed, a block is one line whatever it holds.
    #[test]
    fn a_closed_live_block_keeps_its_head() {
        let row = Row::steps(think(7, 10, true), true);
        assert_eq!(row.len(), 1);
        assert!(shown(&row, 80)[0].contains("line 1"));
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
    // indent it sat under aside; a line cut to one row is read there whole.
    #[test]
    fn a_page_reads_what_opening_in_place_shows() {
        use crate::tui::screen::plain;
        let paint = Paint::new(false);
        let opens_as_in_place = |mut row: Row, i: usize| {
            let page: Vec<String> = (row.opening(i, &paint).expect("it opens").iter())
                .map(|l| plain(&l.line))
                .collect();
            assert!(row.toggle_at(i, &paint, 80, usize::MAX));
            let drawn: Vec<String> = (i..i + page.len())
                .map(|at| plain(&row.line(at, &paint, 80).0))
                .collect();
            for (whole, shown) in page.iter().zip(&drawn) {
                let cut = shown.trim_end_matches(icons::ELLIPSIS);
                assert!(whole.starts_with(cut), "{whole} / {shown}");
            }
        };
        // A call short enough to show opens in place; one cut to its row is
        // read on the page instead, the only place that shows it whole.
        let long = format!(
            "grep -rn banner {}",
            "crates/ui/src/tui/tests/*.rs ".repeat(4)
        );
        let ran = call("bash", &long, &long, "one\ntwo\n");
        let mut listed = bundle(vec![tool("read", "a.rs")]);
        assert!(listed.join(think(1, 3, false)).is_ok());
        assert!(listed.join(Step::Tool(ran)).is_ok());
        listed.prepare_layout(&paint, 80, 40);
        assert!(
            !listed.toggle_at(2, &paint, 80, usize::MAX),
            "a cut line is read on a page"
        );
        assert!(
            listed.page_at(2, &paint, 80, usize::MAX).is_some(),
            "the page holds it"
        );

        let short = call("bash", "cat a", "cat a", "one\ntwo\n");
        let mut plain_row = bundle(vec![tool("read", "a.rs")]);
        assert!(plain_row.join(Step::Tool(short)).is_ok());
        plain_row.prepare_layout(&paint, 80, 40);
        opens_as_in_place(plain_row, 1);
        opens_as_in_place(
            Row::thinking(
                2,
                (1..=4).map(|i| Line::from(format!("l{i}"))).collect(),
                true,
            ),
            0,
        );
    }

    // Every step is visible and answers only to its own heading.
    #[test]
    fn steps_are_visible_and_each_opens_alone() {
        let paint = Paint::new(false);
        let ran = call("bash", "cat <<EOF", "cat <<EOF\nhi\nEOF", "hi\n")
            .timed(Some(std::time::Duration::from_secs(5)));
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(row.join(think(1, 3, false)).is_ok());
        assert!(row.join(Step::Tool(ran)).is_ok());
        assert_eq!(row.len(), 3);
        let unfolded = shown(&row, 80);
        assert_eq!(unfolded.len(), 3, "one line per step");

        assert_eq!(row.click_line(0, 80), None, "nothing under the read");
        assert!(
            row.toggle_at(1, &paint, 80, usize::MAX),
            "the reasoning opens"
        );
        assert_eq!(row.len(), 5);
        assert!(
            row.toggle_at(4, &paint, 80, usize::MAX),
            "the call past it still answers"
        );
        // Its own line, then a blank, one row of what came back, and the
        // count over the rest of what it was made with.
        let lines = shown(&row, 80);
        let head = lines
            .iter()
            .position(|line| line.contains("cat <<EOF"))
            .expect("the call's own line");
        assert_eq!(
            lines[head + 1].trim(),
            "",
            "a blank row parts command from output"
        );
        assert_eq!(
            lines[head + 2].trim(),
            "hi",
            "what came back, under the blank"
        );
        assert!(lines[head + 3].contains("more"), "{lines:?}");
        assert_eq!(row.len(), head + 4, "nothing past the count");
        assert_eq!(
            row.click_line(head + 1, 80),
            None,
            "a body line opens nothing"
        );
        assert!(
            row.toggle_at(1, &paint, 80, usize::MAX) && row.toggle_at(2, &paint, 80, usize::MAX)
        );
        assert_eq!(shown(&row, 80), unfolded);
    }

    // A group of one step has no list to open: opening it opens the step,
    // its lines and no header above them.
    #[test]
    fn a_group_of_one_opens_its_step() {
        let paint = Paint::new(false);
        let mut row = Row::steps(think(1, 2, false), true);
        assert!(row.toggle_expand());
        assert_eq!(row.len(), 2);

        let mut one = bundle(vec![call("read", "a.rs", "a.rs", "fn main() {}")]);
        assert!(one.toggle_at(0, &paint, 80, usize::MAX));
        assert_eq!(one.len(), 3, "its line, a blank, and what it printed");

        // Before its first line lands there is nothing to unfold.
        let empty = Row::steps(think(2, 0, false), false);
        assert!(!empty.is_expandable());
    }

    // One call or many, the text runs up to what follows it, `…` on its end.
    #[test]
    fn a_long_call_fills_the_row_before_what_follows_it() {
        let long = "x".repeat(200);
        let mut row = Row::steps(think(1, 1, false), true);
        assert!(row.join(Step::Tool(tool("bash", &long))).is_ok());
        assert!(row.join(Step::Tool(tool("bash", &long))).is_ok());
        let line = &shown(&row, 80)[1];
        assert!(line.ends_with(icons::ELLIPSIS), "{line}");
        assert_eq!(pi_store::text::width(line), 79, "{line}");

        let one = bundle(vec![tool("bash", &long)]);
        assert_eq!(pi_store::text::width(&shown(&one, 80)[0]), 79);
    }
}
