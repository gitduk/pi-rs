//! A run's events becoming rows: the text as it streams, the tool calls as
//! they run and finish, and the block that closes when the turn does.
use super::Ui;
use super::call::{Run, RunTool, push_tool_row};
use super::row::Row;
use super::scrollback::f_entry;
use super::search::Spot;
use super::view::{Doing, StreamKind, Surface, View, snapshot};
use crate::render;
use crate::status;
use agent::Event;
use agent::session::Entry as LogEntry;
use pi_core::core::lane::Lane;
use pi_store::icons;
use ratatui::text::Line;
use std::time::Instant;

impl Ui {
    pub(super) fn say(&mut self, view: &mut View, line: impl Into<String>) {
        let text = line.into();
        if text.is_empty() {
            view.surface.scrollback.push(Row::aside(String::new()));
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
        view.surface.scrollback.push(Row::aside(line));
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
        if let Some(work) = &mut view.state.work {
            if matches!(
                event,
                Event::TextDelta(_) | Event::ReasoningDelta(_) | Event::ToolQueued { .. }
            ) && let Doing::Turn { committed } = &mut work.doing
            {
                *committed = true;
            }
            if !matches!(event, Event::Retrying { .. }) {
                work.retry = None;
            }
        }
        // Every number either status line shows is read here, once. The arms
        // below decide only what reaches the scrollback.
        lane.note(&event);
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
                    // Kept, not just said: nothing else holds these numbers,
                    // so a rebuild could not draw an aside.
                    let line = parts.join(icons::PART_SEP);
                    self.say_kept(lane, view, &line);
                }
            }
            // What the endpoint left out of the reply just streamed: filed under
            // it, since it is about that reply and stays true of it.
            Event::Warning(w) => {
                self.close(view);
                self.say_kept(lane, view, w);
            }
            // State, not rows: the usage line marks it, `/status` says why.
            Event::Refit(_) => {}
            Event::Compacted(_) if view.state.work.is_some() => {}
            // A `/compact` asked for: its report is the answer, filed as worded.
            Event::Compacted(r) => {
                self.close(view);
                let line = pi_core::core::meter::compaction_line(r);
                self.say_kept(lane, view, &line);
            }
            // A call's events are one line here: queued takes a place in the
            // group or live region, start sets its clock, end settles it,
            // each matched back by id.
            Event::ToolQueued { id, name, args } => {
                self.close(view);
                let place = view.surface.folds.ask(id, args, lane.root());
                view.state.tools.push(RunTool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: crate::render::summarize(args),
                    place,
                    run: Run::Queued,
                });
            }
            Event::ToolStart { id, at } => {
                if let Some(t) = view.state.tools.iter_mut().find(|t| t.id == *id) {
                    t.run = Run::Running {
                        since: *at,
                        progress: None,
                    };
                }
            }
            Event::ToolProgress { id, text } => {
                if let Some(t) = view.state.tools.iter_mut().find(|t| t.id == *id)
                    && let Run::Running { progress, .. } = &mut t.run
                {
                    *progress = Some(text.clone());
                }
            }
            Event::ToolEnd {
                id,
                name,
                outcome,
                preview,
                took,
            } => {
                self.close(view);
                // Parked until the entry commits; adoption checks it against
                // what the entry derives to.
                if let Some(t) = view.state.tools.iter_mut().find(|t| t.id == *id) {
                    let row =
                        Row::result(*outcome, name.clone(), preview.clone()).timed(Some(*took));
                    t.run = Run::Landed(row.at(t.place.clone()));
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
                if let Some(work) = &mut view.state.work {
                    work.retry = Some(format!(
                        "retry {attempt} in {}",
                        render::fmt_delay(*delay_ms)
                    ));
                }
                self.say_muted(view, reason.as_str());
            }
            _ => {
                self.close(view);
                if let Some(said) = render::describe(&event, &self.paint, self.screen.width()) {
                    view.surface
                        .scrollback
                        .extend(said.into_iter().map(Row::notice));
                }
            }
        }
    }

    // The submitted line onto the screen: the row its answer lands under.
    pub(super) fn submit(&mut self, view: &mut View, line: &str) {
        let rows = Row::prompt(line, &self.paint);
        super::scrollback::open_turn(&mut view.surface.scrollback);
        view.surface.scrollback.extend(rows);
        view.surface
            .folds
            .fold_previous(&mut view.surface.scrollback);
    }

    pub(super) fn submit_relayed(&mut self, view: &mut View, label: &str, line: &str) {
        let row = Row::relayed(label, line, agent::session::now(), &self.paint);
        super::scrollback::open_turn(&mut view.surface.scrollback);
        view.surface.scrollback.push(row);
        view.surface
            .folds
            .fold_previous(&mut view.surface.scrollback);
    }

    // `say`, kept in the transcript (an `Entry::Screen`) so a rebuild draws
    // it too; the tail moves here so a later adopt won't draw it twice.
    pub(super) fn say_kept(&self, lane: &mut Lane, view: &mut View, line: &str) {
        let kept = lane.push_screen(line);
        self.draw_kept(view, Row::notice_lines(line, &self.paint), kept);
    }

    // `say_kept` as a note, which the model reads before it next speaks.
    pub(super) fn say_note(&self, lane: &mut Lane, view: &mut View, line: &str) {
        let kept = lane.push_note(line);
        self.draw_kept(view, Row::note_lines(line), kept);
    }

    fn draw_kept(&self, view: &mut View, rows: Vec<Row>, kept: Option<agent::session::EntryId>) {
        view.surface.scrollback.extend(rows);
        if let Some(id) = kept {
            view.surface.tail = Some(id);
        }
    }

    // A run that ended without answering a call leaves its row dangling
    // (the call's own end event is never sent), so file the start line.
    pub(super) fn abandon_tools(&mut self, view: &mut View) {
        for t in std::mem::take(&mut view.state.tools) {
            view.surface.folds.take_asked(&t.id);
            let row = match t.run {
                Run::Landed(row) => row,
                Run::Queued | Run::Running { .. } => {
                    Row::tool_start(&t.name, &t.summary, &self.paint)
                }
            };
            view.surface.scrollback.push(row);
        }
    }

    // Folds freshly committed entries through `f_entry`, retiring the
    // pending live-region lines they supersede — checked for drift.
    pub(super) fn adopt(&self, view: &mut View, entries: &[LogEntry]) {
        let width = self.screen.width();
        for entry in entries {
            if view.surface.tail.is_some_and(|t| entry.id() <= t) {
                continue;
            }
            if let Some(rows) = f_entry(entry, &self.paint) {
                if let LogEntry::Tool { result, .. } = entry {
                    // Placed before the drift check paints them: a row is
                    // never drawn without where its call ran.
                    let (asked, place) = view.surface.folds.take_asked(&result.call);
                    let rows: Vec<Row> = rows.into_iter().map(|r| r.at(place.clone())).collect();
                    self.check_pending(view, &result.call, rows.last(), width);
                    for r in rows {
                        push_tool_row(
                            &mut view.surface.scrollback,
                            &mut view.surface.folds,
                            r,
                            &asked,
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
        let Run::Landed(pending) = view.state.tools.remove(at).run else {
            return;
        };
        if let Some(row) = row {
            let parked = pending.line(0, &self.paint, width).0;
            let derived = row.line(0, &self.paint, width).0;
            debug_assert_eq!(parked, derived, "a tool's two lines disagreed");
        }
    }

    // Rebuilds from the transcript, discarding the old drawing — a rewind
    // changes the conversation, not just adds a note to the old one.
    pub(super) fn rebuild(
        &mut self,
        view: &mut View,
        session: &agent::session::Session,
        root: &std::path::Path,
    ) {
        let folded = view.surface.folds.folded;
        view.surface = Surface::from(session, &self.paint, folded, root);
        view.drawn = true;
        self.rows_moved();
    }

    // The asides gone from the screen, the line left as typed.
    pub(super) fn tidy(&mut self, view: &mut View) {
        if view.surface.scrollback.tidy() {
            view.surface.top = None;
            view.surface.scroll = 0;
            self.rows_moved();
        }
    }

    // What holds a row by its index lets go: the rows are new ones now.
    fn rows_moved(&mut self) {
        // A page held to a transcript row.
        if let Some(read) = &mut self.read
            && read.top.is_some_and(|a| !matches!(a.spot, Spot::Text(_)))
        {
            read.top = None;
        }
        // And a search on them, which holds them too.
        if self.read.is_none() {
            self.search = None;
        }
    }
}
