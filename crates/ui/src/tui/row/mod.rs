//! One row of scrollback, the only place one can be made. `Kind` is
//! private, so the two producers (live stream, rebuild) can't disagree.

use std::cell::{Cell, RefCell};
use std::num::NonZeroUsize;

use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};

use crate::block;
use crate::render::named;
use crate::render::{self, Paint};
use crate::tui::browse::PageLine;
use crate::tui::screen;
use pi_core::core::resolve::Resolved;
use pi_store::icons;

mod banner;
mod call;
mod code;
mod relayed;
mod steps;

use banner::Banner;
pub use call::Call;
#[cfg(test)]
pub(crate) use call::test_call;
use code::{Block, Code};
use relayed::Relayed;
use steps::Group;
pub use steps::{PendingTool, Stage, Step, StepMark, out_for};

pub struct Row(
    Kind,
    Height,
    Option<Box<render::Prefix>>,
    Option<agent::session::EntryId>,
);

// The rows this row takes on screen at one width, wraps included —
// counted per logical line and cached until width or content changes.
#[derive(Default)]
struct Height {
    measured: RefCell<Option<Measured>>,
    layout: RefCell<Option<Box<Layout>>>,
    hovered: Cell<Option<NonZeroUsize>>,
    // The rows a fold may take on screen, as the last frame allowed.
    budget: Cell<usize>,
}

struct Layout {
    width: usize,
    budget: usize,
    lines: Vec<LayoutLine>,
}

#[derive(Clone, Copy)]
enum LayoutLine {
    // `rows` is what the line still shows when the window cut it short.
    Content { source: usize, rows: Option<usize> },
    // The footer that opens the rest of a cut, and the line it cuts.
    More { heading: usize, remaining: usize },
}

impl LayoutLine {
    fn source(self) -> usize {
        match self {
            Self::Content { source, .. } => source,
            Self::More { heading, .. } => heading,
        }
    }

    fn content(source: usize) -> Self {
        Self::Content { source, rows: None }
    }

    fn rows(self, heights: &[usize]) -> usize {
        match self {
            Self::Content { source, rows: None } => heights[source],
            Self::Content {
                rows: Some(rows), ..
            } => rows,
            Self::More { .. } => 1,
        }
    }
}

impl Layout {
    fn heading(&self, source: usize) -> Option<usize> {
        self.lines.iter().position(
            |line| matches!(line, LayoutLine::Content { source: at, .. } if *at == source),
        )
    }
}

// The rows an open fold may take: half the history, so a long one keeps
// filling a tall screen.
fn fold_budget(height: usize) -> usize {
    (height / 2).max(2).min(height.max(1))
}

// What the window can hold of a fold's lines: all of them, or a cut that
// spends its last row on a footer naming the rows left out.
fn within_budget(lines: Vec<LayoutLine>, heights: &[usize], budget: usize) -> Vec<LayoutLine> {
    let total: usize = lines.iter().map(|line| line.rows(heights)).sum();
    if total <= budget {
        return lines;
    }
    let mut kept = Vec::with_capacity(budget);
    let mut left = budget.saturating_sub(1);
    let mut shown = 0;
    let mut heading = 0;
    for line in lines {
        let rows = line.rows(heights);
        let take = rows.min(left);
        if take < rows {
            if take > 0
                && let LayoutLine::Content { source, .. } = line
            {
                kept.push(LayoutLine::Content {
                    source,
                    rows: Some(take),
                });
                shown += take;
            }
            heading = line.source();
            break;
        }
        left -= take;
        shown += take;
        kept.push(line);
    }
    kept.push(LayoutLine::More {
        heading,
        remaining: total - shown,
    });
    kept
}

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
        let mut held = self.measured.borrow_mut();
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
        self.measured
            .borrow()
            .as_ref()
            .filter(|m| m.width == width)
            .and_then(|m| m.lines.get(i).copied().flatten())
    }

    fn set(&self, width: usize, i: usize, height: usize) {
        self.at(width, i + 1).lines[i] = Some(height);
    }

    fn clear(&self) {
        *self.measured.borrow_mut() = None;
        *self.layout.borrow_mut() = None;
    }
}

enum Kind {
    // One logical line of a said prompt: border and body kept apart so
    // a wrap can repeat the rule or blank a command's sigil.
    Said {
        border: Line<'static>,
        // The line's text, in the input style, without the border.
        body: Line<'static>,
        // Only the first logical line carries the prompt's time.
        at: Option<u64>,
        hangs: bool,
    },
    // What the model answered, one row per markdown line, with what in it
    // reads as one. Its own kind, not a notice: it is half the conversation.
    Answer(Line<'static>, crate::unit::Units),
    // A line of an answer's code block, indented by `CODE_INDENT`.
    CodeLine(Line<'static>),
    Code(Code),
    Block(Block),
    // A painted line the screen alone knows: command output or a warning.
    // `times` collapses identical repeats into one row and a count.
    Notice {
        text: Line<'static>,
        times: usize,
        // Drawn only, never filed: what a tidy takes.
        aside: bool,
    },
    Banner(Banner),
    // An empty group marks a removed row.
    Lines(Vec<(Line<'static>, Option<Line<'static>>)>),
    AttachedSkills {
        line: Line<'static>,
        last: agent::session::EntryId,
    },
    Call(Call),
    Relayed(Relayed),
    Steps(Group),
}

/// What a page reads for a closed line: its lines whole, one per row.
pub type Opening = Vec<PageLine>;

/// A closed line's page before its shared indent is taken out: each line with
/// the border a wrap repeats.
pub type Opened = Vec<(PageLine, Option<Line<'static>>)>;

// Opened lines as a page reads them: each border before its line, less the
// indent they all share, which only sat them under the row's own line.
fn page_of(lines: Opened) -> Opening {
    let indent = |border: &Option<Line<'static>>| {
        let lead = border.as_ref().map(screen::plain).unwrap_or_default();
        lead.len() - lead.trim_start_matches(' ').len()
    };
    let shared = lines.iter().map(|(_, b)| indent(b)).min().unwrap_or(0);
    (lines.into_iter())
        .map(|(mut page, border)| {
            let mut cut = shared;
            let mut spans = border.as_ref().map(screen::spans_of).unwrap_or_default();
            for span in &mut spans {
                let k = span.content.len().min(cut);
                span.content = span.content[k..].to_string().into();
                cut -= k;
            }
            spans.retain(|s| !s.content.is_empty());
            if !spans.is_empty() {
                page.prepend(render::Prefix::hanging(Line::from(spans)));
            }
            page
        })
        .collect()
}

// A row that opens and closes. A new kind that folds implements this and
// joins `Row::fold`; nothing else about folding names the kinds.
trait Fold {
    // Whether the content is open in place and needs a layout budget.
    fn is_open(&self) -> bool;
    fn click_start(&self, _heading: usize) -> usize {
        0
    }
    fn continuation(&self, _i: usize, _paint: &Paint, _width: usize) -> Option<Line<'static>> {
        None
    }
    fn more_border(&self, _heading: usize, _paint: &Paint, _width: usize) -> Option<Line<'static>> {
        None
    }
    // Whether opening would show more than the row's own line.
    fn expandable(&self) -> bool;
    // Open if closed, close if open; the caller checked `expandable`.
    fn flip(&mut self);
    // The line a click on line `i` opens or closes: by default the row's
    // own, from anywhere on it.
    fn click_line(&self, _i: usize, _width: usize) -> Option<usize> {
        self.expandable().then_some(0)
    }
    // `click_line`, with the rows the row's place allows it: a call lays a
    // count row out among them, and the count reads on a page.
    fn click_at(&self, i: usize, _rows: usize, width: usize) -> Option<usize> {
        self.click_line(i, width)
    }
    // Open or close what line `i` heads; whether anything moved. `screen` is
    // the screen's rows: a group opened whole has no window, and caps at half.
    fn toggle_at(&mut self, _i: usize, _width: usize, _screen: usize) -> bool {
        self.expandable() && {
            self.flip();
            true
        }
    }
    // Whether line `i` is read on a page where it lies, whatever its height:
    // a call's count row reads the rest of it.
    fn paged(&self, _i: usize, _rows: usize, _width: usize) -> bool {
        false
    }
    // What a page reads for line `i`: the row's lines whole.
    fn opening(&self, i: usize, paint: &Paint) -> Option<Opening>;
    // Whether it draws its own rows inside the budget, in place of the slice
    // a layout is otherwise measured and cut to.
    fn lays_out(&self) -> bool {
        false
    }
    // The rows it takes once opened, its own line included.
    fn opened_rows(&self) -> usize {
        1
    }
    // Whether an opening too tall for the rows it may take is read on a page
    // rather than cut here with a count to click through.
    fn opens_on_a_page(&self) -> bool {
        false
    }
    // Its own line as it reads once opened, where that differs from the line
    // it shows shut — the measure below counts the opened one.
    fn opened_head(&self, _paint: &Paint) -> Option<(Line<'static>, Option<Line<'static>>)> {
        None
    }
}

impl Row {
    // A row whose height is not measured yet; every constructor starts here.
    fn new(kind: Kind) -> Self {
        Self(kind, Height::default(), None, None)
    }

    pub(super) fn entry(&self) -> Option<agent::session::EntryId> {
        self.3
    }

    pub(super) fn bind_entry(&mut self, id: agent::session::EntryId) {
        self.3 = Some(id);
    }

    // The row as something that opens and closes, if it is one.
    fn fold(&self) -> Option<&dyn Fold> {
        match &self.0 {
            Kind::Steps(g) => Some(g),
            Kind::Call(r) => Some(r),
            Kind::Relayed(r) => Some(r),
            _ => None,
        }
    }

    fn fold_mut(&mut self) -> Option<&mut dyn Fold> {
        match &mut self.0 {
            Kind::Steps(g) => Some(g),
            Kind::Call(r) => Some(r),
            Kind::Relayed(r) => Some(r),
            _ => None,
        }
    }

    /// Something only the screen ever knew (a lane's answer, a warning, a
    /// command's output), in the surface's own muted voice.
    pub fn notice(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
            aside: false,
        })
    }

    /// A notice the transcript never gets: gone with a tidy, or a rebuild.
    pub fn aside(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
            aside: true,
        })
    }

    pub fn is_aside(&self) -> bool {
        matches!(self.0, Kind::Notice { aside: true, .. })
    }

    /// Painted lines kept as one row: replaced whole, they leave the rows
    /// under them where they were.
    pub fn lines(lines: Vec<Line<'static>>) -> Self {
        Self::new(Kind::Lines(
            lines.into_iter().map(|line| (line, None)).collect(),
        ))
    }

    /// A blank line within rendered content.
    pub fn gap() -> Self {
        Self::new(Kind::Answer(Line::default(), Vec::new()))
    }

    /// Whether this row is where a turn begins: something said, or relayed.
    pub fn opens_turn(&self) -> bool {
        matches!(&self.0, Kind::Said { .. } | Kind::Relayed(_))
    }

    /// A turn pi sent on the model's behalf, folded under `label`, come in
    /// at `at` (wall-clock seconds).
    pub fn relayed(label: &str, text: &str, at: u64, paint: &Paint) -> Self {
        Self::new(Kind::Relayed(Relayed::new(label, text, at, paint)))
    }

    /// Plain text as notice rows, one per line — shared by the live
    /// stream (which also files it) and a rebuild (from the archived text).
    pub fn notice_lines(text: &str, paint: &Paint) -> Vec<Self> {
        text.lines()
            .map(|line| Line::from(paint.span(&paint.theme.muted, line)))
            .map(Self::notice)
            .collect()
    }

    /// An agent report with its marker kept outside the body text.
    pub fn report_lines(text: &str, paint: &Paint) -> Vec<Self> {
        let prefix = render::Prefix::hanging(Line::from(
            paint.span(&paint.theme.muted, icons::bar(icons::PI_MARK)),
        ));
        text.lines()
            .enumerate()
            .map(|(i, line)| {
                let mut row = Self::notice(Line::from(paint.span(&paint.theme.muted, line)));
                row.2 = Some(Box::new(if i == 0 {
                    prefix.clone()
                } else {
                    render::Prefix::hanging(prefix.rest.clone())
                }));
                row
            })
            .collect()
    }

    pub(super) fn into_aside(mut self) -> Self {
        if let Kind::Notice { aside, .. } = &mut self.0 {
            *aside = true;
        }
        self
    }

    /// Fold a repeat into this row, if it is the same notice: the scrollback
    /// then shows `line ×2` where a second row would have gone.
    ///
    /// Both notices must have the same spans and persistence. Other kinds
    /// never fold, and a tidy cannot change a filed notice's count.
    pub fn repeated(&mut self, row: &Self) -> bool {
        match (&mut self.0, &row.0) {
            (
                Kind::Notice { text, times, aside },
                Kind::Notice {
                    text: next,
                    times: more,
                    aside: next_aside,
                },
            ) if text == next && aside == next_aside && self.2 == row.2 && text.width() > 0 => {
                *times += more;
                self.1.clear();
                true
            }
            _ => false,
        }
    }

    /// One reasoning line. A reasoning line lives inside its group, not
    /// beside it.
    pub fn reasoning_line(line: &str, paint: &Paint) -> Line<'static> {
        Line::from(paint.span(&paint.reasoning_style(), line))
    }

    /// A whole assistant text block, for a caller that has one. Diagrams and
    /// tables retain their source so each width gets its own layout.
    pub fn answer(text: &str, paint: &Paint) -> Vec<Self> {
        let markdown = |text: &str| {
            render::render_coded(text, paint).into_iter().map(|r| {
                let prefix = r.prefix;
                let mut row = match r.coded {
                    render::Coded::Badge(code) => Self::new(Kind::Code(Code {
                        badge: r.line,
                        code,
                        note: Default::default(),
                    })),
                    render::Coded::Code => Self::new(Kind::CodeLine(r.line)),
                    render::Coded::Text => Self::new(Kind::Answer(r.line, r.units)),
                };
                row.2 = prefix.map(Box::new);
                row
            })
        };
        if !paint.color {
            return markdown(text).collect();
        }
        let blank = Self::gap;
        let is_blank = |row: &Self| matches!(row, Self(Kind::Answer(l, _), _, _, _) if l.spans.is_empty() && row.2.is_none());
        let mut rows: Vec<Self> = Vec::new();
        let pieces = block::pieces(text);
        let references = if pieces
            .iter()
            .any(|piece| matches!(piece, block::Piece::Block(block::Form::Table, _)))
        {
            block::references(text)
        } else {
            String::new()
        };
        for piece in pieces {
            match piece {
                block::Piece::Text(text) => rows.extend(markdown(text)),
                block::Piece::Block(form, source) => {
                    if rows.last().is_some_and(|r| !is_blank(r)) {
                        rows.push(blank());
                    }
                    let source = source.trim_end().to_string();
                    let source = if form == block::Form::Table && !references.is_empty() {
                        format!("{source}\n\n{references}")
                    } else {
                        source
                    };
                    rows.push(Self::new(Kind::Block(Block::new(form, source))));
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
        Self::new(Kind::Steps(Group::new(first, folded)))
    }

    /// A lone block of reasoning, as a group of one.
    #[cfg(test)]
    pub fn thinking(block: u64, lines: Vec<Line<'static>>, folded: bool) -> Self {
        let step = Step::thinking(block, lines, false, &Paint::new(false));
        Self::steps(step, folded)
    }

    /// Add a step to this row if it is a group; the step comes back if not.
    pub fn join(&mut self, step: Step) -> Result<(), Box<Step>> {
        let Kind::Steps(g) = &mut self.0 else {
            return Err(Box::new(step));
        };
        g.join(step);
        self.1.clear();
        Ok(())
    }

    /// Whether this row is a group.
    pub fn is_steps(&self) -> bool {
        matches!(&self.0, Kind::Steps(_))
    }

    /// The indent reasoning block `id` shows its lines under, if this group
    /// shows them; the block's unfinished line follows them there.
    pub fn shows_block(&self, id: u64) -> Option<&'static str> {
        match &self.0 {
            Kind::Steps(g) => g.shows_block(id),
            _ => None,
        }
    }

    /// Whether this row is an expandable row.
    pub fn is_expandable(&self) -> bool {
        self.fold().is_some_and(|f| f.expandable())
    }

    /// Toggle the row's own line, returning true if toggled.
    #[cfg(test)]
    pub fn toggle_expand(&mut self) -> bool {
        self.toggle_at(0, &Paint::new(false), 80, usize::MAX)
    }

    fn layout_line(&self, i: usize) -> Option<LayoutLine> {
        self.1.layout.borrow().as_deref()?.lines.get(i).copied()
    }

    fn source_line(&self, i: usize) -> usize {
        self.layout_line(i).map_or(i, LayoutLine::source)
    }

    // The line under the pointer, when there is one: the hover is kept
    // 1-based, a zero there meaning none.
    fn hover(&self) -> Option<usize> {
        self.1.hovered.get().map(|line| line.get() - 1)
    }

    // Whether line `i` is the one under the pointer.
    fn lit(&self, i: usize) -> bool {
        self.hover() == Some(i)
    }

    /// The line a click on line `i` opens or closes, if any: a call from its
    /// own line or the count under it, a group from its own line or a step's.
    pub fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        if self.has_more(i, width) {
            return Some(i);
        }
        let source = self.source_line(i);
        let heading = match &self.0 {
            Kind::Code(_) | Kind::Block(_) => Some(0),
            _ => self.fold()?.click_at(source, self.budget(), width),
        }?;
        self.1
            .layout
            .borrow()
            .as_deref()
            .map_or(Some(heading), |p| Some(p.heading(heading).unwrap_or(i)))
    }

    pub(super) fn click_start(&self, i: usize, paint: &Paint, width: usize) -> usize {
        let border = self
            .line(i, paint, width)
            .1
            .map_or(0, |border| border.width());
        if border >= width {
            return 0;
        }
        if self.has_more(i, width) {
            return border;
        }
        self.fold()
            .map_or(border, |fold| fold.click_start(self.source_line(i)))
    }

    // Whether a click reads this row on a page: what it opens is taller than
    // the frame allows, and it is not already open to be closed.
    fn too_tall(&self, paint: &Paint, width: usize) -> bool {
        let budget = self.1.budget.get();
        // Before a frame the budget is zero: nothing is too tall yet.
        budget > 0
            && self
                .fold()
                .is_some_and(|f| f.opens_on_a_page() && !f.is_open())
            && self.rows_open(paint, width) > budget
    }

    // The screen rows this row would take with everything it holds open,
    // wraps counted: the measure a fold's budget is spent in.
    fn rows_open(&self, paint: &Paint, width: usize) -> usize {
        let fold = self.fold();
        let head = fold.and_then(|f| f.opened_head(paint));
        (0..fold.map_or(1, Fold::opened_rows))
            .map(|i| {
                let (line, border) = match (i, &head) {
                    (0, Some((line, border))) => (line.clone(), border.clone()),
                    _ => self.full_line(i, paint, width),
                };
                screen::wrap_rows(border.as_ref(), &line, width)
            })
            .sum()
    }

    /// Open or close what line `i` heads — see `click_line` — and say
    /// whether anything moved. `screen` is the screen's rows, which cap a
    /// step a group opened whole may take before a page reads it.
    pub fn toggle_at(&mut self, i: usize, paint: &Paint, width: usize, screen: usize) -> bool {
        if self.too_tall(paint, width) {
            return false;
        }
        let source = self.source_line(i);
        let moved = self
            .fold_mut()
            .is_some_and(|f| f.toggle_at(source, width, screen));
        if moved {
            self.1.clear();
        }
        moved
    }

    /// What line `i` reads on a page: the line as
    /// it reads with nothing under the pointer, then what it opens onto — a
    /// step's reasoning or call, a result whole, a relayed turn's text.
    pub fn opening(&self, i: usize, paint: &Paint) -> Option<Opening> {
        self.fold()?.opening(self.source_line(i), paint)
    }

    /// The page that reads line `i`, when it cannot be shown whole where it
    /// lies: a step taller than the group's window — or, the group opened
    /// whole, than half a screen — or one with a line wider than the row.
    pub fn page_at(&self, i: usize, paint: &Paint, width: usize, screen: usize) -> Option<Opening> {
        if let Kind::Steps(group) = &self.0 {
            return group.page_at(i, paint, width, screen);
        }
        // A fold says itself which of its lines reads the rest, and which
        // line is cut.
        let line = self.source_line(i);
        if self
            .fold()
            .is_some_and(|f| f.paged(line, self.budget(), width))
        {
            return self.opening(i, paint);
        }
        // The cut spends screen rows, so this does too: lines that wrap past
        // the budget would be cut with a count to click through.
        if !self.too_tall(paint, width) {
            return None;
        }
        self.opening(i, paint)
    }

    /// Whether line `i` is a group window's bar: the row saying where the
    /// window sits, and the one a click answers.
    pub fn is_bar(&self, i: usize) -> bool {
        let Kind::Steps(group) = &self.0 else {
            return false;
        };
        group.is_bar(i)
    }

    /// The columns line `i` of a bar takes: the cells and the count, and not
    /// the blank rest of the row, which belongs to the transcript.
    pub fn bar_width(&self, i: usize, paint: &Paint, width: usize) -> usize {
        if !self.is_bar(i) {
            return 0;
        }
        let (line, border) = self.line(i, paint, width);
        let lead = border.as_ref().map_or(0, Line::width);
        let text = pi_store::text::width(&screen::plain(&line));
        lead + text
    }

    /// Whether the wheel over this row's bar turns a window: under a window
    /// opened whole there is none, so the transcript keeps the wheel.
    pub fn turns_window(&self) -> bool {
        let Kind::Steps(group) = &self.0 else {
            return false;
        };
        group.turns_window()
    }

    /// Turn a group's window by `step` lines; false when it has nowhere to
    /// go, or holds no window.
    pub fn scroll_window(&mut self, up: bool, step: usize) -> bool {
        let Kind::Steps(group) = &mut self.0 else {
            return false;
        };
        let moved = group.scroll(up, step);
        if moved {
            self.1.clear();
        }
        moved
    }

    pub(super) fn has_more(&self, i: usize, width: usize) -> bool {
        self.1.layout.borrow().as_deref().is_some_and(|p| {
            p.width == width && matches!(p.lines.get(i), Some(LayoutLine::More { .. }))
        })
    }

    /// Brings a row up to date before the frame draws: hover line, spinner
    /// tick, and the calls in flight this row draws.
    pub fn update_live(&mut self, hovered: Option<usize>, spin: usize, held: &[PendingTool]) {
        // Hover bolds, and bold is no wider: only what else changed moves
        // a height.
        self.1.hovered.set(
            hovered
                .and_then(|i| i.checked_add(1))
                .and_then(NonZeroUsize::new),
        );
        let hovered = hovered
            .filter(|&i| !matches!(self.layout_line(i), Some(LayoutLine::More { .. })))
            .map(|i| self.source_line(i));
        let reshaped = match &mut self.0 {
            Kind::Steps(g) => g.update(hovered, spin, held),
            Kind::Code(Code { note, .. }) | Kind::Block(Block { note, .. }) => {
                note.update(hovered.is_some())
            }
            Kind::Relayed(r) => {
                r.hover(hovered.is_some());
                false
            }
            _ => false,
        };
        if reshaped {
            self.1.clear();
        }
    }

    /// A tool call as its result leaves it: the live path, which never holds
    /// a `ToolResult`, and the rebuild's, which does.
    pub fn call(call: Call) -> Self {
        Self::new(Kind::Call(call))
    }

    /// Say `said` beside this row's first line, until `FLASH` has passed.
    pub fn say_copied(&mut self, said: String) {
        if let Kind::Code(Code { note, .. }) | Kind::Block(Block { note, .. }) = &mut self.0 {
            note.copied(said);
            self.1.clear();
        }
    }

    /// Whether a badge heads this row at `width`, for a note to stand beside.
    pub fn badged(&self, paint: &Paint, width: usize) -> bool {
        match &self.0 {
            Kind::Code(_) => true,
            Kind::Block(b) => b.badged(paint, width),
            _ => false,
        }
    }

    /// What a click on this row copies: a code block's text, or the source
    /// of a diagram or table.
    pub fn code(&self) -> Option<&str> {
        match &self.0 {
            Kind::Code(c) => Some(&c.code),
            Kind::Block(b) => Some(&b.source),
            _ => None,
        }
    }

    /// The line a call that was never answered keeps: stopped before it landed.
    pub fn tool_start(name: &str, summary: &str, paint: &Paint) -> Self {
        let border =
            Line::from(paint.span(&paint.theme.muted, format!("{} ", icons::STOPPED_MARK)));
        let mut row = Self::notice(Line::from(paint.tool_call(
            name,
            &named(name, summary),
            false,
        )));
        row.2 = Some(Box::new(render::Prefix::hanging(border)));
        row
    }

    pub(super) fn attached_skill(name: &str, id: agent::session::EntryId, paint: &Paint) -> Self {
        let mark = StepMark::Ended(agent::Outcome::Done);
        let border = Line::from(mark.spans(false, paint));
        let line = Line::from(vec![
            paint.span(&paint.theme.prompt.color, "skills"),
            paint.span(&paint.theme.code, format!(" {name}")),
        ]);
        let mut row = Self::new(Kind::AttachedSkills { line, last: id });
        row.2 = Some(Box::new(render::Prefix::hanging(border)));
        row
    }

    pub(super) fn extend_skills(
        &mut self,
        name: &str,
        id: agent::session::EntryId,
        paint: &Paint,
    ) -> bool {
        // An intervening entry can leave no row or no committed event.
        if let Kind::AttachedSkills { line, last } = &mut self.0
            && last.0.checked_add(1) == Some(id.0)
        {
            line.spans
                .push(paint.span(&paint.theme.code, format!(", {name}")));
            *last = id;
            self.1.clear();
            return true;
        }
        false
    }

    fn said(border: Line<'static>, body: Line<'static>, at: Option<u64>, hangs: bool) -> Self {
        Self::new(Kind::Said {
            border,
            body,
            at,
            hangs,
        })
    }

    /// A prompt's lines as the stream echoed them, with the input's alignment.
    pub fn prompt(text: &str, at: u64, paint: &Paint) -> Vec<Self> {
        let bangs = pi_core::input::bangs(text);
        if bangs > 0 {
            let sigil = match bangs {
                1 => icons::BANG_SIGIL,
                _ => icons::BESIDE_SIGIL,
            };
            let bang = paint.span(&paint.theme.prompt.color, icons::bar(sigil));
            let indent = Span::raw(" ".repeat(bang.width()));
            let mut rows = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let (prefix, body) = if i == 0 {
                    (bang.clone(), &line[bangs..])
                } else {
                    (indent.clone(), line)
                };
                rows.push(Self::said(
                    Line::from(prefix),
                    Line::from(paint.span(&paint.theme.input, body)),
                    (i == 0).then_some(at),
                    true,
                ));
            }
            return rows;
        }
        let border =
            Line::from(paint.span(&paint.theme.prompt.color, icons::bar(icons::SAID_RULE)));
        text.lines()
            .enumerate()
            .map(|(i, line)| {
                Self::said(
                    border.clone(),
                    Line::from(paint.span(&paint.theme.input, line)),
                    (i == 0).then_some(at),
                    false,
                )
            })
            .collect()
    }

    /// How many logical lines the row is made of — the `i`s `line` answers.
    /// Wraps not counted; `line_height` gives a line's screen rows.
    pub fn len(&self) -> usize {
        self.1
            .layout
            .borrow()
            .as_deref()
            .map_or_else(|| self.full_len(), |p| p.lines.len())
    }

    fn full_len(&self) -> usize {
        match &self.0 {
            Kind::Answer(..)
            | Kind::CodeLine(_)
            | Kind::Code(_)
            | Kind::Block(_)
            | Kind::Notice { .. }
            | Kind::AttachedSkills { .. }
            | Kind::Said { .. } => 1,
            Kind::Banner(banner) => banner.len(),
            Kind::Lines(lines) => lines.len(),
            Kind::Call(call) => call.len(self.budget()),
            Kind::Relayed(r) => r.len(),
            Kind::Steps(g) => g.len(),
        }
    }

    /// The rows a fold is given: what a call lays its own rows out in, and
    /// what any other open fold is cut to.
    fn budget(&self) -> usize {
        self.1.budget.get()
    }

    /// Give each fold the height its frame allows: a budget for an open
    /// fold, a window for a group.
    pub(super) fn prepare_layout(&mut self, paint: &Paint, width: usize, height: usize) {
        let budget = fold_budget(height);
        let moved = self.budget() != budget;
        self.1.budget.set(budget);
        // A call lays its own rows out inside the budget: a count row, then
        // the page. There is no slice for the fold to cut, and nothing in it
        // moves but the budget — its paint and its measured rows stay for the
        // next frame, which is what kept redrawing a call's whole text.
        if self.fold().is_some_and(Fold::lays_out) {
            if moved {
                self.1.clear();
            }
            return;
        }
        // A group is a window over its steps: the frame says how tall it
        // may be, and the window keeps the rest for the wheel.
        if let Kind::Steps(group) = &mut self.0 {
            if group.fit(height) {
                self.1.clear();
            }
            return;
        }
        if let Kind::Banner(banner) = &self.0 {
            if banner.prepare(height) {
                self.1.clear();
            }
            return;
        }
        if self
            .1
            .layout
            .borrow()
            .as_deref()
            .is_some_and(|p| p.width == width && p.budget == budget)
        {
            return;
        }
        if !self.fold().is_some_and(Fold::is_open) {
            return;
        }
        self.1.clear();
        let full = self.full_len();
        let mut heights = Vec::with_capacity(full);
        for i in 0..full {
            let (line, border) = self.full_line(i, paint, width);
            heights.push(screen::wrap_rows(border.as_ref(), &line, width));
        }
        let lines = (0..heights.len()).map(LayoutLine::content).collect();
        *self.1.layout.borrow_mut() = Some(Box::new(Layout {
            width,
            budget,
            lines: within_budget(lines, &heights, budget),
        }));
    }

    /// Screen rows logical line `i` takes at `width`, cached by width — a
    /// line never shown is counted without being wrapped.
    pub fn line_height(&self, i: usize, paint: &Paint, width: usize) -> usize {
        if let Some(h) = self.1.line(width, i) {
            return h;
        }
        let (line, border) = self.line(i, paint, width);
        let h = super::screen::wrap_rows(border.as_ref(), &line, width);
        self.1.set(width, i, h);
        h
    }

    /// What the screen renders for row `i` at this width: text, plus an
    /// optional prefix. `continuation` supplies the prefix for wrapped rows.
    pub fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        match self.layout_line(i) {
            Some(LayoutLine::More { heading, remaining }) => {
                let border = self
                    .fold()
                    .and_then(|fold| fold.more_border(heading, paint, width))
                    .filter(|border| border.width() < width);
                let room = width - border.as_ref().map_or(0, Line::width);
                let line = Line::from(paint.action_hint(self.lit(i), format!("{remaining} more")));
                (screen::clip_line(&line, room), border)
            }
            Some(LayoutLine::Content {
                source,
                rows: Some(rows),
            }) => {
                let (line, border) = self.full_line(source, paint, width);
                let room = width
                    .checked_sub(border.as_ref().map_or(0, Line::width))
                    .filter(|room| *room > 0)
                    .unwrap_or(width);
                let spans = screen::fit(&line, room)
                    .into_iter()
                    .take(rows)
                    .flat_map(|line| line.spans)
                    .collect::<Vec<_>>();
                (Line::from(spans), border)
            }
            Some(LayoutLine::Content { source, rows: None }) => {
                self.full_line(source, paint, width)
            }
            None => self.full_line(i, paint, width),
        }
    }

    fn full_line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let (line, border) = match &self.0 {
            Kind::Said { border, body, .. } => (body.clone(), Some(border.clone())),
            Kind::Answer(text, _) => (text.clone(), None),
            Kind::Banner(banner) => (banner.line(i, paint, width), None),
            Kind::Lines(lines) => lines[i].clone(),
            Kind::AttachedSkills { line, .. } => (line.clone(), None),
            Kind::CodeLine(line) => (line.clone(), Some(Line::from(CODE_INDENT))),
            Kind::Code(c) => (c.line(paint), None),
            Kind::Block(b) => (b.line(paint, width), None),
            Kind::Notice { text, times, .. } if *times == 1 => (text.clone(), None),
            Kind::Notice { text, times, .. } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning.
                let mut spans = super::screen::spans_of(text);
                spans.push(paint.span(
                    &paint.theme.muted,
                    format!(" {}", pi_store::text::times(*times)),
                ));
                (Line::from(spans), None)
            }
            // Only the line a click answers lights up: what it opens does not.
            Kind::Call(call) => call.line(i, self.budget(), self.hover(), paint, width),
            Kind::Relayed(r) => r.line(i, paint, width),
            Kind::Steps(g) => g.line(i, paint, width),
        };
        let (line, border) = match &self.2 {
            None => (line, border),
            Some(prefix) => {
                let line = prefix.fit_body(line, width);
                let mut first = prefix.first.clone();
                if let Some(border) = border {
                    first.spans.extend(border.spans);
                }
                (line, Some(first))
            }
        };
        (line, border)
    }

    pub fn continuation(&self, i: usize, paint: &Paint, width: usize) -> Option<Line<'static>> {
        if !self.has_more(i, width)
            && let Some(rest) = self
                .fold()
                .and_then(|fold| fold.continuation(self.source_line(i), paint, width))
        {
            return Some(rest);
        }
        if let Some(prefix) = &self.2 {
            let mut rest = prefix.rest.clone();
            if matches!(self.0, Kind::CodeLine(_)) {
                rest.spans.push(Span::raw(CODE_INDENT));
            }
            return Some(rest);
        }
        self.line(i, paint, width).1.map(|first| {
            if matches!(self.0, Kind::Said { hangs: false, .. }) {
                first
            } else {
                render::Prefix::hanging(first).rest
            }
        })
    }

    /// When the prompt this line opens was sent; only a prompt's first line
    /// has one.
    pub fn sent(&self) -> Option<u64> {
        match &self.0 {
            Kind::Said { at, .. } => *at,
            _ => None,
        }
    }

    /// What in line `i` reads as one — a stretch of code, a link, a table's
    /// cell — as cell ranges of the line `line` gives, its border not counted.
    pub fn units(&self, i: usize, paint: &Paint, width: usize) -> crate::unit::Units {
        if self.has_more(i, width) {
            return Vec::new();
        }
        let i = self.source_line(i);
        match &self.0 {
            Kind::Answer(_, units) => units.clone(),
            Kind::Relayed(r) => r.units(i),
            Kind::Steps(g) => g.units(i, paint),
            Kind::Block(b) => b.units(paint, width),
            _ => Vec::new(),
        }
    }

    pub(super) fn cell(
        &self,
        paint: &Paint,
        width: usize,
        col: usize,
    ) -> Option<(usize, std::sync::Arc<str>, crate::unit::Units)> {
        match &self.0 {
            Kind::Block(block) => block.cell(paint, width, col),
            _ => None,
        }
    }

    pub(super) fn cell_ranges(&self, paint: &Paint, width: usize, id: usize) -> crate::unit::Units {
        match &self.0 {
            Kind::Block(block) => block.cell_ranges(paint, width, id),
            _ => Vec::new(),
        }
    }

    /// The panel this row's screen rows sit in, if it sits in one: the colour
    /// goes behind the row whole, past the text's own end.
    pub fn band(&self, paint: &Paint) -> Option<RStyle> {
        match &self.0 {
            Kind::Said { .. } => paint.band(&paint.theme.prompt.panel.said),
            _ => None,
        }
    }

    pub(super) fn section(&self) -> super::spacing::Section {
        use super::spacing::Section;

        match &self.0 {
            Kind::Said { at: Some(_), .. } => Section::Prompt,
            Kind::Said { at: None, .. } => Section::PromptContinuation,
            Kind::Relayed(_) => Section::Relayed,
            Kind::Call(_) => Section::Tools,
            Kind::Steps(group) if group.has_tools() => Section::Tools,
            Kind::Answer(line, _) if !line.spans.is_empty() || self.2.is_some() => Section::Prose,
            Kind::CodeLine(_) | Kind::Code(_) | Kind::Block(_) => Section::Prose,
            _ => Section::Other,
        }
    }

    /// Whether this row is the group holding reasoning block `id`.
    pub fn holds_block(&self, id: u64) -> bool {
        matches!(&self.0, Kind::Steps(g) if g.holds_block(id))
    }

    /// Whether this row is folded, and the handle to change it.
    pub fn folded(&self) -> Option<bool> {
        match &self.0 {
            Kind::Steps(g) => Some(g.folded()),
            _ => None,
        }
    }

    pub fn set_folded(&mut self, to: bool) {
        if let Kind::Steps(g) = &mut self.0 {
            g.set_folded(to);
            self.1.clear();
        }
    }

    /// Block `id` ended: folds to its first line if the group is folded,
    /// stays open if not; goes if it never had a line.
    pub fn end_block(&mut self, id: u64, paint: &Paint) -> bool {
        let Kind::Steps(g) = &mut self.0 else {
            return false;
        };
        let emptied = g.end_block(id, paint);
        self.1.clear();
        emptied
    }

    /// Append a finished line to reasoning block `id`. A no-op anywhere else,
    /// which no caller can reach: the row is found by `holds_block`.
    pub fn push_line(&mut self, id: u64, line: Line<'static>) {
        if let Kind::Steps(g) = &mut self.0 {
            g.push_line(id, line);
            self.1.clear();
        }
    }

    /// A width-aware startup signature, followed by notices from resolution.
    /// Configuration details belong in the explicit listings.
    pub fn banner(resolved: &Resolved, paint: &Paint) -> Self {
        Self::new(Kind::Banner(Banner::new(&resolved.notes, paint)))
    }
}

/// What sets a code block's lines off from the prose around them: a
/// border, so a wrapped line keeps it on every row.
pub const CODE_INDENT: &str = " ";

pub(super) use llm::figures::count;

// `text` cut to one row beside a `lead` wide mark and the `tail` after it.
fn clip_beside(text: &str, lead: usize, tail: &str, width: usize) -> String {
    let room = render::title_room(width, lead, &format!("{tail} "));
    pi_store::text::clip(text, room.max(8))
}

// A badge copies its block, as an answer's does; a block's line is
// indented as an answer's is, standing on its own.
impl From<crate::render::Rendered> for PageLine {
    fn from(r: crate::render::Rendered) -> Self {
        let mut page = Self {
            line: r.line,
            units: r.units,
            prefix: r.prefix.map(Box::new),
            copies: None,
        };
        match r.coded {
            render::Coded::Badge(code) => page.copies = Some(code),
            render::Coded::Code => page.prepend(render::Prefix::hanging(Line::from(CODE_INDENT))),
            render::Coded::Text => {}
        }
        page
    }
}

#[cfg(test)]
mod height_tests {
    use super::*;

    // The window counts a line by its height, wraps included, without
    // building the rows it skips.
    #[test]
    fn a_lines_height_counts_its_wraps() {
        let paint = Paint::new(false);
        let row = Row::lines(vec![Line::from("abcdef"), Line::from("gh")]);
        assert_eq!(row.line_height(0, &paint, 2), 3, "abcdef wraps to three");
        assert_eq!(row.line_height(1, &paint, 2), 1);
        assert_eq!(row.line_height(0, &paint, 80), 1);
    }
}
