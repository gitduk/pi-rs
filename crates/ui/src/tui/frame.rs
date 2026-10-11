//! One frame: the history window over the transcript or the page that is
//! up, the menu, the bar and the editor, laid bottom-up and drawn.

use pi_core::core::lane::Lane;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{List, ListState};

use super::Ui;
use super::bar::Facts;
use super::browse::{Page, Read};
use super::call;
use super::mouse::{Regions, Target};
use super::row::{PendingTool, Row};
use super::screen::{self, Rows};
use super::scrollback::{Piece, ScrollbackRows, absorb_growth};
use super::ui::Focus;
use super::view::{View, snapshot};
use crate::render;

impl Ui {
    pub(super) fn flush(&mut self, lane: &Lane, view: &mut View) {
        let menu = self.menu();
        let width = self.screen.usable();
        // The reading page has no editor. Browsing it reads the conversation
        // alone (no thinking, calls, notices); a preview reads none of the
        // transcript. The tally and window share this one predicate.
        let reading = self.reading();
        let browse = self.browsing();
        let alone = matches!(self.page(), Some(Page::Preview(_)));
        let reply = matches!(self.page(), Some(Page::Reply(_)));
        let keep = move |row: &Row| !alone && (!browse || row.is_conversation());
        let text = self.page_lines();
        let mut bar = call::job_lines(&self.jobs, &self.paint, width);
        bar.extend(self.bar_lines(&Facts::of(lane), &snapshot(lane, view), width));
        // The bar's rows are not worth a terminal that cannot hold them, a row
        // to type on and a row of history: one that short keeps the other two.
        bar.truncate((self.screen.height as usize).saturating_sub(2));
        let bar_h = bar.len();
        // Nothing to type on, so nothing to pin to the bottom: the rows the
        // editor would have taken go to the history.
        let (input, caret) = if reading {
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
        // Bottom-up: input pinned, the menu above it, history fills the
        // rest — bar always keeps its row.
        let gap = super::mouse::HISTORY_GAP as usize;
        let room = (self.screen.height as usize).saturating_sub(editor_h + bar_h + 1 + gap);
        let menu_h = if menu.is_empty() {
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
        let (live, pending_rows) = if alone {
            (Vec::new(), 0)
        } else {
            self.live(lane, view, row_holds, now)
        };

        self.absorb(view, &keep, live.len(), width);
        let scroll = self.window_scroll(view, &text, hist_view, width);
        // A line a click answers on is lit under the pointer, as a fold's
        // own line is.
        let lit = self.lit_text();
        let bold = lit.filter(|_| self.paint.color);
        let text_stream = text.into_iter().enumerate().map(move |(at, mut line)| {
            if bold == Some(at) {
                for span in &mut line.spans {
                    span.style = render::Paint::lit(span.style);
                }
            }
            (Piece::Live(screen::Ready { line, width }), Target::Text(at))
        });
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
            scrollback.chain(live_stream).chain(text_stream),
            hist_view,
            scroll,
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

        match &mut self.focus {
            Focus::Read(read) => read.scroll = scroll,
            _ => view.surface.scroll = scroll,
        }
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
            let dim = ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM);
            if matches!(self.focus, Focus::Rewind(_)) {
                frame.buffer_mut().set_style(regions.history, dim);
            } else if reply {
                // The reply comes last, under the transcript.
                let above = self
                    .row_targets
                    .iter()
                    .position(|t| matches!(t, Target::Text(_)))
                    .unwrap_or(self.row_targets.len());
                let transcript = Rect {
                    height: above as u16,
                    ..regions.history
                };
                frame.buffer_mut().set_style(transcript, dim);
            }
            if !items.is_empty() {
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
        if over != self.hovered_scrollback || self.lit_text() != lit {
            self.hovered_scrollback = over;
            self.redraw = true;
        }
    }

    // The line of the page's text under the pointer, by the targets the
    // last frame drew, when a click there does something.
    fn lit_text(&self) -> Option<usize> {
        let (_, row) = self.pointer?;
        let Some(Target::Text(n)) = self.target_at(row) else {
            return None;
        };
        self.text_click(n).map(|_| n)
    }

    // The page's own lines: a reply's, or a preview's without what a click
    // on one copies.
    fn page_lines(&self) -> Vec<Line<'static>> {
        match self.page() {
            Some(Page::Reply(lines)) => lines.clone(),
            Some(Page::Preview(lines)) => lines.iter().map(|(line, _)| line.clone()).collect(),
            _ => Vec::new(),
        }
    }

    // While scrolled up, bottom growth folds into the scroll via cached row
    // heights — counting lines instead would misfill the visible area.
    fn absorb(&mut self, view: &mut View, keep: &dyn Fn(&Row) -> bool, live: usize, width: usize) {
        // Text does not grow. The transcript is left alone under a page: the
        // first frame after it counts what grew meanwhile, as one frame's.
        let scroll = match &self.focus {
            Focus::Read(Read {
                page: Page::Conversation,
                scroll,
                ..
            }) => *scroll,
            Focus::Read(_) => return,
            _ => view.surface.scroll,
        };
        let total = (scroll > 0).then(|| {
            let rows: usize = view
                .surface
                .scrollback
                .iter()
                .filter(|r| keep(r))
                .map(|r| r.height(&self.paint, width))
                .sum();
            rows + live
        });
        let (scroll, counted) = match &mut self.focus {
            Focus::Read(read) => (&mut read.scroll, &mut read.counted),
            _ => (&mut view.surface.scroll, &mut view.surface.counted),
        };
        if let Some(total) = total {
            *scroll = absorb_growth(*scroll, *counted, total);
        }
        *counted = total;
    }

    // The scroll the window reads at. A reply's stops at its head: the
    // transcript above it is only what it was asked over.
    fn window_scroll(&self, view: &View, text: &[Line], hist_view: usize, width: usize) -> usize {
        match &self.focus {
            Focus::Read(Read {
                page: Page::Reply(_),
                scroll,
                ..
            }) => {
                let rows: usize = text.iter().map(|l| screen::fit(l, width).len()).sum();
                (*scroll).min(rows.saturating_sub(hist_view))
            }
            Focus::Read(read) => read.scroll,
            _ => view.surface.scroll,
        }
    }
}
