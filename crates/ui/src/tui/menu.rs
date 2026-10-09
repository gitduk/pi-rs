//! The lists a surface opens over the editor — completions, `/model`, the
//! rewind menu — and the keys that drive them.
use super::mouse::at_row_name;
use super::row::Row;
use super::ui::{Focus, LastPress};
use super::view::View;
use super::{Asked, Deed, Ui};
use super::{DOUBLE_TAP, screen};
use agent::session::EntryId;
use crossterm::event::{Event as TermEvent, KeyEventKind, MouseEventKind};
use pi_core::core;
use pi_core::core::lane::Lane;
use pi_core::input::Builtin;
use pi_core::input::commands::{Candidate, Choice};
use pi_core::input::{self, Intent};
use pi_store::keys::{Action, Layers, Mode, Over, Press};
use pi_store::session::ResumeChoice;
use pi_store::session::Store;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::ListItem;
use std::time::Instant;

impl Ui {
    // What the line could become: a completion while a command word is
    // typed, or (rewind selector open) a message to rewind to.
    pub(super) fn menu(&mut self) -> Vec<MenuEntry> {
        match &self.focus {
            Focus::Rewind(rows) => return rows.clone(),
            Focus::Reply(_) | Focus::Browse => return Vec::new(),
            Focus::Editor => {}
        }
        if self.dismissed_at.as_deref() == Some(self.editor.text()) {
            return Vec::new();
        }
        // An @ token outranks the word completions: it names a path, and the
        // filesystem holds the answer, not the command tables.
        if let Some((start, end, query)) =
            pi_core::input::complete::at_prefix(self.editor.text(), self.editor.cursor())
        {
            // The cache is keyed by the query: every keystroke moves it, but
            // every frame redraws the menu against the same one.
            if self
                .at_menu
                .as_ref()
                .is_none_or(|(q, _)| q.as_str() != query)
            {
                let items = pi_core::input::complete::candidates(query, &self.at_root);
                self.at_menu = Some((query.to_string(), items));
                self.picked = None;
            }
            let (_, items) = self.at_menu.as_ref().expect("set above");
            return items
                .iter()
                .rev()
                .map(|f| MenuEntry::File {
                    start,
                    end,
                    show: at_row_name(&f.path, f.dir),
                    path: f.path.clone(),
                    dir: f.dir,
                })
                .collect();
        }
        // Bottom-up: the best match belongs on the row right above the input.
        pi_core::input::commands::complete(
            self.editor.text(),
            &self.commands,
            &self.choices,
            || self.lists.sessions(),
            || self.lists.worktrees(),
        )
        .into_iter()
        .rev()
        .map(MenuEntry::Completion)
        .collect()
    }

    // Open the rewind selector on the given messages, newest selected first.
    pub(super) fn open_rewind(&mut self, rows: Vec<MenuEntry>) {
        self.picked = Some(rows.len().saturating_sub(1));
        self.focus = Focus::Rewind(rows);
    }

    // The highlighted row, clamped: the list shrinks as the word grows.
    pub(super) fn highlighted(&mut self) -> Option<MenuEntry> {
        let mut menu = self.menu();
        if menu.is_empty() {
            return None;
        }
        let at = self.picked.unwrap_or(menu.len() - 1).min(menu.len() - 1);
        Some(menu.swap_remove(at))
    }

    // Splice the accepted @ path over the token the list grew from. A
    // directory keeps completing: no trailing space, the caret stays on it.
    fn apply_file(&mut self, start: usize, end: usize, path: &str, dir: bool) {
        let token = if dir {
            format!("@{path}/")
        } else {
            format!("@{path} ")
        };
        self.editor.splice(start, end, &token);
        self.picked = None;
    }

    // The menu's rows as ratatui list items. The selected row is styled by
    // the list itself; everything else sits muted.
    pub(super) fn menu_items(&self, menu: &[MenuEntry]) -> Vec<ListItem<'static>> {
        let head = menu
            .iter()
            .map(|c| unicode_width::UnicodeWidthStr::width(c.show()))
            .max()
            .unwrap_or(0);
        let muted = self.rat_style(&self.paint.theme.muted);
        menu.iter()
            .map(|c| {
                let line = format!("  {}  {}", pi_store::text::pad(c.show(), head), c.help());
                ListItem::new(Line::from(Span::styled(line, muted)))
            })
            .collect()
    }

    // The submitted line onto the screen: the row its answer lands under.
    // Called only from `Tui::echo_sent`, which alone knows if there's one.
    pub(super) fn submit(&mut self, view: &mut View, line: &str) {
        let rows = Row::prompt(line, &self.paint);
        view.surface.scrollback.extend(rows);
        view.surface
            .folds
            .fold_previous(&mut view.surface.scrollback);
    }

    pub(super) fn submit_relayed(&mut self, view: &mut View, label: &str, line: &str) {
        let row = Row::relayed(label, line, &self.paint);
        view.surface.scrollback.push(row);
        view.surface
            .folds
            .fold_previous(&mut view.surface.scrollback);
    }

    // Whether a line has been submitted since this was last asked: the surface
    // catches the recall list up when it has.
    pub(super) fn took_submit(&mut self) -> bool {
        std::mem::take(&mut self.submitted)
    }

    pub(super) fn key(
        &mut self,
        lane: &Lane,
        view: &mut View,
        event: TermEvent,
        running: bool,
    ) -> Asked {
        let key = match event {
            TermEvent::Resize(w, h) => {
                self.screen.resized(w, h);
                // Re-measuring starts at the new width: the re-wrap is a
                // change of layout, not output, and must not move the view.
                view.surface.counted = None;
                return Asked::Own(Deed::Nothing);
            }
            TermEvent::Paste(text) => {
                self.last_press = None;
                // Only the line takes text, and only while it has the keys:
                // anywhere else the paste would land where nobody can see it.
                if matches!(self.focus, Focus::Editor) {
                    self.editor.insert_str(&text.replace('\r', "\n"));
                }
                return Asked::Own(Deed::Nothing);
            }
            TermEvent::Mouse(mouse) => return self.mouse(view, mouse),
            // Windows reports both press and release; acting on both would
            // double every keystroke.
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                self.selection = None;
                k
            }
            _ => return Asked::Own(Deed::Nothing),
        };

        let press = Press::of(key.code, key.modifiers);
        let now = Instant::now();
        let last = self.last_press.take();
        let prev = last
            .filter(|l| now.duration_since(l.at) < self.pair_window_after(l.press))
            .map(|l| l.press);
        let layers = self.layers(running);
        let hit = self.keys.hit(prev, press, layers);
        if let Some(h) = hit
            && h.pair
        {
            // The pair's first half typed itself onto the line before anyone
            // knew it was one; take it back, so the line shows what it means.
            if last.is_some_and(|l| l.typed) {
                self.editor.backspace();
            }
        } else {
            // A finished pair starts none: `g g g` is a pair and a `g`.
            self.last_press = Some(LastPress {
                press,
                at: now,
                typed: false,
            });
        }
        let action = hit.map(|h| h.action);

        match action {
            Some(Action::AppCancel) => return self.cancel(view, running),
            Some(Action::AppQuit) => return Asked::Core(Intent::Builtin(Builtin::Quit)),
            _ => {}
        }
        match self.focus {
            Focus::Reply(_) | Focus::Browse => self.pager_key(view, action),
            Focus::Rewind(_) => self.rewind_key(action),
            Focus::Editor => self.editor_key(lane, view, key, action),
        }
    }

    // The layers the key table reads, from who has the keyboard.
    fn layers(&mut self, running: bool) -> Layers {
        let over = match self.focus {
            Focus::Reply(_) | Focus::Browse => Over::Pager,
            Focus::Rewind(_) => Over::Menu,
            Focus::Editor => {
                if self.menu().is_empty() {
                    Over::None
                } else {
                    Over::Menu
                }
            }
        };
        Layers {
            over,
            run: running,
            mode: self.vim,
            // Whether the line has anything on it, which the keys bound
            // to an empty one read. Only Normal asks.
            line_empty: self.editor.is_empty(),
        }
    }

    // How long `press` waits for the second of a pair. A bare character is
    // also text, so its pairs must come quickly or typing would trip them.
    fn pair_window_after(&self, press: Press) -> std::time::Duration {
        if press.is_bare() {
            self.pair_window
        } else {
            DOUBLE_TAP
        }
    }

    // The stop: whatever is over the editor closes, and a run stops. With
    // neither, it says the next one leaves.
    fn cancel(&mut self, view: &mut View, running: bool) -> Asked {
        let closed = self.close_overlay(view);
        if running {
            return Asked::Own(Deed::Interrupt);
        }
        if closed {
            return Asked::Own(Deed::Nothing);
        }
        self.flash("press it again to quit");
        Asked::Own(Deed::Nothing)
    }

    // Back to the line, from whatever has the keyboard; false when the line
    // had it already. The press that closed is not the first of a pair.
    fn close_overlay(&mut self, view: &mut View) -> bool {
        match self.focus {
            Focus::Editor => return false,
            Focus::Browse => self.leave_browse(view),
            Focus::Rewind(_) | Focus::Reply(_) => self.focus = Focus::Editor,
        }
        self.disarm();
        true
    }

    // A press that did its own thing is not the first of a pair: the table
    // pairs by key alone (dismissing `esc`, clearing `ctrl+l`).
    fn disarm(&mut self) {
        self.last_press = None;
    }

    // A reply or the conversation view: the pager keys move it, and
    // everything else is swallowed.
    fn pager_key(&mut self, view: &mut View, action: Option<Action>) -> Asked {
        let (up, step) = match action {
            Some(Action::PagerDown) => (false, Stride::Line),
            Some(Action::PagerUp) => (true, Stride::Line),
            Some(Action::PagerHalfDown) => (false, Stride::Half),
            Some(Action::PagerHalfUp) => (true, Stride::Half),
            Some(Action::PagerPageDown) => (false, Stride::Page),
            Some(Action::PagerPageUp) => (true, Stride::Page),
            Some(Action::PagerBottom) => (false, Stride::End),
            Some(Action::PagerTop) => (true, Stride::End),
            Some(Action::PagerClose) => {
                self.close_overlay(view);
                return Asked::Own(Deed::Nothing);
            }
            _ => return Asked::Own(Deed::Nothing),
        };
        if matches!(self.focus, Focus::Browse) {
            let rows = match step {
                Stride::Line => 1,
                Stride::Half => self.half_scroll_step(),
                Stride::Page => self.page_scroll_step(),
                Stride::End => screen::TOP,
            };
            self.scroll_view(view, up, rows);
            return Asked::Own(Deed::Nothing);
        }
        let room = self.regions.menu.height as usize;
        // Wraps at the drawing's width, not the terminal's: a row that wraps
        // after this count would end up unreachable to scroll to.
        let width = self.screen.usable();
        // The last row of a full window is the count, not the reply.
        let page = room.saturating_sub(1).max(1) as isize;
        let rows = match step {
            Stride::Line => 1,
            Stride::Half => (page / 2).max(1),
            Stride::Page => page,
            Stride::End => isize::MAX,
        };
        self.scrolled(if up { -rows } else { rows }, room, width)
    }

    // The rewind selector: move, pick, or close; nothing else reaches it.
    fn rewind_key(&mut self, action: Option<Action>) -> Asked {
        match action {
            Some(action @ (Action::MenuNext | Action::MenuPrevious)) => self.step_menu(action),
            Some(Action::MenuAccept | Action::LineSubmit) => {
                if let Some(MenuEntry::Message { id, .. }) = self.highlighted() {
                    self.focus = Focus::Editor;
                    return Asked::Own(Deed::To(id));
                }
            }
            Some(Action::MenuDismiss) => {
                self.focus = Focus::Editor;
                self.disarm();
            }
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    fn step_menu(&mut self, action: Action) {
        let n = self.menu().len().saturating_sub(1);
        let at = self.picked.unwrap_or(n).min(n);
        self.picked = Some(if action == Action::MenuNext {
            at.saturating_add(1).min(n)
        } else {
            at.saturating_sub(1)
        });
    }

    // The line has the keys, a completion list perhaps over it.
    fn editor_key(
        &mut self,
        lane: &Lane,
        view: &mut View,
        key: crossterm::event::KeyEvent,
        action: Option<Action>,
    ) -> Asked {
        match action {
            Some(Action::LineSubmit) => {
                // Enter with a menu open runs what's highlighted, the typed
                // text a mere prefix. Pick before draining the editor.
                return match self.highlighted() {
                    Some(MenuEntry::Completion(c)) => {
                        // The completion's line runs; the prefix that
                        // produced it goes, so it can't resubmit as a stray prompt.
                        self.editor.take();
                        self.run_line(view, c.line)
                    }
                    Some(MenuEntry::File {
                        start,
                        end,
                        path,
                        dir,
                        ..
                    }) => {
                        // Enter on a path applies it and stays: the prompt
                        // is not done until the user says so.
                        self.apply_file(start, end, &path, dir);
                        Asked::Own(Deed::Nothing)
                    }
                    Some(MenuEntry::Message { .. }) | None => {
                        let typed = self.editor.take();
                        self.run_line(view, typed)
                    }
                };
            }
            Some(Action::EditExternally) => return Asked::Own(Deed::External),
            Some(Action::PasteImage) => match super::clipboard::paste_image() {
                Ok((path, about)) => {
                    let n = match self.images.iter().position(|p| *p == path) {
                        Some(at) => at + 1,
                        None => {
                            self.images.push(path);
                            self.images.len()
                        }
                    };
                    self.editor.insert_str(&format!("[Image #{n} {about}] "));
                }
                Err(why) => self.flash(why),
            },
            Some(Action::RunInterrupt) => {
                // Esc before the model has moved means "I didn't mean to send
                // that"; an empty editor, or unsending overwrites a line.
                if self.editor.is_empty() && lane.is_running() && !view.state.committed {
                    return Asked::Own(Deed::Unsend);
                }
                return Asked::Own(Deed::Interrupt);
            }
            // Only from an empty line: with text on it, `esc esc` is nothing.
            Some(Action::Rewind) if self.editor.is_empty() => {
                return Asked::Own(Deed::Rewind);
            }
            Some(action @ (Action::LaneNext | Action::LanePrev)) => {
                let forward = action == Action::LaneNext;
                return match self.step_checkout(lane, forward) {
                    Some(name) => Asked::Core(Intent::Builtin(Builtin::Worktree(name))),
                    None => {
                        self.flash("the only checkout there is");
                        Asked::Own(Deed::Nothing)
                    }
                };
            }
            // The session is what a press would replace, so it takes two,
            // and only with nothing on the line.
            Some(Action::SessionNew) if self.editor.is_empty() => {
                return Asked::Core(Intent::Builtin(Builtin::New));
            }
            Some(Action::AppExit) => {
                // No `running` check: leaving is one intent whatever is in
                // flight, and `admit` gives it one answer.
                return if self.editor.is_empty() {
                    Asked::Core(Intent::Builtin(Builtin::Quit))
                } else {
                    self.editor.delete();
                    Asked::Own(Deed::Nothing)
                };
            }
            Some(Action::LineClear) => {
                if !self.editor.is_empty() {
                    self.editor.clear();
                    self.disarm();
                }
            }
            Some(Action::InsertNewline) => self.editor.insert('\n'),
            Some(Action::DeleteCharBack) => self.editor.backspace_tag(),
            Some(Action::DeleteCharForward) => self.editor.delete_tag(),
            Some(Action::DeleteWordBack) => self.editor.kill_word_back(),
            Some(Action::DeleteToLineEnd) => self.editor.kill_to_end(),
            Some(Action::DeleteToLineStart) => self.editor.kill_to_start(),
            Some(Action::DeleteLine) => self.editor.delete_line(),
            Some(Action::MoveCharLeft) => self.editor.left(),
            Some(Action::MoveCharRight) => self.editor.right(),
            Some(Action::MoveWordLeft) => self.editor.word_left(),
            Some(Action::MoveWordRight) => self.editor.word_right(),
            Some(Action::MoveWordNext) => self.editor.word_next(),
            Some(Action::ModeNormal) => self.set_mode(Mode::Normal),
            Some(Action::ModeInsert) => self.leave_normal(),
            Some(Action::ModeInsertAfter) => {
                self.editor.right();
                self.leave_normal();
            }
            Some(Action::ModeInsertLineStart) => {
                self.editor.home();
                self.leave_normal();
            }
            Some(Action::ModeInsertLineEnd) => {
                self.editor.end();
                self.leave_normal();
            }
            Some(Action::ChangeChar) => {
                self.editor.delete();
                self.leave_normal();
            }
            Some(Action::ChangeToLineEnd) => {
                self.editor.kill_to_end();
                self.leave_normal();
            }
            Some(Action::ChangeLine) => self.change_line(),
            Some(Action::OpenLineBelow) => {
                self.editor.open_below();
                self.leave_normal();
            }
            Some(Action::OpenLineAbove) => {
                self.editor.open_above();
                self.leave_normal();
            }
            Some(Action::MoveLineStart) => self.editor.home(),
            Some(Action::MoveLineEnd) => self.editor.end(),
            Some(Action::MoveLineFirstNonBlank) => self.editor.first_non_blank(),
            Some(Action::MoveBufferStart) => self.buffer_ends(view, true),
            Some(Action::MoveBufferEnd) => self.buffer_ends(view, false),
            Some(Action::HistoryOlder) => self.editor.up(),
            Some(Action::HistoryNewer) => self.editor.down(),
            Some(Action::ScrollPageUp) => self.scroll_view(view, true, self.page_scroll_step()),
            Some(Action::ScrollPageDown) => self.scroll_view(view, false, self.page_scroll_step()),
            Some(Action::ScrollHalfUp) => self.scroll_view(view, true, self.half_scroll_step()),
            Some(Action::ScrollHalfDown) => self.scroll_view(view, false, self.half_scroll_step()),
            Some(Action::Browse) => self.browse(view),
            Some(Action::ThinkFold) => {
                // The last group only. The switch is left alone, so the
                // groups no one is touching keep what they had.
                view.surface
                    .folds
                    .toggle_current(&mut view.surface.scrollback);
            }
            Some(Action::ThinkFoldAll) => {
                // Every group in the scrollback, last one included, and
                // the switch with them: one key resets the whole screen.
                view.surface.folds.flip_all(&mut view.surface.scrollback);
                // A fold-all reflows groups above the view too; re-baseline.
                view.surface.counted = None;
            }
            Some(Action::MenuAccept) => match self.highlighted() {
                Some(MenuEntry::Completion(c)) => {
                    self.editor.set_line(&c.line);
                    // Something still expected after it wants a space first.
                    if c.more {
                        self.editor.insert(' ');
                    }
                    self.picked = None;
                }
                Some(MenuEntry::File {
                    start,
                    end,
                    path,
                    dir,
                    ..
                }) => self.apply_file(start, end, &path, dir),
                Some(MenuEntry::Message { .. }) | None => {}
            },
            Some(action @ (Action::MenuNext | Action::MenuPrevious)) => self.step_menu(action),
            Some(Action::MenuDismiss) => {
                // Keyed to the text, so any edit brings the list back —
                // "not that", not "never again".
                self.dismissed_at = Some(self.editor.text().to_string());
                self.disarm();
            }
            // Unbound and printable is the one thing no table has to say —
            // except in Normal, where it is the table saying no.
            None => {
                if let Some(c) = pi_store::keys::bare_letter(&key)
                    && self.vim.is_none_or(|m| m == Mode::Insert)
                {
                    self.editor.insert(c);
                    if let Some(last) = &mut self.last_press {
                        last.typed = true;
                    }
                }
            }
            // Another focus's, or a guard above that did not hold (`esc esc`
            // with text on the line): nothing here.
            Some(_) => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // The wheel scrolls what has the keyboard: a reply while one is up,
    // else the transcript. Moves and clicks are the transcript's.
    fn mouse(&mut self, view: &mut View, mouse: crossterm::event::MouseEvent) -> Asked {
        if matches!(self.focus, Focus::Reply(_)) {
            let room = self.regions.menu.height as usize;
            let width = self.screen.usable();
            match mouse.kind {
                MouseEventKind::ScrollUp => return self.scrolled(-1, room, width),
                MouseEventKind::ScrollDown => return self.scrolled(1, room, width),
                _ => {}
            }
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.selection = None;
                self.scroll_view(view, true, 1);
            }
            MouseEventKind::ScrollDown => {
                self.selection = None;
                self.scroll_view(view, false, 1);
            }
            MouseEventKind::Moved => self.on_mouse_move(view, mouse.column, mouse.row),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                self.on_mouse_move(view, mouse.column, mouse.row);
                self.on_drag(mouse.column, mouse.row);
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                self.on_press(view, mouse.column, mouse.row);
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                self.on_release(view, mouse.column, mouse.row);
            }
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // Submit `line` as typed: kept in history if it can be recalled, then read.
    fn run_line(&mut self, view: &mut View, line: String) -> Asked {
        if input::recallable(&line, &self.commands) {
            self.editor.remember(&line);
        }
        if line.trim().is_empty() {
            return Asked::Own(Deed::Nothing);
        }
        let intent = input::read(&line, &self.commands);
        if intent.echoed() {
            self.submit(view, &line);
            view.surface.scroll = 0;
        }
        self.submitted = true;
        Asked::Core(intent)
    }
}

// How far a pager key moves what it reads.
enum Stride {
    Line,
    Half,
    Page,
    // As far as there is.
    End,
}

// One row either menu can offer: a completion of the line, an @ path, or a
// message from the rewind selector to go back to.
#[derive(Clone)]
pub(super) enum MenuEntry {
    Completion(Candidate),
    // An @ path: accepting splices `@path` over the token it grew from —
    // Enter applies it and stays, where a command submits.
    File {
        start: usize,
        end: usize,
        // The row's left column: the file's own name, `/` for a directory.
        show: String,
        path: String,
        dir: bool,
    },
    // `help` says what picking the row does: two rows of prose read alike.
    Message {
        id: EntryId,
        show: String,
        help: &'static str,
    },
}

impl MenuEntry {
    pub(super) fn show(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.show,
            MenuEntry::File { show, .. } => show,
            MenuEntry::Message { show, .. } => show,
        }
    }

    pub(super) fn help(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.help,
            MenuEntry::File { path, .. } => path,
            MenuEntry::Message { help, .. } => help,
        }
    }
}

// What workspace-dependent completions answer with, read lazily: opening
// every archive and forking git upfront was a noticeable startup pause.
pub(super) struct Lists {
    pub(super) store: Store,
    pub(super) workspace: std::path::PathBuf,
    pub(super) sessions: std::cell::OnceCell<Vec<ResumeChoice>>,
    pub(super) worktrees: std::cell::OnceCell<Vec<Choice>>,
}

impl Lists {
    pub(super) fn new(store: Store, workspace: std::path::PathBuf) -> Self {
        Self {
            store,
            workspace,
            sessions: std::cell::OnceCell::new(),
            worktrees: std::cell::OnceCell::new(),
        }
    }

    pub(super) fn sessions(&self) -> &[ResumeChoice] {
        self.sessions
            .get_or_init(|| self.store.choices(&self.workspace))
    }

    // The checkouts with the branch each is on, where that says more than the
    // name. Empty outside a git repository, where there is nothing to offer.
    pub(super) fn worktrees(&self) -> &[Choice] {
        self.worktrees.get_or_init(|| {
            core::worktree::list(&self.workspace)
                .map(|trees| {
                    trees
                        .into_iter()
                        .map(|t| {
                            let note = t.branch_note().unwrap_or_default().to_string();
                            Choice { name: t.name, note }
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    // What's known about the checkouts without asking git: `None` until
    // read, and never forks. The bar reads this so a frame never waits.
    pub(super) fn worktrees_read(&self) -> Option<&[Choice]> {
        self.worktrees.get().map(Vec::as_slice)
    }

    // A turn or switch can change either list. Dropped, not recomputed —
    // whoever asks next pays, and usually nobody does.
    pub(super) fn forget(&mut self) {
        self.sessions.take();
        self.worktrees.take();
    }

    // Points at a workspace, dropping the last one's answers — both
    // lists are keyed by it, so after a move neither is merely stale.
    pub(super) fn at(&mut self, workspace: &std::path::Path) {
        self.workspace = workspace.to_path_buf();
        self.forget();
    }
}
