//! One frame: the history window over the transcript or the page that is
//! up, the menu, the bar and the editor, laid bottom-up and drawn.

use pi_core::core::lane::Lane;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{List, ListState};

use super::Ui;
use super::bar::Facts;
use super::call::{self, drawn, pending_line};
use super::menu::MenuEntry;
use super::mouse::{Regions, Target};
use super::row::{PendingTool, Row, count};
use super::screen::{self, Rows};
use super::scrollback::{Piece, ScrollbackRows, body};
use super::search::{self, Anchor, Laid, Spot};
use super::view::{Doing, StreamKind, View, snapshot};
use crate::render;
use crate::status;
use pi_core::core::question::{Asker, Question};
use pi_store::icons;
use pi_store::keys::{Action, Layers};

// Rows kept between the pick and the list's edge while it scrolls, so what
// comes next is in sight before the pick reaches it.
const MENU_PADDING: usize = 2;

impl Ui {
    pub(super) fn flush(&mut self, lane: &Lane, view: &mut View) {
        let width = self.screen.width();
        let paged = self.read.is_some();
        let text = self.page_lines();
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

        // What the keys asked is answered before anything is laid out, so the
        // list, the bar and the line are drawn as the answer left them.
        if self.search_due() {
            let laid: Vec<Laid> = ScrollbackRows::new(&view.surface.scrollback, &self.paint, width)
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
            self.search_answer(view, &laid);
        }
        let hovered_job = self.pointer.and_then(|(_, row)| {
            (self.job_at(row).map(call::JobRow::Job))
                .or(self.earlier_at(row).map(call::JobRow::Earlier))
                .or(self.fold_at(row).then_some(call::JobRow::Fold))
        });
        let open = self.job_open.as_ref();
        let (mut jobs, job_rows) = call::job_lines(
            &self.jobs,
            hovered_job,
            open,
            self.jobs_all,
            &self.paint,
            width,
        );
        self.job_rows = job_rows;
        let mut bar = self.bar_lines(&Facts::of(lane), &snapshot(lane, view), width);
        // A query being typed is the line's text, drawn as the search's row:
        // `/`, the query, which hit of how many.
        let (input, caret) = match self.search_line().filter(|_| self.search_typing()) {
            Some((line, col)) => (vec![line], (0, col)),
            None => self.editor.view(&self.paint, width),
        };
        let menu = self.menu();
        // A question heads its options, and stays up over words that match
        // none of them, since those answer it too; a command's list hides it.
        let picking = menu.iter().all(|e| matches!(e, MenuEntry::Pick { .. }));
        let mut asked: Vec<Line<'static>> = match self.questions.first() {
            Some(q) if picking && !self.reading() => {
                let layers = self.layers_with(lane.is_running(), !menu.is_empty());
                screen::fit(&self.question_head(q, width, layers), width)
            }
            _ => Vec::new(),
        };
        let area = self.screen.area();
        let regions = Regions::layout(
            area,
            asked.len(),
            menu.len(),
            jobs.len(),
            bar.len(),
            input.len(),
        );
        asked.truncate(regions.asked.height as usize);
        jobs.truncate(regions.jobs.height as usize);
        bar.truncate(regions.bar.height as usize);
        // A paste taller than the terminal keeps the caret's row in view.
        let editor_h = regions.editor.height as usize;
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let input_view: Vec<Line<'static>> =
            input.into_iter().skip(editor_top).take(editor_h).collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        let hist_view = regions.history.height as usize;
        let most = self.most(&text, hist_view, width);
        self.search_land(view, &regions);
        let (scroll, top) = {
            let (scroll, top) = self.window_mut(view);
            (*scroll, *top)
        };
        // A line a click answers on is lit under the pointer, as a fold's
        // own line is.
        let lit = self.lit_text();
        let ruled = self.on_closer();
        let bold = lit.filter(|_| self.paint.color);
        let text_stream = text.into_iter().enumerate().map(move |(at, mut line)| {
            if bold == Some(at) {
                for span in &mut line.spans {
                    span.style = render::Paint::lit(span.style);
                }
            }
            (Piece::Live(screen::Ready { line, width }), Target::Text(at))
        });
        let scrollback = ScrollbackRows::new(&view.surface.scrollback, &self.paint, width)
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

        let at = top.map(|a| (move |t: &Target| Spot::of(*t).cmp(&Some(a.spot)), a.rows));
        // The history row a page's output starts on, the transcript above it.
        let mut output_at = None;
        let (tagged_rows, scroll) = if paged && hist_view > 0 {
            // The page lies over the transcript's bottom rows, under a rule
            // that a page taller than the region pushes up to its top row.
            let (text, scroll) =
                screen::window_tagged(text_stream, hist_view - 1, scroll, at, most);
            let surface = &view.surface;
            let under =
                (surface.top).map(|a| (move |t: &Target| Spot::of(*t).cmp(&Some(a.spot)), a.rows));
            let (mut rows, _) = screen::window_tagged(
                scrollback.chain(live_stream),
                hist_view,
                surface.scroll,
                under,
                usize::MAX,
            );
            let room = (hist_view - 1).saturating_sub(text.len());
            output_at = Some(room);
            rows.truncate(room);
            rows.resize(room, (Line::default(), Target::Gap, 0));
            let rule = self.over_rule(width, ruled && self.paint.color);
            rows.push((rule, Target::Rule, 0));
            rows.extend(text);
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
        // The query marked on the rows a search reads, a page's own.
        let look = self.rat_style(&self.paint.theme.matched);
        let here = self.rat_style(&self.paint.theme.match_current);
        let rows = match self.search_query() {
            Some((query, at)) => {
                let places = self.row_targets.iter().zip(&self.row_of);
                rows.into_iter()
                    .zip(places)
                    .map(|(row, (target, &r))| {
                        if paged && !matches!(target, Target::Text(_)) {
                            return row;
                        }
                        let current = at
                            .filter(|h| Spot::of(*target) == Some(h.at.spot) && r == h.at.row)
                            .map(|h| h.k);
                        search::marked(row, query, current, look, here)
                    })
                    .collect()
            }
            None => rows,
        };
        let plain = |l: &Line<'_>| l.spans.iter().map(|s| s.content.as_ref()).collect();
        // Each readable stretch's rows are selected under the one above, at
        // its full height.
        let [_, (_, jobs_base), (_, bar_base)] = regions.readable();
        self.drawn = rows.iter().map(plain).collect();
        self.drawn.resize(jobs_base, String::new());
        self.drawn.extend(jobs.iter().map(plain));
        self.drawn.resize(bar_base, String::new());
        self.drawn.extend(bar.iter().map(plain));
        let (rows, jobs, bar) = match &self.selection {
            Some(sel) => (
                super::select::highlight(rows, sel, 0, |at| self.lead(view, at)),
                super::select::highlight(jobs, sel, jobs_base, |_| 0),
                super::select::highlight(bar, sel, bar_base, |_| 0),
            ),
            None => (rows, jobs, bar),
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

        // Scrolled up, or on a hit, the next frame opens on this one's top
        // row: a hit stays where it was found as the rows under it grow.
        let on_hit = self.search_query().is_some_and(|(_, at)| at.is_some());
        let top = (scroll > 0 || on_hit)
            .then(|| self.anchor_here(view))
            .flatten();
        let (was, top_was) = self.window_mut(view);
        (*was, *top_was) = (scroll, top);
        let picked = self.list.highlighted(menu.len().max(1));
        let items = self.menu_items(&menu, picked, regions.menu.width as usize);
        // The picked row on the input's band, the menu's width: a bar the eye
        // finds at once, where a mark in its text alone was not enough. Only
        // a row enter takes wears it; one tab alone takes shows by its name.
        let moved = self.list.picked.is_some();
        let cursor = (menu.get(picked))
            .filter(|e| e.taken_by_enter(moved))
            .and_then(|_| self.paint.band(&self.paint.theme.prompt.panel.input))
            .unwrap_or_default();
        self.regions = regions;
        let menu_rule = self.over_rule(width, false);
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
            if let Some(above) = output_at {
                // Up to the rule, which keeps its colour.
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
            frame.render_widget(Rows(&jobs), regions.jobs);
            if regions.rule.height > 0 {
                frame.render_widget(ratatui::widgets::Clear, regions.overlay());
                frame.render_widget(Rows(&asked), regions.asked);
                frame.render_widget(&menu_rule, regions.rule);
                let list = regions.menu;
                // Kept full: ratatui won't pull back an offset that a taller
                // screen or a changed list left past the end.
                let room = list.height as usize;
                let mut state = ListState::default()
                    .with_offset(self.list.top.min(menu.len().saturating_sub(room)))
                    .with_selected(Some(picked));
                frame.render_stateful_widget(
                    List::new(items)
                        .highlight_style(cursor)
                        .scroll_padding(MENU_PADDING),
                    list,
                    &mut state,
                );
                self.list.top = state.offset();
            }
            frame.render_widget(Rows(&bar), regions.bar);
            // Before the rows, so a span over the band keeps it: the columns the
            // input does not reach carry it too.
            if let Some(band) = self.band {
                frame.buffer_mut().set_style(regions.editor, band);
            }
            frame.render_widget(Rows(&input_view), regions.editor);
            let caret_row = regions.editor.y + caret_in_view as u16;
            frame.set_cursor_position((caret.1, caret_row));
        });
        // Bold doesn't move a row, so a second frame settles it.
        let over = self
            .pointer
            .and_then(|(col, row)| self.hovered_row(view, col, row));
        if over != self.hovered_scrollback || self.lit_text() != lit || self.on_closer() != ruled {
            self.hovered_scrollback = over;
            self.redraw = true;
        }
    }

    // The line that parts whatever lies over the transcript from it; `lit`,
    // it says what a click on it does, since a thin line shows no bold.
    fn over_rule(&self, width: usize, lit: bool) -> Line<'static> {
        let color = &self.paint.theme.prompt.color;
        let says = " click to close ";
        let room = width.saturating_sub(pi_store::text::width(says));
        if !lit || room < 2 {
            return Line::from(self.paint.span(color, icons::OVER_RULE.repeat(width)));
        }
        Line::from(vec![
            self.paint.span(color, icons::OVER_RULE.repeat(room / 2)),
            self.paint.span_hovered(true, color, says),
            self.paint
                .span(color, icons::OVER_RULE.repeat(room - room / 2)),
        ])
    }

    // The search's row, and the column the caret stands at in the query.
    fn search_line(&self) -> Option<(Line<'static>, u16)> {
        let (query, count) = self.search_prompt()?;
        let col = pi_store::text::width(&self.search_caret()?) as u16;
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
        self.page_copies(n).map(|_| n)
    }

    // Whether the pointer is where a click closes the page.
    fn on_closer(&self) -> bool {
        self.pointer
            .is_some_and(|(_, row)| self.closes_page_at(row))
    }

    // The page's own lines, without what a click on one copies.
    fn page_lines(&self) -> Vec<Line<'static>> {
        (self.page().unwrap_or_default().iter())
            .map(|l| l.line.clone())
            .collect()
    }

    // How far back the window may go. A page's stops at its head: the
    // transcript under it is only what it was opened over.
    fn most(&self, text: &[Line], hist_view: usize, width: usize) -> usize {
        match self.page() {
            Some(_) => {
                let rows: usize = text.iter().map(|l| screen::rows(l, width)).sum();
                rows.saturating_sub(hist_view.saturating_sub(1))
            }
            None => usize::MAX,
        }
    }

    // Where the top row last drawn opens the next window. A live row is drawn
    // anew and lands as another, so the last settled line holds it instead.
    pub(super) fn anchor_here(&self, view: &View) -> Option<Anchor> {
        let place = self.top_place()?;
        let Spot::Live(i) = place.spot else {
            return Some(place.into());
        };
        let (idx, row) = (view.surface.scrollback.iter().enumerate())
            .rev()
            .find(|(_, r)| r.len() > 0)?;
        let line = row.len() - 1;
        let height = row.line_height(line, &self.paint, self.screen.width());
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
        let width = self.screen.width();
        let mut rows: Vec<Line<'static>> = Vec::new();
        let thinking = view.surface.stream.kind == StreamKind::Reasoning;

        // Calls kept here: a foldable one doesn't show while the group
        // above holds it. Collapsed: newest named with a count for the rest.
        let shown = drawn(&view.state.tools, row_holds);
        let mut pending = Vec::new();
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
        rows.extend(
            pending
                .into_iter()
                .flat_map(|line| screen::fit(&line, width)),
        );
        // The draw tags screen rows by index, and a long summary wraps:
        // count the rows the block takes, not the lines before they did.
        let pending_rows = rows.len();

        rows.extend(body(
            &view.surface.folds,
            &view.surface.scrollback,
            thinking,
            &view.surface.stream.text,
            width,
            &self.paint,
        ));

        if lane.is_running() {
            let mut parts = status::parts(&self.status, &snapshot(lane, view));
            if let Some(work) = &view.state.work {
                if matches!(work.doing, Doing::Compact) {
                    parts.insert(0, format!("compacting{}", icons::ELLIPSIS));
                }
                // A stopping run says so; an ordinary one needs no word — its
                // ticking clock already says the turn is on.
                parts.extend(work.retry.clone());
                if work.stopping {
                    parts.push(format!("stopping{}", icons::ELLIPSIS));
                }
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

        (rows, pending_rows)
    }

    // `judge #3 · tests failed, now what? · space mark · tab take …`: who
    // asks only when it is a job, apart from the turn; then the keys that do
    // each thing with `layers` up, as many as fit beside the question in
    // `width`, the ones nobody would guess first.
    pub(super) fn question_head(
        &self,
        question: &Question,
        width: usize,
        layers: Layers,
    ) -> Line<'static> {
        let theme = &self.paint.theme;
        let key =
            |action| (self.keys.live_keys(action, layers).into_iter().next()).unwrap_or_default();
        let moves = [key(Action::MenuNext), key(Action::MenuPrevious)]
            .into_iter()
            .filter(|k| !k.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        let asker = match &question.from {
            Asker::Job { name, .. } => format!("{name}{}", icons::PART_SEP),
            Asker::Tool { .. } => String::new(),
        };
        let said = format!("{asker}{}", question.text);
        let mut room = width.saturating_sub(pi_store::text::width(&said));
        let hint: String = [
            (key(Action::MenuMark), "mark"),
            (key(Action::MenuAccept), "take"),
            (key(Action::MenuDismiss), "close"),
            (moves, "move"),
            (key(Action::LineSubmit), "send"),
        ]
        .into_iter()
        .filter(|(k, _)| !k.is_empty())
        .map(|(k, what)| format!("{}{k} {what}", icons::PART_SEP))
        .take_while(|part| {
            let fits = pi_store::text::width(part) <= room;
            room = room.saturating_sub(pi_store::text::width(part));
            fits
        })
        .collect();
        Line::from(vec![
            self.paint.span(&theme.muted, asker),
            self.paint.span(&theme.emphasis, question.text.clone()),
            self.paint.span(&theme.muted, hint),
        ])
    }
}
