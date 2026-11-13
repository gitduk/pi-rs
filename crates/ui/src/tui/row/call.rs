//! A tool call as the screen draws it: its own line, and the rows it holds
//! under it. A call inside a run's group and a call standing alone are this
//! same thing — only where it sits, and so how many rows it may take, differs.

use std::ops::Range;
use std::time::Duration;

use ratatui::text::Line;

use std::cell::RefCell;

use super::steps::{STEP_INDENT, StepMark, desc_of, text_room};
use super::{Fold, Opening, page_of};
use crate::render::{self, Outcome, Paint};
use crate::tui::browse::PageLine;
use pi_store::text;

/// One row under a call's line: what it asked for, or what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    /// A line of what the call asked for — the command, or another argument.
    Asked(String),
    /// The blank row parting one block from the next.
    Gap,
    /// What a command printed, before what it changed.
    Printed(String),
    /// What came back, as the tool wrote it.
    Said(String),
    /// What came back, as a change: the rows a tool sketched for the screen.
    Change(String),
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
    // Which of them a row shows: the argument's first line, then what the
    // call found. What a command printed is the page's, and so is the rest
    // of its argument.
    row: Vec<usize>,
    // Where the call ran, when not the workspace root.
    place: Option<String>,
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
            && self.row == other.row
            && self.place == other.place
            && self.took == other.took
            && self.open == other.open
    }
}

impl Eq for Call {}

// One block of a call's body, after the blank row that parts it from the block
// before it. Answers where it landed.
fn block(body: &mut Vec<Part>, rows: &[String], part: fn(String) -> Part) -> Range<usize> {
    if rows.is_empty() {
        return 0..0;
    }
    body.push(Part::Gap);
    let at = body.len()..body.len() + rows.len();
    body.extend(rows.iter().cloned().map(part));
    at
}

impl Call {
    /// A call as its result leaves it: `preview` is what the tool said of
    /// itself — its first line the call's line, any rows under it what it
    /// drew for the screen — `asked` the argument it was made with, and
    /// `output` what it printed, once that is known.
    pub fn new(
        outcome: Outcome,
        name: &str,
        preview: &str,
        asked: &str,
        output: Option<&str>,
    ) -> Self {
        // A line naming its tool alone says nothing of what was asked: the
        // argument tells it, when that reads as one.
        let recovered;
        let preview = if preview.trim() == name {
            recovered = serde_json::from_str(asked)
                .map(|args| tool::summary(&args).into_owned())
                .unwrap_or_else(|_| asked.to_string());
            recovered.as_str()
        } else {
            preview
        };
        let head = preview.lines().next().unwrap_or_default().trim();
        // Keep an argument line unless the line already reads it.
        let first = asked.lines().next().unwrap_or_default().trim();
        let read = !first.is_empty() && !head.is_empty() && head.contains(first);
        let asked: Vec<String> = asked
            .lines()
            .skip(usize::from(read))
            .map(str::to_string)
            .collect();
        // What the tool sketched is what a screen shows; the output is the
        // fallback for a tool that sketched nothing. A command that changed
        // files says three things — what it ran, what it printed, what it
        // changed — and a blank row parts each from the one before it.
        let mut printed: Vec<String> = Vec::new();
        let mut change: Vec<String> = Vec::new();
        for line in preview.lines().skip(1) {
            match tool::row::kind(line) {
                tool::row::Kind::Output(_) => printed.push(line.to_string()),
                _ => change.push(line.to_string()),
            }
        }
        // Nothing sketched: what it said is the body.
        let said: Vec<String> = if printed.is_empty() && change.is_empty() {
            output
                .map(|out| {
                    pi_core::core::tools::shown(name, out)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // The page reads them all: the argument at its line's foot, then a
        // blank, then each block after it in turn.
        let mut body: Vec<Part> = asked.iter().cloned().map(Part::Asked).collect();
        let printed_at = block(&mut body, &printed, Part::Printed);
        // A command that changed more than one file parts them: a blank row
        // between one file's rows and the next file's head, so the rows under
        // a name stay its own.
        let mut change_at = 0..0;
        if !change.is_empty() {
            body.push(Part::Gap);
            change_at.start = body.len();
            for line in &change {
                let names = matches!(tool::row::kind(line), tool::row::Kind::File { .. });
                if names && !matches!(body.last(), Some(Part::Gap)) {
                    body.push(Part::Gap);
                }
                body.push(Part::Change(line.clone()));
            }
            change_at.end = body.len();
        }
        let said_at = block(&mut body, &said, Part::Said);
        // The row keeps the argument's first line and what the call found: a
        // command that changed files is read for its change, not for what it
        // printed on the way, and the rest of a script is the page's.
        let shown = [&change_at, &said_at, &printed_at]
            .into_iter()
            .find(|at| !at.is_empty())
            .unwrap_or(&printed_at);
        let mut row: Vec<usize> = (0..asked.len().min(1)).collect();
        if !shown.is_empty() {
            row.push(shown.start - 1);
            row.extend(shown.clone());
        }
        Self {
            outcome,
            name: name.to_string(),
            text: desc_of(name, preview),
            body,
            row,
            place: None,
            took: None,
            // A change is what a call shows unasked; anything else waits for
            // a click, or for the group it folds into to open with it.
            open: !change_at.is_empty(),
            painted: RefCell::new(None),
        }
    }

    /// Where the call ran, when not the workspace root.
    pub fn at(mut self, place: Option<String>) -> Self {
        self.place = place;
        self.painted = RefCell::new(None);
        self
    }

    /// How long the call ran, if known.
    pub fn timed(mut self, took: Option<Duration>) -> Self {
        self.took = took;
        self.painted = RefCell::new(None);
        self
    }

    /// The calls a run's group holds: one that landed well. A call with
    /// nothing under its line is one of them, and only stops being clickable.
    pub fn holds(&self) -> bool {
        matches!(self.outcome, Outcome::Done | Outcome::Warned)
    }

    /// Whether it has rows under its line, whoever landed it: a failed call
    /// keeps its own line and opens the same way.
    pub fn openable(&self) -> bool {
        !self.body.is_empty()
    }

    /// Whether what it holds is a change to files rather than output: the
    /// rows a tool sketched, which want a line of their own.
    pub fn sketched(&self) -> bool {
        self.body.iter().any(|part| matches!(part, Part::Change(_)))
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
    pub fn row_len(&self) -> usize {
        self.rows_at(usize::MAX)
    }

    // The row's view of its body, `i` rows in.
    fn glimpse(&self, i: usize) -> Option<&Part> {
        self.row.get(i).and_then(|&at| self.body.get(at))
    }

    /// The rows it takes, `rows` being what its place allows: its own line,
    /// then as much of its body as fits, and the count row when more is left.
    /// A place that bounds nothing says so with `usize::MAX`.
    pub fn len(&self, rows: usize) -> usize {
        match self.open {
            true => self.rows_at(rows),
            false => 1,
        }
    }

    // The rows it takes open in a place that allows `rows`.
    fn rows_at(&self, rows: usize) -> usize {
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
        let room = rows.saturating_sub(1);
        if room == 0 {
            return (0, 0);
        }
        let short = self.row.len() < self.body.len();
        let body = self.row.len().min(room.saturating_sub(usize::from(short)));
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

    /// The call's line: its mark, its name and what it asked for.
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
                let lines = (0..self.rows_at(rows))
                    .map(|j| self.lay(j, rows, hover == Some(j), paint, width))
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
    ) -> (Line<'static>, Option<Line<'static>>) {
        if j == 0 {
            return self.head(hovered, paint, width);
        }
        let border = Line::from(STEP_INDENT);
        let room = render::result_room(width);
        let (body, left) = self.shown(rows);
        let line = match self.glimpse(j - 1).filter(|_| j <= body) {
            None => match left > 0 && j == 1 + body {
                true => Line::from(paint.action_hint(hovered, format!("{left} more"))),
                false => Line::default(),
            },
            Some(part) => self.part(part, paint, room),
        };
        (line, Some(border))
    }

    /// One row of what the call holds, in the voice its kind asks for.
    fn part(&self, part: &Part, paint: &Paint, room: usize) -> Line<'static> {
        match part {
            Part::Asked(text) => Line::from(paint.call_line(&self.name, text)),
            Part::Gap => Line::default(),
            // What it printed reads as it came, in the muted voice the change
            // rows keep their headers in.
            Part::Printed(text) => Line::from(paint.span(&paint.theme.muted, text.clone())),
            Part::Said(text) => Line::from(text.clone()),
            Part::Change(text) => render::change(text, paint, room),
        }
    }

    // Where it ran and how long for, kept whole when the text is cut.
    fn tail(&self) -> String {
        render::placed(self.place.as_deref()) + &render::ran_for(self.took)
    }
}

#[cfg(test)]
// A call that landed well, as the screen holds one.
pub(crate) fn test_call(name: &str, preview: &str, asked: &str, output: &str) -> Call {
    Call::new(Outcome::Done, name, preview, asked, Some(output))
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

    // It lays its own rows out in the budget: no slice is measured for it.
    fn lays_out(&self) -> bool {
        true
    }

    fn opening(&self, _i: usize, paint: &Paint) -> Option<Opening> {
        Some(self.page(paint))
    }
}

impl Call {
    /// The page that reads it whole: its own line, then every row it holds.
    pub fn page(&self, paint: &Paint) -> Opening {
        page_of(
            (0..1 + self.body.len())
                .map(|j| {
                    let (line, border) = self.whole(j, paint);
                    (PageLine::from(line), border)
                })
                .collect(),
        )
    }

    // Row `j` as the page reads it: whole, however many rows there are, and
    // the argument with it.
    fn whole(&self, j: usize, paint: &Paint) -> (Line<'static>, Option<Line<'static>>) {
        if j == 0 {
            return self.head(false, paint, usize::MAX);
        }
        let border = Line::from(STEP_INDENT);
        let Some(part) = self.body.get(j - 1) else {
            return (Line::default(), Some(border));
        };
        (self.part(part, paint, usize::MAX), Some(border))
    }
}
