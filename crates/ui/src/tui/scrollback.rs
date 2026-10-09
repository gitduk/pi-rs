//! The transcript as the scrollback walks it: the rows, the folded blocks
//! among them, and the window they are read through.

use std::collections::{HashMap, HashSet};

use agent::session::Entry as LogEntry;
use llm::message::{AssistantContent, ReasoningContent};
use ratatui::text::Line;

use super::call::push_tool_row;
use super::row::{Row, Step};
use super::screen;
use crate::render::{self, Paint};
use pi_core::core;
use pi_store::icons;

// Whether groups are folded, and which reasoning block streams now. A
// group's fold state resets to `folded` once it stops being last.
pub(super) struct Folds {
    // The next block id; a block keeps the id it was born with, so `land`
    // appends only to the open block.
    pub(super) next: u64,
    // The block streaming right now, if any.
    pub(super) streaming: Option<u64>,
    // What untouched groups are folded to: the value a group that stops being
    // last folds back to, and the target a global flip is measured from.
    pub(super) folded: bool,
    // How the last group — the one `ctrl+t` names — is folded; survives
    // the group itself until the key flips it again.
    pub(super) last: bool,
    // Each call's whole leading argument, by call id, from the answer that
    // made it until its result lands: the result does not carry it.
    pub(super) asked: HashMap<String, String>,
}

// Default is folded=true: results are rarely worth the scrollback space.
// Hand-written because `derive` would give `false` here.
impl Default for Folds {
    fn default() -> Self {
        Self {
            next: 1,
            streaming: None,
            folded: true,
            last: true,
            asked: HashMap::new(),
        }
    }
}

impl Folds {
    // Whether the reasoning streaming now is hidden behind a line: its
    // group's, or its own inside an unfolded one.
    pub(super) fn holds(&self, reasoning: bool, scrollback: &[Row]) -> bool {
        reasoning && self.stream_indent(scrollback).is_none()
    }

    // The indent the streaming block shows its lines under, if they show.
    pub(super) fn stream_indent(&self, scrollback: &[Row]) -> Option<&'static str> {
        let Some(id) = self.streaming else {
            return (!self.last).then_some("");
        };
        match scrollback.iter().rev().find(|r| r.holds_block(id)) {
            Some(row) => row.shows_block(id),
            None => (!self.last).then_some(""),
        }
    }

    // The group that was last stops being so: it folds back to the switch.
    fn retire_last(&mut self, scrollback: &mut [Row]) {
        if let Some(row) = last_group(scrollback) {
            row.set_folded(self.folded);
        }
    }

    // Keep what call `id` asked for, for the step it lands as.
    pub(super) fn ask(&mut self, id: &str, args: &serde_json::Value) {
        self.asked
            .insert(id.to_string(), render::asked(args).to_string());
    }

    // What call `id` asked for, once: its result has landed.
    pub(super) fn take_asked(&mut self, id: &str) -> String {
        self.asked.remove(id).unwrap_or_default()
    }

    // The next block id. The one place ids come from, so a rebuilt block and
    // a streamed one can never mean the same number.
    pub(super) fn take_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    // A step joins the last row's group, or opens a new one that retires the
    // old last group and is born the way `ctrl+t` left it.
    pub(super) fn join(&mut self, scrollback: &mut Vec<Row>, step: Step) {
        let step = match scrollback.last_mut() {
            Some(last) => match last.join(step) {
                Ok(()) => return,
                Err(step) => step,
            },
            None => step,
        };
        self.retire_last(scrollback);
        scrollback.push(Row::steps(step, self.birth_fold()));
    }

    // A new block of reasoning starts, in a group from its first delta: the
    // group's line is what shows it until a line lands.
    pub(super) fn start(&mut self, scrollback: &mut Vec<Row>) {
        let block = self.take_id();
        self.streaming = Some(block);
        self.join(
            scrollback,
            // Shut like any other step: an unfolded group lists its steps one
            // line each, and a click opens the one worth reading.
            Step::Thinking {
                block,
                lines: Vec::new(),
                open: false,
                live: true,
            },
        );
    }

    // The group of the block streaming now.
    pub(super) fn streaming_row<'a>(&self, scrollback: &'a mut [Row]) -> Option<&'a mut Row> {
        let id = self.streaming?;
        scrollback.iter_mut().rev().find(|r| r.holds_block(id))
    }

    // The last group stops being so the moment a new input is submitted: it
    // folds to the switch, its unfold lasting only while it was last.
    pub(super) fn fold_previous(&mut self, scrollback: &mut [Row]) {
        self.retire_last(scrollback);
    }

    // The streaming block is over, folded if its group is. One that never
    // had a line leaves nothing, and neither does a group it was alone in.
    pub(super) fn close_block(&mut self, scrollback: &mut Vec<Row>) {
        let Some(id) = self.streaming.take() else {
            return;
        };
        if let Some(at) = scrollback.iter().rposition(|r| r.holds_block(id))
            && scrollback[at].end_block(id)
        {
            scrollback.remove(at);
        }
    }

    // The value the next group is born with: however `ctrl+t` last left the
    // last group.
    pub(super) fn birth_fold(&self) -> bool {
        self.last
    }

    // Folds/unfolds every group and moves the switch with them: rows and
    // switch must never disagree, or the next group is born stale.
    pub(super) fn flip_all(&mut self, scrollback: &mut [Row]) {
        self.folded = !self.folded;
        self.last = self.folded;
        for row in scrollback.iter_mut() {
            row.set_folded(self.folded);
        }
    }

    // Flip the last group only; the switch is left alone. Flipped from what
    // the group shows, which a click or a lost call may have moved.
    pub(super) fn toggle_current(&mut self, scrollback: &mut [Row]) {
        let row = last_group(scrollback);
        self.last = match row.as_deref() {
            Some(r) => r.folded() == Some(false) && r.is_expandable(),
            None => !self.last,
        };
        if let Some(row) = row {
            row.set_folded(self.last);
        }
    }
}

// The newest group in the scrollback, if any.
fn last_group(scrollback: &mut [Row]) -> Option<&mut Row> {
    scrollback.iter_mut().rev().find(|r| r.is_steps())
}

// The scrollback as rows, walked from either end without flattening:
// `window` only needs the newest `want` rows.
pub(super) struct ScrollbackRows<'a> {
    rows: &'a [Row],
    // The frame's width, for the rows that clip to fit.
    width: usize,
    // For a folded group's line, which is synthesized at draw time and so
    // carries no paint of its own.
    paint: &'a Paint,
    // Next entry to read from the front, and the row offset inside it.
    front: (usize, usize),
    // Next entry to read from the back, and the row offset inside it.
    back: (usize, usize),
    // Each row's logical line count, the steps the two walks take. Wrapped
    // heights are the rows' own caches, summed per frame in `flush`.
    lens: Vec<usize>,
}

/// One line of the view: a scrollback row's line (asked for once the
/// window reaches it) or a live line already in hand.
pub(super) enum Piece<'a> {
    Row {
        row: &'a Row,
        line: usize,
        paint: &'a Paint,
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
                width,
            } => row.line_height(*line, paint, *width),
            Piece::Live(line) => screen::Piece::height(line),
        }
    }

    fn pieces(self) -> Vec<Line<'static>> {
        match self {
            Piece::Row {
                row,
                line,
                paint,
                width,
            } => {
                let (line, border) = row.line(line, paint, width);
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
    /// The window's rows; `keep` filters for browse mode (conversation
    /// only). A dropped row counts as zero lines for the window.
    pub(super) fn new(
        rows: &'a [Row],
        paint: &'a Paint,
        width: usize,
        keep: impl Fn(&Row) -> bool,
    ) -> Self {
        let back = rows.len().saturating_sub(1);
        let lens: Vec<usize> = rows
            .iter()
            .map(|r| if keep(r) { r.len() } else { 0 })
            .collect();
        let back_row = lens.get(back).copied().unwrap_or(0);
        Self {
            rows,
            width,
            paint,
            front: (0, 0),
            back: (back, back_row),
            lens,
        }
    }

    pub(super) fn indexed(self) -> IndexedScrollbackRows<'a> {
        IndexedScrollbackRows(self)
    }

    // Row `idx` line `line`, as the window wants it — count already
    // measured, text deferred until a frame shows it.
    pub(super) fn piece(&self, idx: usize, line: usize) -> Piece<'a> {
        Piece::Row {
            row: &self.rows[idx],
            line,
            paint: self.paint,
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

// Rows between the scrollback and status line: the reasoning window,
// plus the paragraph still being written.
pub(super) fn body(
    folds: &Folds,
    scrollback: &[Row],
    reasoning: bool,
    partial: &str,
    width: usize,
    paint: &Paint,
) -> Vec<Line<'static>> {
    // Folded, the group's own line in the scrollback is all it shows.
    if folds.holds(reasoning, scrollback) || partial.is_empty() {
        return Vec::new();
    }
    // All of it, not just the visible rows: this is the only copy until
    // `close` lands it, and scrolling up must reach its head.
    if reasoning {
        // Indented like the lines it will join.
        let indent = folds.stream_indent(scrollback).unwrap_or_default();
        let muted = Line::from(paint.span(&paint.theme.muted, partial));
        let border = (!indent.is_empty()).then(|| Line::from(indent));
        screen::wrap(border.as_ref(), &muted, width)
    } else {
        render::render_markdown(partial, paint)
            .into_iter()
            .flat_map(|line| screen::fit(&line, width))
            .collect()
    }
}
// The transcript as rows, exactly as the live stream would draw them.
// A rewind rebuilds from this, so the view returns to where it left off.
pub(super) fn scrollback_from(
    session: &agent::session::Session,
    paint: &Paint,
    folds: &mut Folds,
) -> Vec<Row> {
    // A call with a session result shows only its result row; an
    // interrupted one shows the start line, as `abandon_tools` leaves it.
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
                                folds.ask(&c.id, &c.args);
                                continue;
                            }
                            out.push(Row::tool_start(
                                &c.name,
                                &crate::render::summarize(&c.args),
                                paint,
                            ));
                        }
                        AssistantContent::Reasoning(r) => {
                            // Muted, exactly as the live stream paints reasoning:
                            // a rebuilt block must not come out brighter.
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
                            // From the live stream's id counter: lookups match
                            // by id, so every block needs a real, unique one.
                            let block = folds.take_id();
                            folds.join(
                                &mut out,
                                Step::Thinking {
                                    block,
                                    lines,
                                    open: false,
                                    live: false,
                                },
                            );
                        }
                    }
                }
            }
            // Neither is anything the screen shows.
            LogEntry::Compaction { .. } => {}
            // Everything else `f_entry` covers, the rebuild and a fresh
            // adoption draw from one place.
            other => {
                if let Some(rows) = f_entry(other, paint) {
                    if let LogEntry::Tool { result, .. } = other {
                        for r in rows {
                            push_tool_row(&mut out, folds, r, result);
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

/// One entry's rows, as both the rebuild and a fresh adoption draw them.
/// Answers don't pass through here — their rows stream directly.
pub(super) fn f_entry(entry: &LogEntry, paint: &Paint) -> Option<Vec<Row>> {
    match entry {
        LogEntry::Ask { ask, .. } if ask.relayed.is_some() => {
            Some(vec![Row::relayed(ask.shown_text(), &ask.text, paint)])
        }
        LogEntry::Ask { ask, .. } => Some(Row::prompt(ask.shown_text(), paint)),
        LogEntry::Bash { run, .. } => {
            let mut rows = Row::prompt(run.shown_text(), paint);
            rows.extend(
                core::bash::bash_said(&run.text)
                    .into_iter()
                    .map(Row::notice),
            );
            Some(rows)
        }
        // Machine prose, not the user's line: rebuilt in the muted voice of
        // a screen notice rather than under the prompt sigil.
        LogEntry::Note { note, .. } => Some(Row::notice_lines(note, paint)),
        // Same voice for screen-only rows — a tally line, a turn warning —
        // so the rebuild reuses the text the surface already worded.
        LogEntry::Screen { text, .. } => Some(Row::notice_lines(text, paint)),
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
