//! A run's events becoming rows: the text as it streams, the tool calls as
//! they run and finish, and the block that closes when the turn does.
use super::Ui;
use super::bar::Facts;
use super::call::{self, RunTool, push_tool_row};
use super::mouse::{Regions, Target};
use super::row::{PendingTool, Row};
use super::screen;
use super::screen::Rows;
use super::scrollback::{Piece, ScrollbackRows, absorb_growth, f_entry};
use super::ui::Focus;
use super::view::{StreamKind, Surface, View, snapshot};
use crate::render;
use crate::status;
use agent::Event;
use agent::session::Entry as LogEntry;
use pi_core::core::lane::Lane;
use pi_store::icons;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{List, ListState};
use std::time::Instant;

impl Ui {
    pub(super) fn say(&mut self, view: &mut View, line: impl Into<String>) {
        let text = line.into();
        if text.is_empty() {
            view.surface.scrollback.push(Row::notice(String::new()));
            return;
        }
        for text_line in text.lines().map(str::to_string) {
            self.say_line(view, Line::from(text_line));
        }
    }

    // `say` in the muted voice.
    pub(super) fn say_muted(&mut self, view: &mut View, line: impl Into<String>) {
        for text in line.into().lines() {
            let line = Line::from(self.paint.span(&self.paint.theme.muted, text));
            self.say_line(view, line);
        }
    }

    // A line already built in spans — the styled word a run ends on, say —
    // landing as one row. Plain text goes through `say`, which splits it.
    pub(super) fn say_line(&mut self, view: &mut View, line: Line<'static>) {
        if let Some(last) = view.surface.scrollback.last_mut()
            && last.repeated(&line)
        {
            return;
        }
        view.surface.scrollback.push(Row::notice(line));
    }

    // A short answer nobody reads back, for the bar row: no scrollback
    // entry, since what a press did *not* do isn't part of the transcript.
    pub(super) fn flash(&mut self, line: impl Into<String>) {
        self.flash = Some((line.into(), Instant::now()));
    }

    // Where a finished row goes: a reasoning line into the streaming block's
    // group, anything else straight into scrollback.
    pub(super) fn land(&mut self, view: &mut View, painted: Line<'static>, reasoning: bool) {
        let surface = &mut view.surface;
        // `start` made the group before any line could land.
        if reasoning
            && let Some(id) = surface.folds.streaming
            && let Some(row) = surface.folds.streaming_row(&mut surface.scrollback)
        {
            row.push_line(id, painted);
            return;
        }
        surface.scrollback.push(Row::notice(painted));
    }

    pub(super) fn close(&mut self, view: &mut View) {
        let reasoning = view.surface.stream.kind == StreamKind::Reasoning;
        if !view.surface.stream.text.is_empty() {
            let text = std::mem::take(&mut view.surface.stream.text);
            if reasoning {
                let painted = Row::reasoning_line(&text, &self.paint);
                self.land(view, painted, reasoning);
            } else {
                view.surface
                    .scrollback
                    .extend(Row::answer(&text, &self.paint));
            }
        }
        if reasoning {
            // The block is over: it stops taking lines, and its group stays
            // where it is.
            view.surface.folds.close_block(&mut view.surface.scrollback);
        }
        view.surface.stream.kind = StreamKind::Answer;
    }

    pub(super) fn write(&mut self, view: &mut View, delta: &str, reasoning: bool) {
        let kind = if reasoning {
            StreamKind::Reasoning
        } else {
            StreamKind::Answer
        };
        if view.surface.stream.kind != kind {
            self.close(view);
            view.surface.stream.kind = kind;
            if reasoning {
                // `close` settled the previous block; this one joins the last
                // group or starts one.
                view.surface.folds.start(&mut view.surface.scrollback);
            }
        }

        view.surface.stream.text.push_str(delta);
        if reasoning {
            // A finished reasoning line belongs in the streaming block's
            // group as soon as it ends.
            while let Some(i) = view.surface.stream.text.find('\n') {
                let line: String = view.surface.stream.text.drain(..=i).collect();
                let line = line.trim_end_matches('\n').to_string();
                let painted = Row::reasoning_line(&line, &self.paint);
                self.land(view, painted, reasoning);
            }
        }
    }

    pub(super) fn on_event(&mut self, lane: &mut Lane, view: &mut View, event: Event) {
        // Anything the model produces spends the chance to unsend: past this
        // the prompt has been answered, not merely sent.
        if matches!(
            event,
            Event::TextDelta(_) | Event::ReasoningDelta(_) | Event::ToolStart { .. }
        ) {
            view.state.committed = true;
        }
        // Every number either status line shows is read here, once. The arms
        // below decide only what reaches the scrollback.
        lane.note(&event);
        if !matches!(event, Event::Retrying { .. }) {
            view.state.retry = None;
        }
        match &event {
            Event::TextDelta(d) => self.write(view, d, false),
            Event::ReasoningDelta(d) => self.write(view, d, true),
            // Counted already, and none of them draws a row of its own.
            Event::Usage(_)
            | Event::TurnEnd { .. }
            | Event::TurnStart { .. }
            | Event::Context { .. } => {}
            Event::Done { .. } => {
                self.close(view);
                // Still running as far as the screen is concerned: `turn`
                // clears the clock only once the loop returns.
                let snap = snapshot(lane, view);
                // A run whose segments all had nothing to say leaves no row.
                let parts = status::parts(&self.status, &snap);
                if !parts.is_empty() {
                    // Filed, not just drawn: nothing else holds these numbers,
                    // so a rebuild could not draw an unfiled row.
                    let line = parts.join(icons::PART_SEP);
                    self.file_screen(lane, view, &line);
                }
            }
            // A warning about the turn — reshaped prompt, short reply, dropped
            // field. Filed and drawn muted, since a rebuild can't recolor it.
            Event::Warning(w) => {
                self.close(view);
                let line = format!("{} {w}", icons::WARN_MARK);
                self.file_screen(lane, view, &line);
            }
            // What compaction gave up; the numbers live only in the pass's
            // own report, so they're filed as worded here.
            Event::Compacted(r) => {
                self.close(view);
                let line = render::compaction_line(r);
                self.file_screen(lane, view, &line);
            }
            // A call's two events are one line here: start takes a place in
            // the group or live region; end settles it, matched back by id.
            Event::ToolStart { id, name, args, .. } => {
                self.close(view);
                view.surface.folds.ask(id, args);
                view.state.tools.push(RunTool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: crate::render::summarize(args),
                    started: std::time::Instant::now(),
                    progress: None,
                    done: None,
                });
            }
            Event::ToolProgress { id, text } => {
                if let Some(t) = view.state.tools.iter_mut().find(|t| t.id == *id) {
                    t.progress = Some(text.clone());
                }
            }
            Event::ToolEnd {
                id,
                name,
                is_error,
                preview,
            } => {
                self.close(view);
                // Parked until the entry commits; adoption checks it against
                // what the entry derives to.
                if let Some(t) = view.state.tools.iter_mut().find(|t| t.id == *id) {
                    t.done = Some(Row::result(!*is_error, name.clone(), preview.clone()));
                    t.progress = None;
                }
            }
            // New entries: derive rows via `f_entry`, check them against
            // pending live-region lines, and file — never draw directly.
            Event::Committed { entries } => {
                self.close(view);
                self.adopt(view, entries);
            }
            // Transport news, not part of the answer: wait goes on the
            // status line, the reason in its own row for full reading.
            Event::Retrying {
                attempt,
                delay_ms,
                reason,
            } => {
                self.close(view);
                view.state.retry = Some(format!(
                    "retry {attempt} in {}",
                    render::fmt_delay(*delay_ms)
                ));
                self.say_muted(view, reason.as_str());
            }
            _ => {
                self.close(view);
                if let Some(said) = render::describe(&event, &self.paint, self.screen.usable()) {
                    view.surface
                        .scrollback
                        .extend(said.into_iter().map(Row::notice));
                }
            }
        }
    }

    // Files this lane's row and draws it now, so a rebuild draws the same
    // thing; moves the tail cursor here so a later adopt won't redraw it.
    pub(super) fn file_screen(&self, lane: &mut Lane, view: &mut View, line: &str) {
        let filed = lane.push_screen(line);
        view.surface
            .scrollback
            .extend(Row::notice_lines(line, &self.paint));
        if let Some(id) = filed {
            view.surface.tail = Some(id);
        }
    }

    // A run that ended without answering a call leaves its row dangling
    // (the call's own end event is never sent), so file the start line.
    pub(super) fn abandon_tools(&mut self, view: &mut View) {
        for t in std::mem::take(&mut view.state.tools) {
            view.surface.folds.take_asked(&t.id);
            let row = t
                .done
                .unwrap_or_else(|| Row::tool_start(&t.name, &t.summary, &self.paint));
            view.surface.scrollback.push(row);
        }
    }

    // Folds freshly committed entries through `f_entry`, retiring the
    // pending live-region lines they supersede — checked for drift.
    pub(super) fn adopt(&self, view: &mut View, entries: &[LogEntry]) {
        let width = self.screen.usable();
        for entry in entries {
            if view.surface.tail.is_some_and(|t| entry.id() <= t) {
                continue;
            }
            if let Some(rows) = f_entry(entry, &self.paint) {
                if let LogEntry::Tool { result, .. } = entry {
                    self.check_pending(view, &result.call, rows.last(), width);
                    for r in rows {
                        push_tool_row(
                            &mut view.surface.scrollback,
                            &mut view.surface.folds,
                            r,
                            result,
                        );
                    }
                } else {
                    view.surface.scrollback.extend(rows);
                }
            }
            view.surface.tail = Some(entry.id());
        }
    }

    // Retires the row `ToolEnd` parked, checked against what the entry
    // derives to; disagreement is drift, caught only in debug builds.
    fn check_pending(&self, view: &mut View, call: &str, row: Option<&Row>, width: usize) {
        let Some(at) = view.state.tools.iter().position(|t| t.id == call) else {
            return;
        };
        let Some(pending) = view.state.tools.remove(at).done else {
            return;
        };
        if let Some(row) = row {
            let parked = pending.line(0, &self.paint, width).0;
            let derived = row.line(0, &self.paint, width).0;
            debug_assert_eq!(parked, derived, "a tool's two lines disagreed");
        }
    }

    pub(super) fn flush(&mut self, lane: &Lane, view: &mut View) {
        let menu = self.menu();
        let width = self.screen.usable();
        // Browse mode is the conversation alone (no thinking, calls, notices,
        // editor); the tally and window share this one predicate.
        let browse = self.browsing();
        let keep = move |row: &Row| !browse || row.is_conversation();
        let mut bar = super::call::job_lines(&self.jobs, self.spinner, &self.paint, width);
        bar.extend(self.bar_lines(&Facts::of(lane), &snapshot(lane, view), width));
        // The bar's rows are not worth a terminal that cannot hold them, a row
        // to type on and a row of history: one that short keeps the other two.
        bar.truncate((self.screen.height as usize).saturating_sub(2));
        let bar_h = bar.len();
        // Nothing to type on, so nothing to pin to the bottom: the rows the
        // editor would have taken go to the history.
        let (input, caret) = if browse {
            (Vec::new(), (0, 0))
        } else {
            self.editor.view(&self.paint, width)
        };
        // A paste taller than the terminal must not push the editor area off
        // the bottom; the editor scrolls to keep the caret's row visible.
        let editor_h = input
            .len()
            .min((self.screen.height as usize).saturating_sub(1 + bar_h));
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let input_view: Vec<Line<'static>> =
            input.into_iter().skip(editor_top).take(editor_h).collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        // Bottom-up: input pinned, menu above it (reply or completions, one
        // at a time), history fills the rest — bar always keeps its row.
        let gap = super::mouse::HISTORY_GAP as usize;
        let room = (self.screen.height as usize).saturating_sub(editor_h + bar_h + 1 + gap);
        let reply = match &self.focus {
            Focus::Reply(r) => Some(r.view(room, width, &self.paint)),
            _ => None,
        };
        let menu_h = if let Some(reply) = &reply {
            reply.len()
        } else if menu.is_empty() {
            0
        } else {
            menu.len().min(room)
        };
        // Every pinned row, bar included: what `Fill(1)` is left with;
        // `Rows` fills top-down and drops overflow off the bottom (newest).
        let hist_view = (self.screen.height as usize)
            .saturating_sub(editor_h + menu_h + bar_h + gap)
            .max(1);
        // The group live calls fold into, when it's the last scrollback
        // row: theirs to draw, so the live block leaves them alone.
        let last = view.surface.scrollback.len().checked_sub(1);
        let row_holds = view.surface.scrollback.last().is_some_and(Row::is_steps);
        let now = std::time::Instant::now();
        let held = if row_holds {
            call::held(&view.state.tools, now)
        } else {
            Vec::new()
        };
        if self
            .copied
            .as_ref()
            .is_some_and(|at| at.elapsed() >= super::FLASH)
        {
            self.copied = None;
        }
        for (idx, row) in view.surface.scrollback.iter_mut().enumerate() {
            // Only the last row takes them, and only the last row can be the
            // summary they belong to: any other is handed nothing.
            let flight: &[PendingTool] = if Some(idx) == last { &held } else { &[] };
            let hovered = self.hovered_scrollback.filter(|h| h.0 == idx).map(|h| h.1);
            row.update_live(hovered, self.spinner, flight);
        }
        let (live, pending_rows) = self.live(lane, view, row_holds, now);

        // While scrolled up, bottom growth folds into `scroll` via cached row
        // heights — counting lines instead would misfill the visible area.
        if view.surface.scroll > 0 {
            let total = view
                .surface
                .scrollback
                .iter()
                .filter(|r| keep(r))
                .map(|r| r.height(&self.paint, width))
                .sum::<usize>()
                + live.len();
            view.surface.scroll = absorb_growth(view.surface.scroll, view.surface.counted, total);
            view.surface.counted = Some(total);
        } else {
            view.surface.counted = None;
        }
        let scrollback = ScrollbackRows::new(&view.surface.scrollback, &self.paint, width, keep)
            .indexed()
            .map(|(item, idx)| {
                let line = match &item {
                    Piece::Row { line, .. } => *line,
                    Piece::Live(_) => 0,
                };
                (item, Target::Scrollback(idx, line))
            });

        // The pending-call rows lead the live block; a click on one opens or
        // closes the batch.
        let live_stream = live.iter().enumerate().map(move |(i, s)| {
            let target = if i < pending_rows {
                Target::PendingTools
            } else {
                Target::None
            };
            // In hand already, and the window takes it as it is.
            (
                Piece::Live(screen::Ready {
                    line: s.clone(),
                    width,
                }),
                target,
            )
        });

        let (tagged_rows, scroll, cut) = screen::window_tagged(
            scrollback.chain(live_stream),
            hist_view,
            view.surface.scroll,
        );

        let (rows, row_targets): (Vec<Line<'static>>, Vec<Target>) =
            tagged_rows.into_iter().unzip();
        self.row_targets = row_targets;
        self.top_cut = cut;
        self.drawn = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        let rows = match &self.selection {
            Some(sel) => {
                let lead = |at: usize| match self.row_targets.get(at) {
                    Some(Target::Scrollback(idx, line)) => view
                        .surface
                        .scrollback
                        .get(*idx)
                        .and_then(|r| r.line(*line, &self.paint, width).1)
                        .map_or(0, |b| b.width()),
                    _ => 0,
                };
                super::select::highlight(rows, sel, lead)
            }
            None => rows,
        };

        view.surface.scroll = scroll;
        let picked = self
            .picked
            .unwrap_or(menu.len().saturating_sub(1))
            .min(menu.len().saturating_sub(1));
        let items = self.menu_items(&menu, picked);
        // The picked row on the input's band, the menu's width: a bar the eye
        // finds at once, where a mark in its text alone was not enough.
        let cursor = self
            .paint
            .band(&self.paint.theme.prompt.panel.input)
            .unwrap_or_default();
        let _ = self.screen.draw(|frame| {
            // Input last, so the caret sits on the bottom row and the bar
            // reads as history's edge, not something hanging off the typed line.
            let regions =
                Regions::layout(frame.area(), menu_h as u16, bar_h as u16, editor_h as u16);
            self.regions = regions;
            frame.render_widget(Rows(&rows), regions.history);
            // While the keys are a panel's, the transcript under it steps back so
            // the two do not read as one; a menu that comes with typing does not.
            if matches!(self.focus, Focus::Reply(_) | Focus::Rewind(_)) {
                frame.buffer_mut().set_style(
                    regions.history,
                    ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM),
                );
            }
            if let Some(reply) = &reply {
                frame.render_widget(Rows(reply), regions.menu);
            } else if !items.is_empty() {
                // Kept full: ratatui won't pull back an offset that a taller
                // screen or a changed list left past the end.
                let mut state = ListState::default()
                    .with_offset(self.menu_top.min(menu.len() - menu_h))
                    .with_selected(Some(picked));
                frame.render_stateful_widget(
                    List::new(items).highlight_style(cursor),
                    regions.menu,
                    &mut state,
                );
                self.menu_top = state.offset();
            }
            frame.render_widget(Rows(&bar), regions.bar);
            // Before the rows, so a span over the band keeps it: the columns the
            // input does not reach carry it too, one short of the edge.
            if let Some(band) = self.band {
                frame.buffer_mut().set_style(
                    Rect {
                        width: width as u16,
                        ..regions.editor
                    },
                    band,
                );
            }
            frame.render_widget(Rows(&input_view), regions.editor);
            // The caret marks where keys land: nowhere in the line while
            // something else has them.
            if matches!(self.focus, Focus::Editor) {
                let caret_row = regions.editor.y + caret_in_view as u16;
                frame.set_cursor_position((caret.1, caret_row));
            }
        });
        // Bold doesn't move a row, so a second frame settles it.
        let over = self
            .pointer
            .and_then(|(col, row)| self.hovered_row(view, col, row));
        if over != self.hovered_scrollback {
            self.hovered_scrollback = over;
            self.redraw = true;
        }
    }

    // Rebuilds from the transcript, discarding the old drawing — a rewind
    // changes the conversation, not just adds a note to the old one.
    pub(super) fn rebuild(&mut self, view: &mut View, session: &agent::session::Session) {
        let folded = view.surface.folds.folded;
        view.surface = Surface::from(session, &self.paint, folded);
        view.drawn = true;
    }
}
