//! A tool call as the screen draws it: its own line, and the rows it holds
//! under it. A call inside a run's group and a call standing alone are this
//! same thing — only where it sits, and so how many rows it may take, differs.

use std::ops::Range;
use std::time::Duration;

use ratatui::text::Line;

use std::cell::RefCell;

use super::steps::{STEP_INDENT, StepMark, text_room};
use super::{Fold, Opening, page_of};
use crate::render::{self, Outcome, Paint};
use crate::tui::browse::PageLine;
use pi_core::core::memory::Remember;
use pi_store::text;

/// One row under a call's line: what it was made with, or what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// A line of the argument the call was made with — a command, a path.
    Argument(String),
    /// The blank row parting one block from the next.
    Gap,
    /// A line the tool sketched as output: what the command printed, before
    /// what it changed.
    Output(String),
    /// A line of the result as the tool wrote it, when it sketched nothing.
    Result(String),
    /// What came back, as a change: the rows a tool sketched for the screen.
    Change(String),
    /// A memory change or its accompanying receipt text, wrapped as prose.
    Receipt(String),
}

/// The rows a row draws under a call's line: the one block it keeps of the
/// page, and the argument's leading line when the line above does not read it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Glimpse {
    /// Whether the body's leading row, the argument's line, is the row's too.
    lead: bool,
    /// The body rows of the block the row draws, its parting blank row first.
    block: Range<usize>,
}

impl Glimpse {
    // How many rows the row draws under its line.
    fn len(&self) -> usize {
        usize::from(self.lead) + self.block.len()
    }

    // The body rows the row draws under its line, in order.
    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.lead.then_some(0).into_iter().chain(self.block.clone())
    }
}

/// A tool call, however it came out.
#[derive(Debug, Clone)]
pub struct Call {
    pub outcome: Outcome,
    pub name: String,
    // The line as it reads after the mark: the name and the call's own first
    // word, composed once so a clip and a click weigh the same text.
    text: String,
    // Every row it holds, in the order the page reads them.
    body: Vec<Part>,
    // What a row shows of them. What a command printed is the page's, and so
    // is the rest of its argument.
    glimpse: Glimpse,
    // Where the call ran, when not the workspace root.
    cwd: Option<String>,
    // The file it works on, when it names one: what a right click opens.
    file: Option<std::path::PathBuf>,
    pub took: Option<Duration>,
    // Open where it stands, or closed to its own line. Its place sets this
    // when it lands; from then it is the reader's.
    open: bool,
    // Its rows as the last frame laid them out: painting the body per row
    // made every redraw cost the call's whole text.
    painted: RefCell<Option<Painted>>,
}

// A call's rows for one place: another width, budget, open state or line
// under the pointer is another picture.
#[derive(Debug, Clone)]
struct Painted {
    width: usize,
    rows: usize,
    open: bool,
    hover: Option<usize>,
    lines: Vec<(Line<'static>, Option<Line<'static>>)>,
}

impl PartialEq for Call {
    fn eq(&self, other: &Self) -> bool {
        self.outcome == other.outcome
            && self.name == other.name
            && self.text == other.text
            && self.body == other.body
            && self.glimpse == other.glimpse
            && self.cwd == other.cwd
            && self.file == other.file
            && self.took == other.took
            && self.open == other.open
    }
}

impl Eq for Call {}

// One block of a call's body, after the blank row that parts it from the block
// before it. Answers the rows it holds, that blank row first.
fn block(
    body: &mut Vec<Part>,
    rows: &[&str],
    part: fn(String) -> Part,
    heads: fn(&str) -> bool,
) -> Range<usize> {
    if rows.is_empty() {
        return 0..0;
    }
    let at = body.len();
    body.push(Part::Gap);
    for &line in rows {
        if heads(line) && !matches!(body.last(), Some(Part::Gap)) {
            body.push(Part::Gap);
        }
        body.push(part(line.to_owned()));
    }
    at..body.len()
}

impl Call {
    /// A call as its result leaves it: `screen` is what the tool said of
    /// itself — its first line the call's line, any rows under it what it
    /// drew for the screen — `argument` the text it was made with, and
    /// `result` what the call answered, once that is known.
    pub fn new(
        outcome: Outcome,
        name: &str,
        screen: &str,
        argument: &str,
        result: Option<&str>,
    ) -> Self {
        let receipt = name == Remember::NAME;
        // A line naming its tool alone says nothing of what it was made with:
        // the argument tells it, when that reads as one.
        let recovered;
        let screen = if !receipt && screen.trim() == name {
            recovered = serde_json::from_str(argument)
                .map(|args| tool::summary(&args).into_owned())
                .unwrap_or_else(|_| argument.to_string());
            recovered.as_str()
        } else {
            screen
        };
        let mut rows = screen.lines();
        let head = rows.next().unwrap_or_default().trim();
        // The page keeps what the call's own line does not already read: a
        // command is that line, and what it carries below is not.
        let first = argument.lines().next().unwrap_or_default().trim();
        let head_reads = !first.is_empty() && !head.is_empty() && head.contains(first);
        let argument: Vec<_> = argument.lines().skip(usize::from(head_reads)).collect();
        // What the tool sketched is what a screen shows; the result is the
        // fallback for a tool that sketched nothing. A command that changed
        // files says three things — what it ran, what it printed, what it
        // changed — and a blank row parts each from the one before it.
        let (output, change): (Vec<_>, Vec<_>) = rows.partition(|line| {
            !receipt && matches!(tool::row::kind(line), tool::row::Kind::Output(_))
        });
        let result: Vec<_> = result
            .filter(|_| output.is_empty() && change.is_empty())
            .into_iter()
            .flat_map(|out| pi_core::core::tools::shown(name, out))
            .collect();
        let mut body = Vec::new();
        if !receipt {
            body.extend(
                argument
                    .iter()
                    .map(|line| Part::Argument((*line).to_owned())),
            );
        }
        let output_at = block(&mut body, &output, Part::Output, |_| false);
        // A command that changed several files parts them: a blank row before
        // each file's head, so the rows under a name stay its own.
        let change_at = if receipt {
            body.extend(
                change
                    .into_iter()
                    .map(|line| Part::Receipt(line.to_owned())),
            );
            0..body.len()
        } else {
            block(&mut body, &change, Part::Change, |line| {
                matches!(tool::row::kind(line), tool::row::Kind::File { .. })
            })
        };
        let result_at = block(&mut body, &result, Part::Result, |_| false);
        if receipt {
            block(&mut body, &argument, Part::Argument, |_| false);
        }
        // The row keeps what the call found — a change before what it printed —
        // and the argument's leading line, unless the call's line reads it.
        let shown = [&change_at, &result_at, &output_at]
            .into_iter()
            .find(|at| !at.is_empty())
            .cloned()
            .unwrap_or_default();
        let lead = !receipt && !head_reads && !argument.is_empty();
        Self {
            outcome,
            name: name.to_string(),
            text: tool::label(name, head),
            body,
            glimpse: Glimpse { lead, block: shown },
            cwd: None,
            file: None,
            took: None,
            // A change is what a call shows unasked; anything else waits for
            // a click, or for the group it folds into to open with it.
            open: !change_at.is_empty() || (receipt && !result_at.is_empty()),
            painted: RefCell::new(None),
        }
    }

    /// Where the call ran, when not the workspace root.
    pub fn in_cwd(mut self, cwd: Option<String>) -> Self {
        self.cwd = cwd;
        self.painted = RefCell::new(None);
        self
    }

    /// How long the call ran, if known.
    pub fn timed(mut self, took: Option<Duration>) -> Self {
        self.took = took;
        self.painted = RefCell::new(None);
        self
    }

    /// The file the call works on, when it names one.
    pub fn on_file(mut self, file: Option<std::path::PathBuf>) -> Self {
        self.file = file;
        self
    }

    /// The file the call works on, when it names one: what a right click opens.
    pub fn file(&self) -> Option<&std::path::Path> {
        self.file.as_deref()
    }

    /// Whether it landed well: a failed call is read on its own line.
    pub fn landed_well(&self) -> bool {
        matches!(self.outcome, Outcome::Done | Outcome::Warned)
    }

    /// Whether it has rows under its line, whoever landed it: a failed call
    /// keeps its own line and opens the same way.
    pub fn openable(&self) -> bool {
        !self.body.is_empty()
    }

    /// Whether what it holds is a change to files rather than output: the
    /// rows a tool drew, which want a line of their own.
    pub fn changed(&self) -> bool {
        self.body
            .iter()
            .any(|part| matches!(part, Part::Change(_) | Part::Receipt(_)))
    }

    /// Whether it stands open.
    pub fn open(&self) -> bool {
        self.open
    }

    /// Open where it has rows to show, or closed to its own line.
    pub fn open_to(&mut self, open: bool) {
        self.open = open && self.openable();
    }

    /// The reader opened it, or closed it: the opposite of where it stands.
    pub fn toggle(&mut self) {
        self.open_to(!self.open);
    }

    /// The rows it would take open where nothing bounds it: what a caller
    /// weighs it by before it is opened.
    pub fn height(&self) -> usize {
        self.height_in(usize::MAX)
    }

    /// The rows it takes, `rows` being what its place allows: its own line,
    /// then as much of its body as fits, and the count row when more is left.
    /// A place that bounds nothing says so with `usize::MAX`.
    pub fn len(&self, rows: usize) -> usize {
        match self.open {
            true => self.height_in(rows),
            false => 1,
        }
    }

    // The rows it takes open in a place that allows `rows`.
    fn height_in(&self, rows: usize) -> usize {
        let (body, left) = self.shown(rows);
        1 + body + usize::from(left > 0)
    }

    /// How much of the body fits under the line, and how much is left: the
    /// count row takes its row from what the line leaves, so a glimpse that
    /// fills the place leaves it none and gives up a row to it. A place that
    /// bounds nothing (`usize::MAX`) has room for both.
    fn shown(&self, rows: usize) -> (usize, usize) {
        // What a row leaves out of its view is the page's as much as what it
        // has no room for: the count says how many rows it holds unseen.
        let room = if self.lays_out() {
            rows.saturating_sub(1)
        } else {
            // The transcript budgets the receipt after its lines wrap.
            usize::MAX
        };
        if room == 0 {
            return (0, 0);
        }
        let drawn = self.glimpse.len();
        let short = drawn < self.body.len();
        let body = drawn.min(room.saturating_sub(usize::from(short)));
        (body, self.body.len().saturating_sub(body))
    }

    /// Whether line `j` is the count that reads the rest on a page.
    pub fn page_row(&self, j: usize, rows: usize) -> bool {
        if !self.open || j == 0 {
            return false;
        }
        let (body, left) = self.shown(rows);
        left > 0 && j == 1 + body
    }

    /// Whether line `j` answers a click: its own line opens or closes it, the
    /// count under it reads the rest on a page.
    pub fn opens_at(&self, j: usize, rows: usize, width: usize) -> bool {
        self.click_line(j, width).is_some() || self.page_row(j, rows)
    }

    /// Whether its own line says less than it holds: more of it than the
    /// width shows.
    pub fn cut(&self, width: usize) -> bool {
        text::width(&self.text) > text_room(width, &self.tail())
    }

    /// The call's line: its mark, its name and what it was made with.
    pub fn head(
        &self,
        hovered: bool,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let border = Line::from(StepMark::Ended(self.outcome).spans(hovered, paint));
        let tail = self.tail();
        // Opened, the line reads whole and what the tail said is below it.
        let cut;
        let text = match self.open {
            true => self.text.as_str(),
            false => {
                cut = text::clip(&self.text, text_room(width, &tail));
                &cut
            }
        };
        let mut spans = paint.tool_call(&self.name, text, hovered);
        if !self.open {
            spans.push(paint.span_hovered(hovered, &paint.theme.muted, tail));
        }
        (Line::from(spans), Some(border))
    }

    /// Line `j` of the call, `rows` being what its place allows: its own line
    /// at 0, what it holds under it, and the count last. `hover` is the line
    /// under the pointer, in the call's own rows.
    pub fn line(
        &self,
        j: usize,
        rows: usize,
        hover: Option<usize>,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        let mut painted = self.painted.borrow_mut();
        let lines = match &mut *painted {
            Some(had)
                if had.width == width
                    && had.rows == rows
                    && had.open == self.open
                    && had.hover == hover =>
            {
                &mut had.lines
            }
            slot => {
                let receipt = self.paint_receipt(paint);
                let lines = (0..self.height_in(rows))
                    .map(|j| self.lay(j, rows, hover == Some(j), paint, width, &receipt))
                    .collect();
                &mut slot
                    .insert(Painted {
                        width,
                        rows,
                        open: self.open,
                        hover,
                        lines,
                    })
                    .lines
            }
        };
        lines.get(j).cloned().unwrap_or_default()
    }

    // One row of the layout, by index: the count of what the place holds.
    fn lay(
        &self,
        j: usize,
        rows: usize,
        hovered: bool,
        paint: &Paint,
        width: usize,
        receipt: &[Line<'static>],
    ) -> (Line<'static>, Option<Line<'static>>) {
        if j == 0 {
            return self.head(hovered, paint, width);
        }
        let border = Line::from(STEP_INDENT);
        let room = render::result_room(width);
        let (body, left) = self.shown(rows);
        let line = match self.glimpse.rows().nth(j - 1).filter(|_| j <= body) {
            None if left > 0 && j == 1 + body => {
                let hint = if self.name == Remember::NAME {
                    "details".to_string()
                } else {
                    format!("{left} more")
                };
                Line::from(paint.action_hint(hovered, hint))
            }
            None => Line::default(),
            Some(at) => self.part(at, paint, room, receipt),
        };
        (line, Some(border))
    }

    // Paint the receipt together so a replacement can highlight its changed text.
    // Receipt parts are at the start of the body, in the same order.
    fn paint_receipt(&self, paint: &Paint) -> Vec<Line<'static>> {
        let rows: Vec<_> = self
            .body
            .iter()
            .map_while(|part| match part {
                Part::Receipt(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        render::receipt_changes(&rows, paint)
    }

    /// One row of what the call holds, in the voice its kind asks for.
    fn part(
        &self,
        at: usize,
        paint: &Paint,
        room: usize,
        receipt: &[Line<'static>],
    ) -> Line<'static> {
        match &self.body[at] {
            Part::Argument(text) => Line::from(paint.call_line(&self.name, text)),
            Part::Gap => Line::default(),
            // What it printed reads as it came, in the muted voice the change
            // rows keep their headers in.
            Part::Output(text) => Line::from(paint.span(&paint.theme.muted, text.clone())),
            Part::Result(text) => Line::from(text.clone()),
            Part::Change(text) => render::change(text, paint, room),
            Part::Receipt(_) => receipt[at].clone(),
        }
    }

    // Where it ran and how long for, kept whole when the text is cut.
    fn tail(&self) -> String {
        render::tail(self.cwd.as_deref(), &render::ran_for(self.took))
    }
}

#[cfg(test)]
// A call that landed well, as the screen holds one.
pub(crate) fn test_call(name: &str, screen: &str, argument: &str, answer: &str) -> Call {
    Call::new(Outcome::Done, name, screen, argument, Some(answer))
}

impl Fold for Call {
    fn is_open(&self) -> bool {
        self.open
    }

    fn flip(&mut self) {
        self.toggle();
    }

    fn expandable(&self) -> bool {
        self.openable()
    }

    // Its own line opens or closes it, whatever the rows it was given.
    fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        (i == 0 && (self.open || self.expandable() || self.cut(width))).then_some(0)
    }

    // The count under it reads the rest on a page, so a click on any line it
    // answers is answered by that line.
    fn click_at(&self, i: usize, rows: usize, width: usize) -> Option<usize> {
        self.opens_at(i, rows, width).then_some(i)
    }

    // Only its own line opens or closes it here: the count is the page's.
    fn toggle_at(&mut self, i: usize, width: usize, _screen: usize) -> bool {
        self.click_line(i, width).is_some() && {
            self.flip();
            true
        }
    }

    // A count row, and a line the width cuts, read whole on a page.
    fn paged(&self, i: usize, rows: usize, width: usize) -> bool {
        self.page_row(i, rows) || self.cut(width)
    }

    // Receipts use the transcript's wrapped layout so long notes fit the budget.
    fn lays_out(&self) -> bool {
        self.name != Remember::NAME
    }

    fn opening(&self, _i: usize, paint: &Paint) -> Option<Opening> {
        Some(self.page(paint))
    }
}

impl Call {
    /// The page that reads it whole: its own line, then every row it holds.
    pub fn page(&self, paint: &Paint) -> Opening {
        let receipt = self.paint_receipt(paint);
        let (head, border) = self.head(false, paint, usize::MAX);
        page_of(
            std::iter::once((PageLine::from(head), border))
                .chain((0..self.body.len()).map(|at| {
                    let line = self.part(at, paint, usize::MAX, &receipt);
                    (PageLine::from(line), Some(Line::from(STEP_INDENT)))
                }))
                .collect(),
        )
    }
}
