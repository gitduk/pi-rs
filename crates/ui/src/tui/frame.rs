//! One frame: the history window over the transcript or the page that is
//! up, the menu, the bar and the editor, laid bottom-up and drawn.

use pi_core::core::lane::Lane;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, TableState};

use super::Ui;
use super::bar::Facts;
use super::call::{self, drawn, pending_line};
use super::menu::MenuEntry;
use super::mouse::{Regions, Target, row_in};
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
        self.pin_selection(view);
        self.repaint(view, lane);
        if self.background_focus.is_some() && (self.reading() || !self.menu().is_empty()) {
            self.leave_background();
        }
        let width = self.screen.width();
        let menu = self.menu();
        let rewind = self.rewind_follow(view, &menu);
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
        let (live, live_targets) = self.live(lane, view, row_holds, now);

        let hovered_command = self.pointer.and_then(|(_, row)| {
            (self
                .command_at(row)
                .map(call::CommandRow::BackgroundCommand))
            .or(self.earlier_at(row).map(call::CommandRow::Earlier))
            .or(self.fold_at(row).then_some(call::CommandRow::Fold))
        });
        let open = self.background_open.as_ref();
        let (mut executions, mut command_rows) = call::background_lines(
            &self.executions,
            hovered_command,
            open,
            self.background_all || self.background_focus.is_some(),
            &self.paint,
            width,
        );
        for (line, row) in executions.iter_mut().zip(&command_rows) {
            if matches!(row, call::CommandRow::BackgroundCommand(id) if Some(*id) == self.background_focus)
            {
                line.style = line.style.add_modifier(ratatui::style::Modifier::REVERSED);
            }
        }
        let bar_rows = self.bar_lines(&Facts::of(lane), &snapshot(lane, view), width);
        let mut bar: Vec<_> = bar_rows.iter().map(|row| row.line.clone()).collect();
        let (menu, hint) = self.assistance().into_parts();
        let (input, caret) = self.input_view(width, hint.as_deref());
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
            paged,
            asked.len(),
            menu.len(),
            executions.len(),
            bar.len(),
            input.len(),
        );
        for row in view.surface.scrollback.iter() {
            row.prepare_preview(&self.paint, width, regions.history.height as usize);
        }
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
                        .map(|(n, l)| Laid::of(Spot::Text(n), screen::Piece::pieces(l.clone()))),
                )
                .collect();
            self.search_answer(view, &laid);
        }
        asked.truncate(regions.asked.height as usize);
        // A hidden selection must never receive destructive keys.
        let focus_row = self.background_focus.and_then(|id| {
            command_rows
                .iter()
                .position(|row| *row == call::CommandRow::BackgroundCommand(id))
        });
        let command_top = focus_row.map_or(0, |at| {
            let id = self.background_focus.expect("a focused row");
            let end = command_rows
                .iter()
                .enumerate()
                .skip(at + 1)
                .find(|(_, row)| {
                    matches!(row, call::CommandRow::BackgroundCommand(other) if *other != id)
                        || **row == call::CommandRow::Fold
                })
                .map_or(command_rows.len(), |(at, _)| at);
            end.saturating_sub(regions.executions.height as usize)
                .min(at)
        });
        executions.drain(..command_top.min(executions.len()));
        command_rows.drain(..command_top.min(command_rows.len()));
        executions.truncate(regions.executions.height as usize);
        command_rows.truncate(regions.executions.height as usize);
        let command_caret = focus_row.and_then(|at| {
            let row = at.saturating_sub(command_top);
            (row < regions.executions.height as usize)
                .then_some((regions.executions.x, regions.executions.y + row as u16))
        });
        self.command_rows = command_rows;
        if self.background_focus.is_some() && command_caret.is_none() {
            self.leave_background();
        }
        bar.truncate(regions.bar.height as usize);
        // A paste taller than the terminal keeps the caret's row in view.
        let editor_h = regions.editor.height as usize;
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let input_view: Vec<Line<'static>> =
            input.into_iter().skip(editor_top).take(editor_h).collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        let hist_view = regions.history.height as usize;
        let most = self.most(&text, hist_view);
        self.search_land(view, &regions);
        if let Some(at) = rewind {
            self.land_at(view, at, &regions);
        }
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
                for span in &mut line.line.spans {
                    span.style = render::Paint::lit(span.style);
                }
            }
            (Piece::Live(line), Target::Text(at))
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

        let live_stream = live.iter().enumerate().map(move |(i, s)| {
            let target = live_targets[i];
            // In hand already, and the window takes it as it is.
            (
                Piece::Live(screen::Ready {
                    line: s.clone(),
                    width,
                    prefix: None,
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
            rows.push((Line::default(), Target::Rule, 0));
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
            let target = match target {
                Target::Scrollback(idx, line)
                    if at >= view.surface.scrollback[idx].line_height(line, &self.paint, width) =>
                {
                    Target::Separator(idx, line)
                }
                target => target,
            };
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
        let [_, (_, commands_base), (_, bar_base)] = regions.readable();
        self.drawn = rows.iter().map(plain).collect();
        self.drawn.resize(commands_base, String::new());
        self.drawn.extend(executions.iter().map(plain));
        self.drawn.resize(bar_base, String::new());
        self.drawn.extend(bar.iter().map(plain));
        self.regions = regions;
        self.reflow_selection(view, width);
        self.drawn_width = width;
        let stamps = self.sent_stamps(view, regions.history);
        let (rows, executions, bar) = match &self.selection {
            Some(sel) => (
                super::select::highlight(rows, sel, 0, |at| self.lead(view, at)),
                super::select::highlight(executions, sel, commands_base, |_| 0),
                super::select::highlight(bar, sel, bar_base, |_| 0),
            ),
            None => (rows, executions, bar),
        };

        self.below = if self.background_focus.is_some() {
            0
        } else {
            scroll
        };
        let gap = if self.background_focus.is_none() && scroll > 0 {
            let shine = self
                .pointer
                .is_some_and(|(_, row)| row_in(regions.gap, row).is_some());
            let more = format!("{scroll} more below");
            Some(Line::from(self.paint.action_hint(shine, more)).centered())
        } else {
            None
        };

        // Scrolled up, or on a hit, the next frame opens on this one's top
        // row: a hit stays where it was found as the rows under it grow.
        let on_hit = self.search_query().is_some_and(|(_, at)| at.is_some());
        let top = (scroll > 0 || on_hit)
            .then(|| self.anchor_here(view))
            .flatten();
        let (was, top_was) = self.window_mut(view);
        (*was, *top_was) = (scroll, top);
        let picked = self.list.highlighted(&menu);
        let table = self.menu_table(&menu, picked, regions.menu.width as usize);
        // The picked row on the input's band, the menu's width: a bar the eye
        // finds at once, where a mark in its text alone was not enough. Only
        // a row enter takes wears it; one tab alone takes shows by its name.
        let moved = self.list.picked.is_some();
        let cursor = (menu.get(picked))
            .filter(|e| e.taken_by_enter(moved))
            .and_then(|_| self.paint.band(&self.paint.theme.prompt.panel.input))
            .map(|band| band.patch(self.rat_style(&self.paint.theme.menu.selected)))
            .unwrap_or_default();

        self.more_hits = self.more_actions(view, regions);
        let flash_body = self.flash_hit.as_ref().and_then(|(at, hit)| {
            if *at >= regions.bar.height as usize {
                return None;
            }
            let (line, areas) = hit.more.as_ref()?;
            let body = Rect {
                x: areas.body.x + regions.bar.x,
                y: regions.bar.y + *at as u16,
                ..areas.body
            };
            Some((line.clone(), body))
        });
        let menu_rule = self.over_rule(false);
        let page_rule = self.over_rule(ruled && self.paint.color);
        let rule_areas: Vec<_> = regions
            .history
            .rows()
            .zip(&self.row_targets)
            .filter_map(|(area, target)| matches!(target, Target::Rule).then_some(area))
            .collect();
        let _ = self.screen.draw(|frame| {
            // Laid out for the size last reported: a frame of another would
            // draw off its edge, and the resize on its way redraws.
            if frame.area() != area {
                return;
            }
            frame.render_widget(Rows(&rows), regions.history);
            for (area, line) in &stamps {
                frame.render_widget(line, *area);
            }
            for &area in &rule_areas {
                frame.render_widget(&page_rule, area);
            }
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
            if let Some(gap) = &gap {
                frame.render_widget(gap, regions.gap);
            }
            frame.render_widget(Rows(&executions), regions.executions);
            if regions.overlay().height > 0 {
                frame.render_widget(ratatui::widgets::Clear, regions.overlay());
                frame.render_widget(Rows(&asked), regions.asked);
                frame.render_widget(&menu_rule, regions.rule);
                let list = regions.menu;
                // Kept full: ratatui won't pull back an offset that a taller
                // screen or a changed list left past the end.
                let room = list.height as usize;
                let padding = MENU_PADDING.min(room / 2);
                let top = self
                    .list
                    .top
                    .min(picked.saturating_sub(padding))
                    .max((picked + padding + 1).saturating_sub(room))
                    .min(menu.len().saturating_sub(room));
                let mut state = TableState::default()
                    .with_offset(top)
                    .with_selected(Some(picked));
                frame.render_stateful_widget(table.row_highlight_style(cursor), list, &mut state);
                self.list.top = state.offset();
            }
            frame.render_widget(Rows(&bar), regions.bar);
            if self.selection.is_none() {
                for (area, row) in regions.bar.rows().zip(&bar_rows) {
                    for (part, line) in &row.parts {
                        let part = Rect {
                            x: area.x + part.x,
                            y: area.y,
                            ..*part
                        };
                        frame.render_widget(ratatui::widgets::Clear, part);
                        frame.render_widget(line, part);
                    }
                }
            }
            if let Some((line, body)) = &flash_body {
                frame.render_widget(ratatui::widgets::Clear, *body);
                frame.render_widget(line, *body);
            }
            for &action in &self.more_hits {
                if matches!(
                    self.row_targets
                        .get(action.y.saturating_sub(regions.history.y) as usize),
                    Some(Target::Scrollback(..))
                ) && regions.history.contains((action.x, action.y).into())
                {
                    continue;
                }
                let hovered = self
                    .pointer
                    .is_some_and(|(col, row)| action.contains((col, row).into()));
                frame.render_widget(ratatui::widgets::Clear, action);
                frame.render_widget(self.paint.action_hint(hovered, super::browse::MORE), action);
            }
            // Before the rows, so a span over the band keeps it: the columns the
            // input does not reach carry it too.
            if let Some(band) = self.band {
                frame.buffer_mut().set_style(regions.editor, band);
            }
            frame.render_widget(Rows(&input_view), regions.editor);
            let caret_row = regions.editor.y + caret_in_view as u16;
            frame.set_cursor_position(command_caret.unwrap_or((caret.1, caret_row)));
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

    fn more_actions(&self, view: &View, regions: Regions) -> Vec<Rect> {
        let mut row_actions = Vec::new();
        let mut actions = Vec::new();
        let mut add = |region: Rect, at: usize| {
            if at >= region.height as usize {
                return;
            }
            let area = Rect {
                y: region.y + at as u16,
                height: 1,
                ..region
            };
            if area.intersection(regions.overlay()).is_empty()
                && let Some(areas) = super::browse::MoreAreas::of(area)
            {
                actions.push(areas.action);
            }
        };
        if !self.reading() {
            for (at, target) in self.row_targets.iter().enumerate() {
                match target {
                    Target::Scrollback(idx, line)
                        if self.row_of.get(at) == Some(&0)
                            && view.surface.scrollback[*idx]
                                .has_more(*line, self.screen.width()) =>
                    {
                        let (text, border) = view.surface.scrollback[*idx].line(
                            *line,
                            &self.paint,
                            self.screen.width(),
                        );
                        let start = border.as_ref().map_or(0, Line::width);
                        let text = super::screen::plain(&text);
                        let indent = text.len() - text.trim_start().len();
                        let action = Rect::new(
                            regions.history.x + (start + indent) as u16,
                            regions.history.y + at as u16,
                            pi_store::text::width(text.trim_start()) as u16,
                            1,
                        )
                        .intersection(regions.history);
                        if action.intersection(regions.overlay()).is_empty() {
                            row_actions.push(action);
                        }
                    }
                    Target::BackgroundResult(..) => {
                        add(
                            Rect {
                                width: regions.history.width.saturating_sub(1),
                                ..regions.history
                            },
                            at,
                        );
                    }
                    _ => {}
                }
            }
            for (at, target) in self.command_rows.iter().enumerate() {
                let opens = match target {
                    call::CommandRow::Earlier(_) => true,
                    call::CommandRow::BackgroundCommand(id) => {
                        !self
                            .background_open
                            .as_ref()
                            .is_some_and(|open| open.id == *id)
                            && self.executions.iter().any(|execution| {
                                execution.id == *id
                                    && call::background_opens(execution, self.screen.width())
                            })
                    }
                    call::CommandRow::Under | call::CommandRow::Fold => false,
                };
                if opens {
                    add(
                        Rect {
                            width: regions.executions.width.saturating_sub(1),
                            ..regions.executions
                        },
                        at,
                    );
                }
            }
        }
        if let Some((at, hit)) = &self.flash_hit
            && *at < regions.bar.height as usize
            && let Some((_, areas)) = &hit.more
        {
            actions.push(Rect {
                x: areas.action.x + regions.bar.x,
                y: regions.bar.y + *at as u16,
                ..areas.action
            });
        }
        actions.extend(row_actions);
        actions
    }

    // A prompt's send time in its first screen row's free end, added after
    // `drawn` is read so selection and search never take it for text.
    fn sent_stamps(&self, view: &View, region: Rect) -> Vec<(Rect, Line<'static>)> {
        let now = agent::session::now();
        region
            .rows()
            .zip(&self.row_targets)
            .enumerate()
            .filter_map(|(at, (area, target))| {
                let Target::Scrollback(idx, _) = target else {
                    return None;
                };
                if self.row_of.get(at) != Some(&0) {
                    return None;
                }
                let row = view.surface.scrollback.get(*idx)?;
                let time = render::clock(row.sent()?, now);
                if time.is_empty() {
                    return None;
                }
                let used = Line::raw(self.drawn[at].trim_end()).width() as u16;
                let span = self.paint.span(&self.paint.theme.muted, time);
                let wanted = span.width() as u16;
                let [body, gap, _, stamp] = Layout::horizontal([
                    Constraint::Length(used),
                    Constraint::Length(1),
                    Constraint::Fill(1),
                    Constraint::Length(wanted),
                ])
                .areas(area);
                if body.width != used || gap.width != 1 || stamp.width != wanted {
                    return None;
                }
                let band = row.band(&self.paint).unwrap_or_default();
                Some((stamp, Line::from(span).style(band)))
            })
            .collect()
    }

    fn over_rule(&self, lit: bool) -> Block<'static> {
        let color = &self.paint.theme.prompt.color;
        let mut block = Block::default()
            .borders(Borders::TOP)
            .border_style(self.rat_style(color))
            .border_set(ratatui::symbols::border::Set {
                horizontal_top: icons::OVER_RULE,
                ..ratatui::symbols::border::PLAIN
            });
        if lit {
            block = block
                .title_top(Line::from(self.paint.action_hint(true, " click to close ")).centered());
        }
        block
    }

    pub(super) fn input_view(
        &self,
        width: usize,
        hint: Option<&str>,
    ) -> (Vec<Line<'static>>, (u16, u16)) {
        if let Some((line, col)) = self.search_line().filter(|_| self.search_typing()) {
            return (vec![line], (0, col));
        }
        let (mut rows, caret) = self.editor.view(&self.paint, width);
        if let Some(hint) = hint {
            let hint = pi_store::text::clip(hint, width.saturating_sub(caret.1 as usize));
            rows[caret.0 as usize]
                .spans
                .push(self.paint.span(&self.paint.theme.muted, hint));
        }
        (rows, caret)
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
    fn page_lines(&self) -> Vec<screen::Ready<'static>> {
        self.page()
            .unwrap_or_default()
            .iter()
            .map(|line| line.ready(self.screen.width()))
            .collect()
    }

    // How far back the window may go. A page's stops at its head: the
    // transcript under it is only what it was opened over.
    fn most(&self, text: &[screen::Ready<'static>], hist_view: usize) -> usize {
        match self.page() {
            Some(_) => {
                let rows: usize = text.iter().map(screen::Piece::height).sum();
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

    pub(super) fn live(
        &self,
        lane: &Lane,
        view: &View,
        row_holds: bool,
        now: std::time::Instant,
    ) -> (Vec<Line<'static>>, Vec<Target>) {
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
            let (mut line, border) = pending_line(now, self.spinner, t, &self.paint);
            line.spans
                .push(self.paint.span(&self.paint.theme.muted, extra));
            pending.push((line, border));
        }
        let mut spacing = super::spacing::Spacing::after(&view.surface.scrollback);
        if !pending.is_empty() && spacing.push(super::spacing::Block::Tools) {
            rows.push(Line::default());
        }
        rows.extend(pending.into_iter().flat_map(|(line, border)| {
            let prefix = render::Prefix::hanging(border);
            screen::wrap_continued(Some(&prefix.first), Some(&prefix.rest), &line, width)
        }));
        // Target indices include wrapped tool rows and their leading gap.
        let pending_rows = rows.len();
        let body = |text: &str| {
            body(
                &view.surface.folds,
                &view.surface.scrollback,
                thinking,
                text,
                width,
                &self.paint,
            )
        };
        // Reasoning streams only its unfinished line; an answer streams
        // whole until it closes, too long to draw anew every frame.
        let content = if thinking {
            body(&view.surface.stream.text)
        } else {
            (view.surface.stream).answer_lines(width, &self.paint, body)
        };
        if !thinking
            && content.first().is_some_and(|line| !line.spans.is_empty())
            && spacing.push(super::spacing::Block::Prose)
        {
            rows.push(Line::default());
        }
        rows.extend(content);

        if lane.is_running() {
            let mut parts = status::parts(&self.status, &snapshot(lane, view), true);
            if let Some(work) = &view.state.work {
                if matches!(work.doing, Doing::Compact) {
                    parts.insert(0, format!("compacting{}", icons::ELLIPSIS));
                }
                // A stopping run says so; an ordinary one needs no word — its
                // ticking clock already says the turn is on.
                if let Some(retry) = &work.retry
                    && now < retry.until
                {
                    parts.push(format!(
                        "retry {} in {}",
                        retry.attempt,
                        llm::figures::elapsed(std::time::Duration::from_secs(
                            (retry.until - now).as_millis().div_ceil(1000).max(1) as u64,
                        ))
                    ));
                }
                if work.stopping {
                    parts.push(format!("stopping{}", icons::ELLIPSIS));
                }
            }
            // `status = []` asks for no line, and a blank row is not none.
            if !parts.is_empty()
                || (view.state.work.is_some()
                    && self.status.contains(&pi_store::status::Segment::Elapsed))
            {
                let line = parts.join(icons::PART_SEP);
                let painted = status::painted(&line, &self.paint);
                let marker = status::breath_style(self.spinner, self.paint.theme.syntax);
                let prefix = render::Prefix::hanging(Line::from(
                    self.paint.span(&marker, format!("{} ", icons::PI_MARK)),
                ));
                rows.extend(screen::wrap_continued(
                    Some(&prefix.first),
                    Some(&prefix.rest),
                    &painted,
                    width,
                ));
            }
        }
        // Typed lines precede execution results to match delivery priority.
        let waiting = |mark: &str, line: &str| {
            let first = line.lines().next().unwrap_or_default();
            let more = if line.trim_end().contains('\n') {
                icons::ELLIPSIS
            } else {
                ""
            };
            let prefix = render::Prefix::hanging(Line::from(
                self.paint.span(&self.paint.theme.muted, icons::bar(mark)),
            ));
            let muted = Line::from(
                self.paint
                    .span(&self.paint.theme.muted, format!("{first}{more}")),
            );
            screen::wrap_continued(Some(&prefix.first), Some(&prefix.rest), &muted, width)
        };
        rows.extend(
            view.queued
                .lock()
                .iter()
                .filter_map(|q| q.line.as_deref())
                .flat_map(|line| waiting(icons::SAID_RULE, line)),
        );
        let mut targets: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                if i < pending_rows && !row.spans.is_empty() {
                    Target::PendingTools(i)
                } else {
                    Target::Live(i)
                }
            })
            .collect();
        let hovered = self.pointer.and_then(|(_, row)| self.target_at(row));
        let (results, result_targets) = call::background_results(
            &self.every_command,
            self.background_results_shown,
            hovered,
            &self.paint,
            width,
            rows.len(),
        );
        rows.extend(results);
        targets.extend(result_targets);
        (rows, targets)
    }

    // `judge #3 · tests failed, now what? · space mark · tab take …`: who
    // asks only when it is an execution, apart from the turn; then the keys that do
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
            Asker::Background { name, .. } => format!("{name}{}", icons::PART_SEP),
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
