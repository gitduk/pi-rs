//! A row of folded steps: the read-only calls and reasoning of a run,
//! one line closed, a list open, each step opening on its own.

use ratatui::text::{Line, Span};

use super::{Fold, Opening, count};
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
        // Only a head that reads the first line already can leave it out:
        // a script's head is its name alone.
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
    const TICKS_PER_FRAME: usize = 2;
    icons::CALL_FRAMES[tick / TICKS_PER_FRAME % icons::CALL_FRAMES.len()]
}

/// What a call's line ends with: its progress, if it reports any, then how
/// long it has been out or ran, as `render::ran_for` says it.
pub fn out_for(progress: Option<&str>, secs: Option<u64>) -> String {
    let progress = progress.map_or_else(String::new, |p| format!("{}{p}", icons::PART_SEP));
    progress + &render::ran_for(secs.map(std::time::Duration::from_secs))
}

// Whether the row still has a call out. Any of them, not the newest:
// the row is the only place a call in flight shows.
fn running(pending: &[PendingTool]) -> bool {
    pending.iter().any(|p| !p.landed())
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

/// The mark a line naming a step or a call leads with.
#[derive(Clone, Copy)]
pub enum StepMark {
    /// Landed, as it came out.
    Ended(Outcome),
    /// Out, turning at this spinner tick.
    Out(usize),
    /// Waiting its turn: still, since nothing runs yet.
    Queued,
    /// Reasoning wears none: a call is the thing that gets a mark.
    Thought,
}

impl StepMark {
    /// The glyph alone, for a line drawn in one style.
    pub fn glyph(self) -> &'static str {
        match self {
            StepMark::Ended(outcome) => render::outcome_glyph(outcome),
            StepMark::Out(tick) => call_frame(tick),
            StepMark::Queued => icons::PENDING_MARK,
            StepMark::Thought => "",
        }
    }

    // The mark and the space after it, or nothing.
    fn spans(self, hovered: bool, paint: &Paint) -> Vec<Span<'static>> {
        let style = match self {
            StepMark::Ended(outcome) => render::outcome_style(outcome, &paint.theme),
            StepMark::Out(_) | StepMark::Queued => &paint.theme.muted,
            StepMark::Thought => return Vec::new(),
        };
        vec![
            paint.span_hovered(hovered, style, self.glyph()),
            paint.span_hovered(hovered, &paint.theme.muted, " "),
        ]
    }

    fn width(self) -> usize {
        match self {
            StepMark::Thought => 0,
            _ => 2,
        }
    }
}

// What one line naming a step or a call says, before it is fitted to a width.
struct Head {
    mark: StepMark,
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
            .saturating_sub(self.mark.width() + pi_store::text::width(&self.tail) + 1)
            .max(8)
    }

    fn cut(&self, width: usize) -> bool {
        pi_store::text::width(&self.text) > self.room(width)
    }

    // Opened, the text is whole and wraps, and what the tail said is below it.
    fn line(&self, open: bool, hovered: bool, paint: &Paint, width: usize) -> Line<'static> {
        let text = if open {
            self.text.clone()
        } else {
            let text = pi_store::text::clip_more(&self.text, self.room(width), self.more);
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

// `timed` keeps how long it has been out or ran on the tail.
fn pending_head(p: &PendingTool, tick: usize, timed: bool) -> Head {
    let place = render::placed(p.place.as_deref());
    Head {
        mark: p.stage.mark(tick),
        text: p.desc(),
        tail: match timed {
            true => place + &out_for(p.stage.progress(), p.secs),
            false => place,
        },
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
                mark: StepMark::Ended(t.outcome),
                text: t.desc(),
                tail: t.placed() + &render::ran_for(t.took),
                more: t.asked > 0,
            },
            Step::Thinking { lines, .. } => Head {
                mark: StepMark::Thought,
                text: lines
                    .first()
                    .map_or_else(|| THINKING.to_string(), screen::plain),
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

// A group's own line, the same folded or open: its newest call as that
// call's own line reads it, then how many more the group holds.
fn steps_header(
    steps: &[Step],
    pending: &[PendingTool],
    open: bool,
    hovered: bool,
    tick: usize,
    paint: &Paint,
    width: usize,
) -> Line<'static> {
    let calls = calls(steps, pending);
    // The group's mark, not its newest call's: a block still streaming
    // after the last call landed means the group is not done.
    let thinking = thinking_now(steps).is_some() && !running(pending);
    let mark = if running(pending) || thinking {
        StepMark::Out(tick)
    } else {
        StepMark::Ended(ended(steps, pending))
    };
    // The newest call's line; once it lands, less its time, which is the
    // call's alone.
    let newest = match pending.last() {
        Some(p) => Some(pending_head(p, tick, !p.landed())),
        None => steps.iter().rev().find_map(|s| match s {
            Step::Tool(t) => Some(Head {
                tail: t.placed(),
                ..s.head()
            }),
            Step::Thinking { .. } => None,
        }),
    };
    let Some(Head {
        text,
        mut tail,
        more,
        ..
    }) = newest
    else {
        return Line::from(paint.span_hovered(hovered, &paint.theme.muted, THINKING.to_string()));
    };
    if calls > 1 {
        tail.push_str(&format!("{}{} more", icons::PART_SEP, calls - 1));
    }
    if thinking {
        tail.push_str(&format!("{}{THINKING}", icons::PART_SEP));
    }
    let head = Head {
        mark,
        text,
        tail,
        more,
    };
    // Opened, a lone step is that step opened; a list's header stays as folded.
    head.line(
        open && lone(steps, pending).is_some(),
        hovered,
        paint,
        width,
    )
}

// How the group came out: warned if any call it holds asks for a look.
fn ended(steps: &[Step], pending: &[PendingTool]) -> Outcome {
    let warned = steps
        .iter()
        .any(|s| matches!(s, Step::Tool(t) if t.outcome == Outcome::Warned))
        || (pending.iter()).any(|p| p.stage == Stage::Landed(Outcome::Warned));
    if warned {
        Outcome::Warned
    } else {
        Outcome::Done
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
            pending_head(p, tick, true).line(false, false, paint, room),
            STEP_INDENT,
        ),
        None => (Line::default(), STEP_INDENT),
    };
    (line, Some(Line::from(indent)))
}

/// The run's working behind one fold: read-only calls that landed well and
/// the reasoning around them, in order.
pub(super) struct Group {
    steps: Vec<Step>,
    // Calls in flight this row draws: not in the scrollback yet, left
    // to the row by the live block, since this is where they'll land.
    pending: Vec<PendingTool>,
    pub(super) folded: bool,
    // The line under the mouse, when it is one a click opens or closes.
    hovered: Option<usize>,
    // The tick the marks of calls still out turn at.
    spinner: usize,
}

impl Group {
    pub(super) fn new(first: Step, folded: bool) -> Self {
        Self {
            steps: vec![first],
            pending: Vec::new(),
            folded,
            hovered: None,
            spinner: 0,
        }
    }

    pub(super) fn join(&mut self, step: Step) {
        self.steps.push(step);
    }

    fn lone(&self) -> Option<&Step> {
        lone(&self.steps, &self.pending)
    }

    // Unfolded, and with more to show than its own line.
    fn open(&self) -> bool {
        !self.folded && self.expandable()
    }

    pub(super) fn len(&self) -> usize {
        if !self.open() {
            return 1;
        }
        match self.lone() {
            // The block is the group: its lines, with no line above.
            Some(Step::Thinking { lines, .. }) => lines.len(),
            Some(Step::Tool(t)) => 1 + t.body.len(),
            None => 1 + self.steps.iter().map(Step::len).sum::<usize>() + self.pending.len(),
        }
    }

    pub(super) fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let open = self.open();
        let hover = |at: usize| self.hovered == Some(at);
        let (steps, pending, tick) = (&self.steps, &self.pending, self.spinner);
        match self.lone() {
            Some(Step::Thinking { lines, .. }) if open => {
                let mut line = lines[i].clone();
                // Bold as `span_hovered` bolds: only where there is colour.
                if hover(i) && paint.color {
                    for span in &mut line.spans {
                        span.style = Paint::lit(span.style);
                    }
                }
                (line, None)
            }
            Some(Step::Tool(t)) if open && i > 0 => {
                (body_line(t, i - 1, paint), Some(Line::from(STEP_INDENT)))
            }
            _ if !open || i == 0 => (
                steps_header(steps, pending, open, hover(0), tick, paint, width),
                None,
            ),
            _ => step_line(steps, pending, i - 1, hover(i), tick, paint, width),
        }
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
        if self.folded {
            return None;
        }
        let held = |s: &Step| matches!(s, Step::Thinking { block, .. } if *block == id);
        match self.lone() {
            Some(s) => held(s).then_some(""),
            None => self
                .steps
                .iter()
                .any(|s| held(s) && s.is_open())
                .then_some(STEP_INDENT),
        }
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
    pub(super) fn end_block(&mut self, id: u64) -> bool {
        let hidden = self.folded;
        self.steps.retain_mut(|s| match s {
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
        self.steps.is_empty()
    }

    // The step whose own line is line `i` of the group, while it lists them.
    fn listed_step(&self, i: usize) -> Option<usize> {
        if self.folded || self.lone().is_some() {
            return None;
        }
        step_index(&self.steps, i.checked_sub(1)?)
    }
}

impl Fold for Group {
    fn expandable(&self) -> bool {
        expandable(&self.steps, &self.pending)
    }

    fn flip(&mut self) {
        self.folded = !self.folded;
    }

    // The group from its own line, a listed step from its.
    fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        if i == 0 {
            return self.expandable().then_some(0);
        }
        self.listed_step(i)
            .filter(|&k| self.steps[k].opens(width))
            .map(|_| i)
    }

    fn toggle_at(&mut self, i: usize, width: usize) -> bool {
        if i == 0 {
            return self.expandable() && {
                self.flip();
                true
            };
        }
        if self.click_line(i, width) != Some(i) {
            return false;
        }
        let Some(k) = self.listed_step(i) else {
            return false;
        };
        self.steps[k].flip();
        true
    }

    // A step closed on line `i`: a lone one from the folded line, a
    // listed one from its own.
    fn opening(&self, i: usize, paint: &Paint, width: usize) -> Option<Opening> {
        let (head, step) = match self.lone() {
            // The group is the step: opening the group opens it.
            Some(step) if i == 0 && self.folded => {
                let head = steps_header(
                    &self.steps,
                    &self.pending,
                    false,
                    false,
                    self.spinner,
                    paint,
                    width,
                );
                (head, step)
            }
            None if i > 0 => {
                let step = &self.steps[self.listed_step(i)?];
                if step.is_open() {
                    return None;
                }
                let head = step_line(
                    &self.steps,
                    &self.pending,
                    i - 1,
                    false,
                    self.spinner,
                    paint,
                    width,
                )
                .0;
                (head, step)
            }
            _ => return None,
        };
        let body = match step {
            // Read as an answer is, markdown and all: muted is for the
            // transcript, where it steps back from what was said.
            Step::Thinking { lines, .. } => {
                let text: Vec<String> = lines.iter().map(screen::plain).collect();
                render::render_coded(&text.join("\n"), paint)
                    .into_iter()
                    .map(PageLine::from)
                    .collect()
            }
            Step::Tool(t) => (0..t.body.len())
                .map(|j| body_line(t, j, paint).into())
                .collect(),
        };
        Some((head, body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render;
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

    // What a header holding `calls` calls ends with.
    fn more_tail(calls: usize) -> String {
        format!("{}{} more", icons::PART_SEP, calls - 1)
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
            format!("{} read b.rs{}", call_frame(0), more_tail(3)),
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

    // Folded is one line; unfolded, a line per step under that same line —
    // each step opens alone, named by the click's line.
    #[test]
    fn a_group_unfolds_to_its_steps_and_each_step_opens_alone() {
        let ran = FoldedTool::new("bash", "cat <<EOF", "cat <<EOF\nhi\nEOF", "hi\n")
            .timed(Some(std::time::Duration::from_secs(5)));
        let mut row = bundle(vec![tool("read", "a.rs")]);
        assert!(row.join(think(1, 3)).is_ok());
        assert!(row.join(Step::Tool(ran)).is_ok());
        let (done, dots, sep) = (icons::DONE_MARK, icons::ELLIPSIS, icons::PART_SEP);
        let header = format!("{done} bash cat <<EOF{dots}{}", more_tail(2));
        assert_eq!(shown(&row, 80), std::slice::from_ref(&header));
        assert!(row.toggle_expand());
        let unfolded = [
            header,
            format!("  {done} read a.rs"),
            format!("  line 1{dots}{sep}3 lines"),
            format!("  {done} bash cat <<EOF{dots}{sep}5s"),
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
        assert_eq!(shown(&row, 80), [THINKING]);
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
        let tail = format!("{}{}", icons::ELLIPSIS, more_tail(2));
        assert!(line.ends_with(&tail), "{line}");
        assert_eq!(pi_store::text::width(line), 79, "{line}");

        let one = bundle(vec![tool("bash", &long)]);
        assert_eq!(pi_store::text::width(&shown(&one, 80)[0]), 79);
    }

    #[test]
    fn result_with_many_diff_lines_expands_and_collapses() {
        let mut preview = "crates/foo.rs +30 -0".to_string();
        for i in 1..=30 {
            preview.push_str(&format!("\n  {i} + line {i}"));
        }
        let mut row = Row::result(crate::render::Outcome::Done, "edit", preview);
        assert_eq!(row.len(), render::SKETCHED_ROWS + 1);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 32);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), render::SKETCHED_ROWS + 1);
    }
}
