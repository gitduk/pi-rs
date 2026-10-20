//! One frame: the history window over the transcript or the page that is
//! up, the menu, the bar and the editor, laid bottom-up and drawn.

use pi_core::core::lane::Lane;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{List, ListState};

use super::Ui;
use super::bar::Facts;
use super::browse::{Page, Read};
use super::call::{self, drawn, pending_line};
use super::mouse::{Regions, Target};
use super::row::{PendingTool, Row, count};
use super::screen::{self, Rows};
use super::scrollback::{Piece, ScrollbackRows, body};
use super::search::{self, Anchor, Laid, Spot};
use super::view::{StreamKind, View, snapshot};
use crate::render;
use crate::status;
use pi_store::icons;

impl Ui {
    pub(super) fn flush(&mut self, lane: &Lane, view: &mut View) {
        let menu = self.menu();
        let width = self.screen.usable();
        let alone = matches!(self.page(), Some(Page::Preview(_)));
        let slash = matches!(self.page(), Some(Page::Slash(_)));
        let keep = self.keeps();
        let text = self.page_lines();
        let hovered_job = self.pointer.and_then(|(_, row)| {
            (self.job_at(row).map(call::JobRow::Job))
                .or(self.earlier_at(row).map(call::JobRow::Earlier))
                .or(self.fold_at(row).then_some(call::JobRow::Fold))
        });
        let open = (self.job_open, self.job_body.as_str());
        let (mut bar, job_rows) = call::job_lines(
            &self.jobs,
            hovered_job,
            open,
            self.jobs_all,
            &self.paint,
            width,
        );
        self.job_rows = job_rows;
        let jobs = bar.len();
        bar.extend(self.bar_lines(&Facts::of(lane), &snapshot(lane, view), width));
        // Counted from the bar region's top, which the job rows lead.
        if let Some((at, ..)) = &mut self.flash_hit {
            *at += jobs;
        }
        // A query being typed has the line's row; otherwise the line keeps it,
        // a page up or not.
        let (input, caret) = match self.search_line().filter(|_| self.search_typing()) {
            Some((line, col)) => (vec![line], (0, col)),
            None => self.editor.view(&self.paint, width),
        };
        let area = self.screen.area();
        let regions = Regions::layout(area, menu.len(), bar.len(), input.len());
        bar.truncate(regions.bar.height as usize);
        let menu_h = regions.menu.height as usize;
        // A paste taller than the terminal keeps the caret's row in view.
        let editor_h = regions.editor.height as usize;
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let mut input_view: Vec<Line<'static>> =
            input.into_iter().skip(editor_top).take(editor_h).collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        let hist_view = regions.history.height as usize;
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

        if self.search_due() {
            let laid: Vec<Laid> =
                ScrollbackRows::new(&view.surface.scrollback, &self.paint, width, keep)
                    .indexed()
                    .map(|(piece, idx)| {
                        let line = match &piece {
                            Piece::Row { line, .. } => *line,
                            Piece::Live(_) => 0,
                        };
                        Laid::of(Spot::Row(idx, line), screen::Piece::pieces(piece))
                    })
                    .chain(
                        live.iter()
                            .enumerate()
                            .map(|(i, l)| Laid::of(Spot::Live(i), screen::fit(l, width))),
                    )
                    .chain(
                        text.iter()
                            .enumerate()
                            .map(|(n, l)| Laid::of(Spot::Text(n), screen::fit(l, width))),
                    )
                    .collect();
            self.search_answer(&laid);
            // The count the answer moved, on the row laid out before it: one
            // row either way, so only what it says changes.
            if let Some((line, _)) = self.search_line()
                && input_view.len() == 1
            {
                input_view = vec![line];
            }
        }
        let most = self.most(&text, hist_view, width);
        let (scroll, top) = {
            let (scroll, top) = self.window_mut(view);
            (*scroll, *top)
        };
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
                Target::PendingTools(i)
            } else {
                Target::Live(i)
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

        let at = top.map(|a| (move |t: &Target| Spot::of(*t).cmp(&a.spot), a.rows));
        let (tagged_rows, scroll) = if slash {
            // The output lies over the transcript's bottom rows: the
            // transcript stays where it stood, not pushed up by it.
            let (text, scroll) = screen::window_tagged(text_stream, hist_view, scroll, at, most);
            let surface = &view.surface;
            let under =
                (surface.top).map(|a| (move |t: &Target| Spot::of(*t).cmp(&a.spot), a.rows));
            let (mut rows, _) = screen::window_tagged(
                scrollback.chain(live_stream),
                hist_view,
                surface.scroll,
                under,
                usize::MAX,
            );
            let room = hist_view.saturating_sub(text.len());
            rows.truncate(room);
            rows.resize(room, (Line::default(), Target::Text(0), 0));
            rows.extend(text);
            (rows, scroll)
        } else if alone {
            // The head closes the page, so it stays on the top rows however
            // far down the rest is read.
            let mut text = text_stream;
            let mut rows: Vec<_> = text.next().map_or_else(Vec::new, |(head, target)| {
                let pieces = screen::Piece::pieces(head).into_iter().enumerate();
                pieces.map(|(at, row)| (row, target, at)).collect()
            });
            let room = hist_view.saturating_sub(rows.len());
            let (rest, scroll) = screen::window_tagged(text, room, scroll, at, most);
            rows.extend(rest);
            (rows, scroll)
        } else {
            screen::window_tagged(
                scrollback.chain(live_stream).chain(text_stream),
                hist_view,
                scroll,
                at,
                most,
            )
        };

        let mut rows = Vec::with_capacity(tagged_rows.len());
        self.row_targets.clear();
        self.row_of.clear();
        for (row, target, at) in tagged_rows {
            rows.push(row);
            self.row_targets.push(target);
            self.row_of.push(at);
        }
        self.search_settle();
        // The query marked on the rows a search reads, a slash page's own.
        let rows = match self.search_query() {
            Some((query, at)) => {
                let places = self.row_targets.iter().zip(&self.row_of);
                rows.into_iter()
                    .zip(places)
                    .map(|(row, (target, &r))| {
                        if slash && !matches!(target, Target::Text(_)) {
                            return row;
                        }
                        let current = at
                            .filter(|h| Spot::of(*target) == h.at.spot && r == h.at.row)
                            .map(|h| h.k);
                        search::marked(row, query, current)
                    })
                    .collect()
            }
            None => rows,
        };
        let plain = |l: &Line<'_>| l.spans.iter().map(|s| s.content.as_ref()).collect();
        // The bar's rows are selected under the history's, at its full height.
        let base = regions.history.height as usize;
        self.drawn = rows.iter().map(plain).collect();
        self.drawn.resize(base, String::new());
        self.drawn.extend(bar.iter().map(plain));
        let (rows, bar) = match &self.selection {
            Some(sel) => (
                super::select::highlight(rows, sel, 0, |at| self.lead(view, at)),
                super::select::highlight(bar, sel, base, |_| 0),
            ),
            None => (rows, bar),
        };

        let below = (scroll > 0).then(|| {
            let shine = self
                .pointer
                .is_some_and(|(_, row)| (regions.gap.y..regions.gap.bottom()).contains(&row));
            let more = format!("{scroll} more below");
            Line::from(
                self.paint
                    .span_hovered(shine, &self.paint.theme.muted, more),
            )
            .centered()
        });
        self.below = scroll;

        // Scrolled up, the next frame opens on this one's top row.
        let top = (scroll > 0).then(|| self.anchor_here(view)).flatten();
        let (was, top_was) = self.window_mut(view);
        (*was, *top_was) = (scroll, top);
        let picked = self
            .picked
            .unwrap_or(menu.len().saturating_sub(1))
            .min(menu.len().saturating_sub(1));
        let items = self.menu_items(&menu, picked, regions.menu.width as usize);
        // The picked row on the input's band, the menu's width: a bar the eye
        // finds at once, where a mark in its text alone was not enough.
        let cursor = self
            .paint
            .band(&self.paint.theme.prompt.panel.input)
            .unwrap_or_default();
        self.regions = regions;
        let _ = self.screen.draw(|frame| {
            // Laid out for the size last reported: a frame of another would
            // draw off its edge, and the resize on its way redraws.
            if frame.area() != area {
                return;
            }
            frame.render_widget(Rows(&rows), regions.history);
            // While the keys are a page's, the transcript under it steps back so
            // the two do not read as one; a menu that comes with typing does not.
            let dim = ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM);
            if slash {
                // The transcript the output lies over steps back.
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
            // Under the list, which may lie over the gap row.
            if let Some(below) = &below {
                frame.render_widget(below, regions.gap);
            }
            if !items.is_empty() {
                // Kept full: ratatui won't pull back an offset that a taller
                // screen or a changed list left past the end.
                let mut state = ListState::default()
                    .with_offset(self.menu_top.min(menu.len() - menu_h))
                    .with_selected(Some(picked));
                frame.render_widget(ratatui::widgets::Clear, regions.menu);
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
            let caret_row = regions.editor.y + caret_in_view as u16;
            frame.set_cursor_position((caret.1, caret_row));
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

    // The search's row, and the column the caret stands at after the query.
    fn search_line(&self) -> Option<(Line<'static>, u16)> {
        let (query, count) = self.search_prompt()?;
        let col = unicode_width::UnicodeWidthStr::width(query.as_str()) as u16;
        let line = Line::from(vec![
            query.into(),
            self.paint.span(&self.paint.theme.muted, count),
        ]);
        Some((line, col))
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

    // The page's own lines: a command's, set off from the transcript above it
    // by a blank row, or a preview's without what a click on one copies.
    fn page_lines(&self) -> Vec<Line<'static>> {
        match &self.read {
            Some(Read {
                page: Page::Slash(lines),
                ..
            }) => std::iter::once(Line::default())
                .chain(lines.iter().cloned())
                .collect(),
            Some(Read {
                page: Page::Preview(lines),
                ..
            }) => lines.iter().map(|l| l.line.clone()).collect(),
            _ => Vec::new(),
        }
    }

    // How far back the window may go. A slash page's stops at its head:
    // the transcript under it is only what it was asked over.
    fn most(&self, text: &[Line], hist_view: usize, width: usize) -> usize {
        match self.page() {
            Some(Page::Slash(_)) => {
                let rows: usize = text.iter().map(|l| screen::rows(l, width)).sum();
                rows.saturating_sub(hist_view)
            }
            _ => usize::MAX,
        }
    }

    // The scrollback rows the window reads: browsing, the conversation alone
    // (no thinking, calls, notices); a preview, none of the transcript.
    pub(super) fn keeps(&self) -> impl Fn(&Row) -> bool + Copy + use<> {
        let browse = self.browsing();
        let alone = matches!(self.page(), Some(Page::Preview(_)));
        move |row: &Row| !alone && (!browse || row.is_conversation())
    }

    // Where the top row last drawn opens the next window. A live row is drawn
    // anew and lands as another, so the last settled line holds it instead.
    pub(super) fn anchor_here(&self, view: &View) -> Option<Anchor> {
        let place = self.top_place()?;
        let Spot::Live(i) = place.spot else {
            return Some(place.into());
        };
        let keep = self.keeps();
        let (idx, row) = (view.surface.scrollback.iter().enumerate())
            .rev()
            .find(|(_, r)| keep(r) && r.len() > 0)?;
        let line = row.len() - 1;
        let height = row.line_height(line, &self.paint, self.screen.usable());
        Some(Anchor {
            spot: Spot::Row(idx, line),
            rows: (height + i) as isize,
        })
    }

    // Rows above the input: calls in flight the group isn't drawing, the
    // open stream, the status line. Includes the count leading pending calls.
    pub(super) fn live(
        &self,
        lane: &Lane,
        view: &View,
        row_holds: bool,
        now: std::time::Instant,
    ) -> (Vec<Line<'static>>, usize) {
        let width = self.screen.usable();
        let mut rows: Vec<Line<'static>> = Vec::new();
        // Browse mode shows the conversation and nothing that happened on the
        // way to it: the calls in flight, the thinking and the spinner all go.
        let thinking = view.surface.stream.kind == StreamKind::Reasoning;

        // Calls kept here: a foldable one doesn't show while the group
        // above holds it. Collapsed: newest named with a count for the rest.
        let shown = drawn(&view.state.tools, row_holds);
        let mut pending = Vec::new();
        if !self.browsing() {
            if self.live_tools_shown {
                pending.extend(
                    shown
                        .iter()
                        .copied()
                        .map(|t| pending_line(now, self.spinner, t, &self.paint)),
                );
            } else if let Some(t) = shown.last() {
                let extra = if shown.len() > 1 {
                    format!("{}{}", icons::PART_SEP, count(shown.len(), "call"))
                } else {
                    String::new()
                };
                let mut line = pending_line(now, self.spinner, t, &self.paint);
                line.spans
                    .push(self.paint.span(&self.paint.theme.muted, extra));
                pending.push(line);
            }
        }
        rows.extend(
            pending
                .into_iter()
                .flat_map(|line| screen::fit(&line, width)),
        );
        // The draw tags screen rows by index, and a long summary wraps:
        // count the rows the block takes, not the lines before they did.
        let pending_rows = rows.len();

        rows.extend(if thinking && self.browsing() {
            Vec::new()
        } else {
            body(
                &view.surface.folds,
                &view.surface.scrollback,
                thinking,
                &view.surface.stream.text,
                width,
                &self.paint,
            )
        });

        if lane.is_running() && !self.browsing() {
            let mut parts = status::parts(&self.status, &snapshot(lane, view));
            if view.state.compacting {
                parts.insert(0, format!("compacting{}", icons::ELLIPSIS));
            }
            // A stopping run says so; an ordinary one needs no word — its
            // ticking clock already says the turn is on.
            parts.extend(view.state.retry.clone());
            if view.state.stopping {
                parts.push(format!("stopping{}", icons::ELLIPSIS));
            }
            // `status = []` asks for no line, and a blank row is not none.
            if !parts.is_empty() {
                let line = parts.join(icons::PART_SEP);
                let muted = Line::from(self.paint.span(&self.paint.theme.muted, line));
                rows.extend(screen::fit(&muted, width));
            }
        }
        // Typed lines waiting their turn, muted under the said rule: off the
        // transcript until they start, and up takes them back.
        if !self.browsing() {
            for line in view.queued.iter().filter_map(|q| q.line.as_deref()) {
                let first = line.lines().next().unwrap_or_default();
                let more = if line.trim_end().contains('\n') {
                    icons::ELLIPSIS
                } else {
                    ""
                };
                let text = format!("{} {first}{more}", icons::SAID_RULE);
                let muted = Line::from(self.paint.span(&self.paint.theme.muted, text));
                rows.extend(screen::fit(&muted, width));
            }
        }

        (rows, pending_rows)
    }
}
