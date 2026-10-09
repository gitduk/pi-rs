//! A row of folded steps: the read-only calls and reasoning of a run,
//! one line closed, a list open, each step opening on its own.

use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::{clipped, count};
use crate::render::Paint;
use crate::tui::{THINKING, screen};
use pi_store::icons;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedTool {
    pub name: String,
    pub preview: String,
    // What the step shows opened, under its line: the lines of the argument
    // past the first the line names (`asked` of them), then what came back.
    pub(super) body: Vec<String>,
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
    /// What the call last said of how far it has got.
    pub progress: Option<String>,
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

/// What a call's line says while it is out: its progress, if it reports any,
/// then how long it has been out — nothing for a call over before that reads.
pub fn out_for(progress: Option<&str>, secs: Option<u64>) -> String {
    let secs = secs.map(|s| llm::figures::elapsed(std::time::Duration::from_secs(s)));
    progress
        .into_iter()
        .map(str::to_string)
        .chain(secs)
        .map(|part| format!("{}{part}", icons::PART_SEP))
        .collect()
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
pub(super) fn lone<'a>(steps: &'a [Step], pending: &[PendingTool]) -> Option<&'a Step> {
    match (steps, pending) {
        ([step], []) => Some(step),
        _ => None,
    }
}

// Whether unfolding would show more than the row's own line.
pub(super) fn expandable(steps: &[Step], pending: &[PendingTool]) -> bool {
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
        tail: out_for(p.progress.as_deref(), p.secs),
        more: false,
    }
}

impl Step {
    pub(super) fn is_open(&self) -> bool {
        match self {
            Step::Tool(t) => t.open,
            Step::Thinking { open, .. } => *open,
        }
    }

    pub(super) fn flip(&mut self) {
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
    pub(super) fn len(&self) -> usize {
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
    pub(super) fn opens(&self, width: usize) -> bool {
        self.is_open()
            || self.body_len() > 0
            || self.head().cut(width.saturating_sub(STEP_INDENT.len()))
    }
}

// What line `i` of an unfolded group's list sits in, and how far into it.
pub(super) enum Unit<'a> {
    Step(&'a Step),
    Pending(&'a PendingTool),
}

pub(super) fn unit_at<'a>(
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
pub(super) fn step_index(steps: &[Step], i: usize) -> Option<usize> {
    match walk(steps, i) {
        Ok((k, 0)) => Some(k),
        _ => None,
    }
}

// A group's own line: lists its steps, or (folded) names its newest
// call so the line holds still when the result lands.
pub(super) fn steps_header(
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
        0 => THINKING.to_string(),
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
            text: THINKING.to_string(),
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
            _ => THINKING.to_string(),
        };
        return Line::from(muted(text));
    };
    let mut tail = pending
        .last()
        .map(|p| out_for(p.progress.as_deref(), p.secs))
        .unwrap_or_default();
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
pub(super) fn body_line(t: &FoldedTool, j: usize, paint: &Paint) -> Line<'static> {
    let text = t.body.get(j).cloned().unwrap_or_default();
    if j < t.asked {
        Line::from(paint.span(&paint.theme.muted, text))
    } else {
        Line::from(text)
    }
}

// Line `i` of an unfolded group's list, the calls in flight last, with the
// indent it sits under as the border a wrap repeats.
pub(super) fn step_line(
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
            landed,
            progress: None,
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
