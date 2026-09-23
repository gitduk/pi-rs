//! A run's events becoming rows: the text as it streams, the tool calls as
//! they run and finish, and the block that closes when the turn does.
use super::mouse::{Regions, Target};
use super::row::{PendingTool, Row};
use super::screen;
use super::screen::Rows;
use super::scrollback::{Piece, ScrollbackRows, absorb_growth, f_entry};
use super::tool::{self, RunTool, push_tool_row};
use super::view::{StreamKind, Surface, View, snapshot};
use super::{BAR_H, FLASH, Ui};
use crate::app::lane::Lane;
use crate::ui::render;
use crate::ui::status;
use agent::Event;
use agent::session::Entry as LogEntry;
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

    // `say` in the muted voice, for the callers that used to hand a painted
    // string through it.
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

    // Answer one keypress on the bar row and leave nothing behind.
    //
    // The scrollback is a transcript, and what a press did *not* do is not
    // part of one — sent there it also stacked a row per press, which is how
    // holding the step key in a single checkout wrote a screenful of one line.
    // Muted here rather than at the callers, which had drifted apart on it.
    pub(super) fn flash(&mut self, line: impl Into<String>) {
        let text = Line::from(self.paint.span(&self.paint.theme.muted, line.into()));
        self.flash = Some((text, Instant::now()));
    }

    // The bar row while a flash is up, and the only place an expired one is
    // dropped — every frame passes through here, so nothing else has to
    // remember to clear it.
    fn flash_line(&mut self, width: usize) -> Option<Line<'static>> {
        if self
            .flash
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= FLASH)
        {
            self.flash = None;
        }
        let (text, _) = self.flash.as_ref()?;
        screen::fit(text, width).into_iter().next()
    }

    // The bar row, whatever it is saying: the flash while it is up, the bar's
    // own line otherwise, and a bare row when there is neither — never nothing,
    // so nothing above it moves.
    fn bar_line(&mut self, model: &str, width: usize) -> Line<'static> {
        self.flash_line(width)
            .or_else(|| self.lane_bar(model, width))
            .unwrap_or_default()
    }

    // Where a finished row goes: a reasoning line into the streaming block's
    // foldable entry, anything else straight into scrollback.
    pub(super) fn land(&mut self, view: &mut View, painted: Line<'static>, reasoning: bool) {
        if reasoning && let Some(id) = view.surface.folds.streaming {
            if let Some(row) = self.streaming_row(view, id) {
                row.push_line(painted);
                return;
            }
            // The block's first line: born the way `ctrl+t` last left the last
            // block — its own fold, not the switch.
            view.surface.scrollback.push(Row::reasoning(
                id,
                vec![painted],
                view.surface.folds.birth_fold(),
            ));
            return;
        }
        view.surface.scrollback.push(Row::notice(painted));
    }

    // The scrollback entry for a streaming block, if it has one yet.
    pub(super) fn streaming_row<'a>(&mut self, view: &'a mut View, id: u64) -> Option<&'a mut Row> {
        view.surface
            .scrollback
            .iter_mut()
            .rev()
            .find(|r| r.block() == Some(id))
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
            // The block is over: it stops taking lines; its entry is already
            // in the scrollback, folded or not.
            view.surface.folds.close_block();
            if view
                .surface
                .scrollback
                .last()
                .is_some_and(Row::is_empty_reasoning)
            {
                view.surface.scrollback.pop();
            }
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
                // A new reasoning block: `close` just settled the previous
                // one; this one gets a fresh id and pushes the old last one
                // back to the switch.
                view.surface.folds.start(&mut view.surface.scrollback);
            }
        }

        view.surface.stream.text.push_str(delta);
        if reasoning {
            // A finished reasoning line belongs in the streaming block's
            // foldable entry as soon as it ends.
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
                // Asked now rather than at every draw: a run whose segments
                // all had nothing to say leaves no row, and a blank one is
                // worse than none.
                if !status::parts(&self.done, &snap).is_empty() {
                    view.surface.scrollback.push(Row::tally(snap));
                }
            }
            // A call's two events are one line here: the start either hands
            // the call to the summary row above or takes a line in the live
            // region (where the spinner can animate it), and the end settles
            // that line to the ✗/✓ mark it lands with. Parallel calls each
            // hold a place of their own, matched back by id because they end
            // out of order.
            Event::ToolStart { id, name, args, .. } => {
                self.close(view);
                view.state.tools.push(RunTool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: crate::store::text::summarize(args),
                    done: None,
                });
            }
            Event::ToolEnd {
                id,
                name,
                is_error,
                preview,
            } => {
                self.close(view);
                // Not a row yet: the row the entry will adopt parks here
                // until the committed entries arrive, and adoption checks it
                // against what the entry itself derives to.
                if let Some(t) = view.state.tools.iter_mut().find(|t| t.id == *id) {
                    t.done = Some(Row::result(!*is_error, name.clone(), preview.clone()));
                }
            }
            // The transcript gained entries. Derive their rows through the A
            // table, check them against the pending live-region lines, and
            // file them: the event carries state, never drawing instructions.
            Event::Committed { entries } => {
                self.close(view);
                self.adopt(view, entries);
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

    // A run that ended without answering a call leaves its animated row
    // dangling. The call's own end event is never sent — a cancelled run
    // returns before its results are reported — so give the scrollback the
    // start line the row stood for and clear the row.
    pub(super) fn abandon_tools(&mut self, view: &mut View) {
        for t in std::mem::take(&mut view.state.tools) {
            let row = t
                .done
                .unwrap_or_else(|| Row::tool_start(&t.name, &t.summary, &self.paint));
            view.surface.scrollback.push(row);
        }
    }

    // Fold freshly committed entries into the scrollback through the A table
    // itself, and retire the pending live-region lines they supersede. The
    // two lines are built from different halves — the event's facts and the
    // entry's content — so their equality is the drift alarm the
    // two-producer layout used to lack.
    pub(super) fn adopt(&self, view: &mut View, entries: &[LogEntry]) {
        let width = self.screen.usable();
        for entry in entries {
            if view.surface.tail.is_some_and(|t| entry.id() <= t) {
                continue;
            }
            if let Some(rows) = f_entry(entry, &self.paint) {
                if let LogEntry::Tool { result: r, .. } = entry {
                    self.check_pending(view, &r.call, rows.last(), width);
                    for r in rows {
                        push_tool_row(&mut view.surface.scrollback, r);
                    }
                } else {
                    view.surface.scrollback.extend(rows);
                }
            }
            view.surface.tail = Some(entry.id());
        }
    }

    // Retire the row a `ToolEnd` parked, checking it against the row the
    // committed entry derives to. Equality is expected; anything else is
    // drift the old layout shipped silently.
    fn check_pending(&self, view: &mut View, call: &str, row: Option<&Row>, width: usize) {
        let Some(at) = view.state.tools.iter().position(|t| t.id == call) else {
            return;
        };
        let Some(pending) = view.state.tools.remove(at).done else {
            return;
        };
        if let Some(row) = row {
            let parked = pending.line(0, &self.paint, &[], width).0;
            let derived = row.line(0, &self.paint, &[], width).0;
            debug_assert_eq!(parked, derived, "a tool's two lines disagreed");
        }
    }

    pub(super) fn flush(&mut self, lane: &Lane, view: &mut View) {
        let menu = self.menu();
        let width = self.screen.usable();
        // Browse mode is the conversation alone: the thinking, the calls, the
        // notices and the editor itself all go, and one predicate says so. The
        // tally that follows the scroll and the window that draws it read the
        // same one, so a scrolled-up browse measures what it shows.
        let browse = self.browsing;
        let keep = move |row: &Row| !browse || row.is_conversation();
        // A flash outranks the bar's own line: it is gone in a moment, where
        // that line is always a keystroke away.
        let bar = self.bar_line(lane.model(), width);
        // The bar's row is not worth a terminal that cannot hold it, a row to
        // type on and a row of history: one that short keeps the other two, and
        // what is being typed keeps its row.
        let bar_h = BAR_H.min((self.screen.height as usize).saturating_sub(2));
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
        // From the bottom up: the input line is pinned, the menu sits above
        // it, and the scrolled history fills what is left. The caret's row
        // therefore depends only on the pinned rows, never on how the
        // history wraps.
        // One space, one panel: they all draw over the menu, and the surface
        // can hold only one of them at a time.
        let panel = self.panel.as_ref().map(|p| p.view(&self.paint, width));
        let panel_h = panel.as_ref().map_or(0, |(r, _)| r.len());
        // Both branches leave the bar its row: a menu tall enough to take it
        // would drop whatever that row is saying.
        let room = (self.screen.height as usize).saturating_sub(editor_h + bar_h + 1);
        let menu_h = if panel.is_some() {
            panel_h.min(room)
        } else if menu.is_empty() {
            0
        } else {
            menu.len().min(room)
        };
        // Every pinned row, the bar's included: this is what `Fill(1)` will
        // be left with, and `Rows` fills top-down — a row over that count is
        // dropped off the bottom, where the newest one is.
        let hist_view = (self.screen.height as usize)
            .saturating_sub(editor_h + menu_h + bar_h)
            .max(1);
        // The summary row the calls in flight fold into, when it is the last
        // thing in the scrollback: its line is where they show, so they are
        // its to draw and the live block leaves them alone.
        let last = view.surface.scrollback.len().checked_sub(1);
        let row_holds = view
            .surface
            .scrollback
            .last()
            .is_some_and(Row::is_tools_summary);
        let held = if row_holds {
            tool::held(&view.state.tools)
        } else {
            Vec::new()
        };
        for (idx, row) in view.surface.scrollback.iter_mut().enumerate() {
            // Only the last row takes them, and only the last row can be the
            // summary they belong to: any other is handed nothing.
            let flight: &[PendingTool] = if Some(idx) == last { &held } else { &[] };
            row.update_live(self.hovered_scrollback == Some(idx), self.spinner, flight);
        }
        let (live, pending_rows) = self.live(lane, view, row_holds);

        // While the view is scrolled up, rows the bottom gained fold back
        // into `scroll` — a sum of per-row cached heights, where a wrap counts
        // for exactly the rows it takes. Measured in rows, not lines: a line
        // wider than the terminal is several rows, and counting lines here
        // would put more rows in the area than fit — pushing the newest ones
        // off the bottom, underneath the input, where nothing shows them.
        if view.surface.scroll > 0 {
            let total = view
                .surface
                .scrollback
                .iter()
                .filter(|r| keep(r))
                .map(|r| r.height(&self.paint, &self.done, width))
                .sum::<usize>()
                + live.len();
            view.surface.scroll = absorb_growth(view.surface.scroll, view.surface.counted, total);
            view.surface.counted = Some(total);
        } else {
            view.surface.counted = None;
        }
        let scrollback = ScrollbackRows::new(
            &view.surface.scrollback,
            &self.paint,
            &self.done,
            width,
            keep,
        )
        .indexed()
        .map(|(item, idx)| (item, Target::Scrollback(idx)));

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

        let (tagged_rows, scroll) = screen::window_tagged(
            scrollback.chain(live_stream),
            hist_view,
            view.surface.scroll,
        );

        let (rows, row_targets): (Vec<Line<'static>>, Vec<Target>) =
            tagged_rows.into_iter().unzip();
        self.row_targets = row_targets;

        view.surface.scroll = scroll;
        let items = self.menu_items(&menu);
        let picked = self
            .picked
            .unwrap_or(menu.len().saturating_sub(1))
            .min(menu.len().saturating_sub(1));
        let highlight = self.rat_style(&self.paint.theme.menu.selected);
        let _ = self.screen.draw(|frame| {
            // The input line is last, so the caret sits on the bottom row and
            // the bar reads as the edge of the history above it rather than
            // as something hanging off the line being typed.
            let regions =
                Regions::layout(frame.area(), menu_h as u16, bar_h as u16, editor_h as u16);
            self.regions = regions;
            frame.render_widget(Rows(&rows), regions.history);
            if let Some((panel, _)) = &panel {
                frame.render_widget(Rows(panel), regions.menu);
            } else if !items.is_empty() {
                let mut state = ListState::default();
                state.select(Some(picked));
                frame.render_stateful_widget(
                    List::new(items).highlight_style(highlight),
                    regions.menu,
                    &mut state,
                );
            }
            frame.render_widget(Rows(std::slice::from_ref(&bar)), regions.bar);
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
            if let Some((_, Some((row, col)))) = panel {
                if (row as usize) < menu_h {
                    frame.set_cursor_position((regions.menu.x + col, regions.menu.y + row));
                }
            } else if self.panel.is_none() && !self.browsing {
                let caret_row = regions.editor.y + caret_in_view as u16;
                frame.set_cursor_position((caret.1, caret_row));
            }
        });
    }

    // Rebuild the history from the transcript, forgetting everything the old
    // drawing showed: a rewind changes what the conversation is, and the
    // screen has to show the new one, not the old one with a note on it.
    pub(super) fn rebuild(&mut self, view: &mut View, session: &agent::session::Session) {
        let folded = view.surface.folds.folded;
        view.surface = Surface::from(session, &self.paint, folded);
    }
}
