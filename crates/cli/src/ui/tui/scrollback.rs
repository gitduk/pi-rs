//! The transcript as the scrollback walks it: the rows, the folded blocks
//! among them, and the window they are read through.

use std::collections::HashSet;

use agent::session::Entry as LogEntry;
use llm::message::{AssistantContent, ReasoningContent};
use ratatui::text::Line;

use super::THINKING;
use super::row::Row;
use super::screen;
use super::tool::push_tool_row;
use crate::app;
use crate::store::icons;
use crate::store::status::Segment;
use crate::ui::render::{self, Paint};

// Whether reasoning is folded to its count line, and which block the stream
// is filling right now.
//
// Reasoning always lives in a foldable scrollback entry, folded or not: the
// screen is repainted from its rows every frame, so a line already shown
// can still be folded. A block's own state lasts only while it is last; the
// next block pushes it back to `folded`, the switch.
pub(super) struct Folds {
    // The next block id; closed rows keep the id they were born with, so
    // `land` appends only to the open block's entry.
    pub(super) next: u64,
    // The block streaming right now, if any.
    pub(super) streaming: Option<u64>,
    // What untouched blocks are folded to: the value a block that stops being
    // last folds back to, and the target a global flip is measured from.
    pub(super) folded: bool,
    // How the last block — the one `ctrl+t` names, finished or streaming —
    // is folded. It survives the block itself, so the next block is born
    // with it until the key flips it again.
    pub(super) last: bool,
}

// Shut: the reasoning is worth a glance while it runs and almost never worth
// the scrollback it costs afterwards.
//
// The only constructor, because a derived one would answer `false` here — the
// opposite of what the type says two lines up, in the one place nobody would
// think to look.
impl Default for Folds {
    fn default() -> Self {
        Self {
            next: 1,
            streaming: None,
            folded: true,
            last: true,
        }
    }
}

impl Folds {
    // Whether a reasoning row is hidden behind the count line: dim, and the
    // streaming block folded — its own entry when it has one, the last
    // value it will be born with otherwise.
    pub(super) fn holds(&self, reasoning: bool, scrollback: &[Row]) -> bool {
        reasoning && self.stream_fold(scrollback)
    }

    // How the block streaming now is folded: its entry's own state, or —
    // before its first line lands — the last value.
    pub(super) fn stream_fold(&self, scrollback: &[Row]) -> bool {
        if let Some(id) = self.streaming
            && let Some(folded) = scrollback
                .iter()
                .rev()
                .find(|r| r.block() == Some(id))
                .and_then(Row::folded)
        {
            return folded;
        }
        self.last
    }

    // The block that was last stops being so: it folds back to the switch.
    // A new input and a new block both push it out of last.
    fn retire_last(&mut self, scrollback: &mut [Row]) {
        if let Some(row) = last_folded(scrollback) {
            row.set_folded(self.folded);
        }
    }

    // The next block id. The one place ids come from, so a rebuilt block and
    // a streamed one can never mean the same number.
    pub(super) fn take_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    // A new reasoning block is about to start. It gets an id the scrollback
    // entry will be born with; its first line takes `birth_fold`.
    pub(super) fn start(&mut self, scrollback: &mut [Row]) {
        self.retire_last(scrollback);
        self.streaming = Some(self.take_id());
    }

    // The block that was last stops being so the moment a new input is
    // submitted: it folds to the switch, its unfold lasting only while it
    // was last.
    pub(super) fn fold_previous(&mut self, scrollback: &mut [Row]) {
        self.retire_last(scrollback);
    }

    // The streaming block is over; the entry it filled stays where it is.
    pub(super) fn close_block(&mut self) {
        self.streaming = None;
    }

    // The value the next block's entry is born with: however `ctrl+t` last
    // left the last block.
    pub(super) fn birth_fold(&self) -> bool {
        self.last
    }

    // Fold or unfold every block in the scrollback and move the switch with
    // them, the last block included: rows and switch must never disagree,
    // or the next block is born with a stale value and a mixed screen can
    // never fold back to a single state.
    pub(super) fn flip_all(&mut self, scrollback: &mut [Row]) {
        self.folded = !self.folded;
        self.last = self.folded;
        for row in scrollback.iter_mut() {
            row.set_folded(self.folded);
        }
    }

    // Flip the last block only: the one streaming, or the newest finished
    // one when nothing is. The switch is left alone, so the blocks no one is
    // touching keep what they had; a block with no entry yet is born with
    // the flip.
    pub(super) fn toggle_current(&mut self, scrollback: &mut [Row]) {
        self.last = !self.last;
        let flipped = if let Some(id) = self.streaming {
            scrollback.iter_mut().rev().find(|r| r.block() == Some(id))
        } else {
            last_folded(scrollback)
        };
        if let Some(row) = flipped {
            row.set_folded(self.last);
        }
    }
}

// The newest reasoning block's entry in the scrollback, if any.
fn last_folded(scrollback: &mut [Row]) -> Option<&mut Row> {
    scrollback.iter_mut().rev().find(|r| r.block().is_some())
}

// The scrollback as rows, walked from either end without flattening the
// whole history: `window` only ever needs the newest `want` rows, and an
// unfolded thinking block is not worth re-materializing per frame.
pub(super) struct ScrollbackRows<'a> {
    rows: &'a [Row],
    // The frame's width, for the rows that clip to fit.
    width: usize,
    // For the folded summary row, which is synthesized at draw time and so
    // carries no paint of its own.
    paint: &'a Paint,
    // What a finished run's row spells itself out with, for the same reason:
    // it is rendered here, not when the run ended.
    done: &'a [Segment],
    // Next entry to read from the front, and the row offset inside it.
    front: (usize, usize),
    // Next entry to read from the back, and the row offset inside it.
    back: (usize, usize),
    // Each row's logical line count, the steps the two walks take. Wrapped
    // heights are the rows' own caches, summed per frame in `flush`.
    lens: Vec<usize>,
}

/// One line of the view as the window walks it: a scrollback row's line, which
/// waits for the frame that shows it, or a live line already in hand.
///
/// The split is what a scrolled view costs. The walk passes over every line
/// above where the window starts, and a row's line is asked for only once the
/// window has reached it — the walk itself reads the row's own count.
pub(super) enum Piece<'a> {
    Row {
        row: &'a Row,
        line: usize,
        paint: &'a Paint,
        done: &'a [Segment],
        width: usize,
    },
    Live(screen::Ready<'a>),
}

impl screen::Piece for Piece<'_> {
    fn height(&self) -> usize {
        match self {
            Piece::Row {
                row,
                line,
                paint,
                done,
                width,
            } => row.line_height(*line, paint, done, *width),
            Piece::Live(line) => screen::Piece::height(line),
        }
    }

    fn pieces(self) -> Vec<Line<'static>> {
        match self {
            Piece::Row {
                row,
                line,
                paint,
                done,
                width,
            } => {
                let (line, border) = row.line(line, paint, done, width);
                let pieces = screen::wrap(border.as_ref(), &line, width);
                // After the wrap, so every screen row a said line spans gets
                // the band whole and the padding never wraps a row of its own.
                match row.band() {
                    Some(band) => pieces
                        .into_iter()
                        .map(|piece| screen::banded(piece, band, width))
                        .collect(),
                    None => pieces,
                }
            }
            Piece::Live(line) => screen::Piece::pieces(line),
        }
    }
}

pub(super) struct IndexedScrollbackRows<'a>(ScrollbackRows<'a>);

impl<'a> Iterator for IndexedScrollbackRows<'a> {
    type Item = (Piece<'a>, usize);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_indexed()
    }
}

impl<'a> DoubleEndedIterator for IndexedScrollbackRows<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back_indexed()
    }
}

impl<'a> ScrollbackRows<'a> {
    pub(super) fn new(
        rows: &'a [Row],
        paint: &'a Paint,
        done: &'a [Segment],
        width: usize,
    ) -> Self {
        let back = rows.len().saturating_sub(1);
        let back_row = if rows.is_empty() { 0 } else { rows[back].len() };
        let lens = rows.iter().map(|r| r.len()).collect();
        Self {
            rows,
            width,
            paint,
            done,
            front: (0, 0),
            back: (back, back_row),
            lens,
        }
    }

    pub(super) fn indexed(self) -> IndexedScrollbackRows<'a> {
        IndexedScrollbackRows(self)
    }

    // Line `line` of row `idx` as the window wants it: the row itself, whose
    // count per line is already measured and whose text waits until a frame
    // shows it.
    pub(super) fn piece(&self, idx: usize, line: usize) -> Piece<'a> {
        Piece::Row {
            row: &self.rows[idx],
            line,
            paint: self.paint,
            done: self.done,
            width: self.width,
        }
    }

    fn next_indexed(&mut self) -> Option<(Piece<'a>, usize)> {
        if self.rows.is_empty() {
            return None;
        }
        while self.front.0 <= self.back.0 {
            let idx = self.front.0;
            if self.front.0 == self.back.0 {
                // The two walks have met inside one row: no line left between
                // them for either to take.
                if self.front.1 >= self.back.1 {
                    return None;
                }
                let item = self.piece(idx, self.front.1);
                self.front.1 += 1;
                return Some((item, idx));
            }
            if self.front.1 < self.lens[idx] {
                let item = self.piece(idx, self.front.1);
                self.front.1 += 1;
                return Some((item, idx));
            }
            self.front = (self.front.0 + 1, 0);
        }
        None
    }

    fn next_back_indexed(&mut self) -> Option<(Piece<'a>, usize)> {
        if self.rows.is_empty() {
            return None;
        }
        while self.front.0 <= self.back.0 {
            let idx = self.back.0;
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                self.back.1 -= 1;
                let item = self.piece(idx, self.back.1);
                return Some((item, idx));
            }
            if self.back.1 > 0 {
                self.back.1 -= 1;
                let item = self.piece(idx, self.back.1);
                return Some((item, idx));
            }
            self.back = (self.back.0 - 1, self.lens[self.back.0 - 1]);
        }
        None
    }
}

impl<'a> Iterator for ScrollbackRows<'a> {
    type Item = Piece<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_indexed().map(|(item, _)| item)
    }
}

impl<'a> DoubleEndedIterator for ScrollbackRows<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.next_back_indexed().map(|(item, _)| item)
    }
}

// The rows between the scrollback and the status line: the reasoning window,
// and the paragraph still being written.
//
// A free function because it is where both of this feature's bugs lived and
// `Ui` cannot be built without a terminal — a decision no test can reach is
// one that gets its second chance in front of the user.
pub(super) fn body(
    folds: &Folds,
    scrollback: &[Row],
    reasoning: bool,
    partial: &str,
    width: usize,
    paint: &Paint,
) -> Vec<Line<'static>> {
    if folds.holds(reasoning, scrollback) {
        // The block's count row in the scrollback already answers the fold
        // switch; the live placeholder is only for the moment before the
        // block's first completed line exists to count.
        let counted = folds
            .streaming
            .is_some_and(|id| scrollback.iter().rev().any(|r| r.block() == Some(id)));
        if !counted {
            return vec![Line::from(paint.span(&paint.theme.muted, THINKING))];
        }
        return Vec::new();
    }
    if partial.is_empty() {
        return Vec::new();
    }
    // All of it, not the rows the terminal has room for: this is the only copy
    // until `close` lands it, and a scroll up has to reach its head.
    if reasoning {
        let muted = Line::from(paint.span(&paint.theme.muted, partial));
        screen::fit(&muted, width)
    } else {
        render::render_markdown(partial, paint)
            .into_iter()
            .flat_map(|line| screen::fit(&line, width))
            .collect()
    }
}
// The transcript as rows, exactly as the live stream would have drawn them:
// prompts with their sigil, answers as markdown, tool calls as their result
// lines, reasoning as a foldable block. A rewind rebuilds the screen from
// this, so the view returns to the point the conversation did.
pub(super) fn scrollback_from(
    session: &agent::session::Session,
    paint: &Paint,
    folds: &mut Folds,
) -> Vec<Row> {
    // A call whose result is in the session shows only its result row; one that
    // never got an answer (an interrupted turn) shows the start line instead,
    // the way `abandon_tools` leaves it.
    let answered: HashSet<String> = session
        .history()
        .filter_map(|e| match e {
            LogEntry::Tool { result: r, .. } if !agent::session::is_stopped_call(r) => {
                Some(r.call.clone())
            }
            _ => None,
        })
        .collect();

    let mut out = Vec::new();
    // History, not the view: compaction is the model losing sight of the
    // conversation, not the user. What it dropped is marked and kept.
    let mut hidden = false;
    let unseen = session.out_of_view();
    for entry in session.history() {
        let gone = unseen.contains(&entry.id());
        if gone != hidden && !matches!(entry, LogEntry::Compaction { .. }) {
            hidden = gone;
            if gone {
                out.push(Row::notice(Line::from(paint.span(
                    &paint.theme.muted,
                    format!(
                        "{} compacted; the model no longer sees the rest of this {}",
                        icons::COMPACT_RULE,
                        icons::COMPACT_RULE
                    ),
                ))));
            }
        }
        match entry {
            LogEntry::Answer { blocks, .. } => {
                for b in blocks {
                    match b {
                        AssistantContent::Text(t) => {
                            out.extend(Row::answer(&t.text, paint));
                        }
                        AssistantContent::ToolCall(c) => {
                            if answered.contains(&c.id) {
                                continue;
                            }
                            out.push(Row::tool_start(
                                &c.name,
                                &crate::store::text::summarize(&c.args),
                                paint,
                            ));
                        }
                        AssistantContent::Reasoning(r) => {
                            // Muted, exactly as the live stream paints a
                            // reasoning row: a rebuilt block must not come
                            // out brighter than the one it replaces.
                            let lines: Vec<Line<'static>> = r
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ReasoningContent::Text { text, .. } => Some(text.as_str()),
                                    _ => None,
                                })
                                .flat_map(str::lines)
                                .map(|l| Row::reasoning_line(l, paint))
                                .collect();
                            if lines.is_empty() {
                                continue;
                            }
                            // From the same counter the live stream draws
                            // from, because there is only one rule for what a
                            // block id is. Handing every rebuilt block `0`
                            // worked only for as long as nothing looked one up
                            // by id — and `streaming_row` and `stream_fold` both
                            // do, taking the last match.
                            out.push(Row::reasoning(folds.take_id(), lines, folds.folded));
                        }
                    }
                }
            }
            // Neither is anything the screen shows.
            LogEntry::Compaction { .. } => {}
            // Everything else the A table covers, the rebuild and a fresh
            // adoption draw from one place.
            other => {
                if let Some(rows) = f_entry(other, paint) {
                    if matches!(other, LogEntry::Tool { .. }) {
                        for r in rows {
                            push_tool_row(&mut out, r);
                        }
                    } else {
                        out.extend(rows);
                    }
                }
            }
        }
    }
    out
}

/// One entry's rows, as the rebuild and a fresh adoption both draw them:
/// the A table without its cross-entry markers. Answers do not pass through
/// here — their streamed rows are adopted by construction.
pub(super) fn f_entry(entry: &LogEntry, paint: &Paint) -> Option<Vec<Row>> {
    match entry {
        LogEntry::Ask { ask, .. } => Some(Row::prompt(ask.shown_text(), paint)),
        LogEntry::Bash { run, .. } => {
            let mut rows = Row::prompt(run.shown_text(), paint);
            rows.extend(app::bash::bash_said(&run.text).into_iter().map(Row::notice));
            Some(rows)
        }
        // Machine prose, not the user's line: rebuilt in the muted voice of
        // a screen notice rather than under the prompt sigil.
        LogEntry::Note { note, .. } => Some(
            note.lines()
                .map(|l| Row::notice(Line::from(paint.span(&paint.theme.muted, l))))
                .collect(),
        ),
        LogEntry::Tool {
            result: r, preview, ..
        } => (!agent::session::is_stopped_call(r))
            .then(|| vec![Row::stored_result(r, preview.as_deref())]),
        _ => None,
    }
}
// Scroll for the same window one frame later: growth since `last` folds
// back into the offset so the window keeps its place; `None` re-bases.
pub(super) fn absorb_growth(scroll: usize, last: Option<usize>, total: usize) -> usize {
    match last {
        Some(last) => scroll.saturating_add_signed(total as isize - last as isize),
        None => scroll,
    }
}
