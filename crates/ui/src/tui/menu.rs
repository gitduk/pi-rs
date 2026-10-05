//! The lists a surface opens over the editor — completions, `/model`, the
//! rewind menu — and the keys that drive them.
use super::mouse::at_row_name;
use super::row::Row;
use super::view::View;
use super::vim::{Typed, double_tap};
use super::{Asked, Deed, Ui};
use agent::session::EntryId;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, MouseEventKind};
use pi_core::core;
use pi_core::core::lane::Lane;
use pi_core::input::Builtin;
use pi_core::input::commands::{Candidate, Choice};
use pi_core::input::{self, Intent};
use pi_store::keys::{Action, Layers, Menu, Press};
use pi_store::session::ResumeChoice;
use pi_store::session::Store;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::ListItem;
use std::time::Instant;

impl Ui {
    // A reply is up: it owns the menu's space, and the completion list
    // waits. Also forces the menu's own key layer while open.
    pub(super) fn overlay(&self) -> bool {
        self.reply.is_some()
    }

    // What the line could become: a completion while a command word is
    // typed, or (rewind selector open) a message to rewind to.
    pub(super) fn menu(&mut self) -> Vec<MenuEntry> {
        if self.overlay() {
            return Vec::new();
        }
        if !self.rewind.is_empty() {
            return self.rewind.clone();
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
        self.rewind = rows;
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
                self.last_esc = None;
                if let Some(v) = &mut self.vim {
                    v.last = None;
                }
                // Browse mode hides the line: text pasted into it would land
                // where nobody can see it, and be there on the way out.
                if !self.browsing {
                    self.editor.insert_str(&text.replace('\r', "\n"));
                }
                return Asked::Own(Deed::Nothing);
            }
            TermEvent::Mouse(mouse) => {
                // A reply is topmost, as for keys: the wheel scrolls it
                // while up, never the transcript underneath.
                if self.reply.is_some() {
                    let room = self.regions.menu.height as usize;
                    let width = self.screen.usable();
                    match mouse.kind {
                        MouseEventKind::ScrollUp => return self.scrolled(-1, room, width),
                        MouseEventKind::ScrollDown => return self.scrolled(1, room, width),
                        _ => {}
                    }
                }
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_view(view, true, 1),
                    MouseEventKind::ScrollDown => self.scroll_view(view, false, 1),
                    MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                        self.on_mouse_move(view, mouse.column, mouse.row);
                    }
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        self.on_mouse_click(view, mouse.column, mouse.row);
                    }
                    _ => {}
                }
                return Asked::Own(Deed::Nothing);
            }
            // Windows reports both press and release; acting on both would
            // double every keystroke. Any non-Esc key breaks the rewind double-tap.
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                if k.code != KeyCode::Esc {
                    self.last_esc = None;
                }
                k
            }
            _ => return Asked::Own(Deed::Nothing),
        };
        // Browse mode takes the keyboard whole — the editor's table has
        // nothing on screen to aim at. A reply (drawn over everything) outranks it.
        if self.browsing && !self.overlay() {
            return self.browse_key(view, key);
        }
        let press = Press::of(key.code, key.modifiers);
        // A reply counts as a menu (its keys are the Menu bindings) and
        // forces the layer on, computed here since `menu()` mutates state.
        let menu = if self.overlay() {
            Menu::On
        } else if self.menu().is_empty() {
            Menu::Off
        } else {
            Menu::On
        };
        let bound = self.keys.action(
            press,
            Layers {
                menu,
                run: running,
                mode: self.vim.as_ref().map(|v| v.mode),
                // Whether the line has anything on it, which the keys bound
                // to an empty one read. Only Normal asks.
                line_empty: self.editor.is_empty(),
            },
        );

        // A key that means something breaks the escape sequence: `j`, a
        // command, then `k` is two commands and a `j`, not a mode change.
        if bound.is_some()
            && let Some(v) = &mut self.vim
        {
            v.last = None;
        }

        // The reply reads only its own keys (scroll, dismiss, global
        // presses); anything else is typing and takes it down first.
        if self.reply.is_some() && !super::reply::owns(bound, key) {
            self.reply = None;
        }
        if self.reply.is_some() {
            if matches!(bound, Some(Action::LineSubmit | Action::AppCancel)) {
                self.reply = None;
            } else {
                return self.reply_key(bound, key);
            }
        }

        if !self.rewind.is_empty()
            && !matches!(
                bound,
                Some(
                    Action::MenuDismiss
                        | Action::MenuAccept
                        | Action::MenuNext
                        | Action::MenuPrevious
                        | Action::LineSubmit
                )
            )
        {
            self.rewind.clear();
        }

        match bound {
            Some(Action::AppCancel) => return self.interrupt_or_quit(running),
            Some(Action::LineSubmit) => {
                // Enter with a menu open runs what's highlighted, the typed
                // text a mere prefix. Pick before draining the editor.
                match self.highlighted() {
                    Some(MenuEntry::Message { id, .. }) => {
                        self.rewind.clear();
                        return Asked::Own(Deed::To(id));
                    }
                    Some(MenuEntry::Completion(c)) => {
                        let line = c.line;
                        // The completion's line runs; the prefix that
                        // produced it goes, so it can't resubmit as a stray prompt.
                        self.editor.take();
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
                        return Asked::Core(intent);
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
                        return Asked::Own(Deed::Nothing);
                    }
                    None => {
                        let typed = self.editor.take();
                        if input::recallable(&typed, &self.commands) {
                            self.editor.remember(&typed);
                        }
                        if typed.trim().is_empty() {
                            return Asked::Own(Deed::Nothing);
                        }
                        let intent = input::read(&typed, &self.commands);
                        if intent.echoed() {
                            self.submit(view, &typed);
                            view.surface.scroll = 0;
                        }
                        self.submitted = true;
                        return Asked::Core(intent);
                    }
                }
            }
            Some(Action::EditExternally) => return Asked::Own(Deed::External),
            Some(Action::RunInterrupt) => {
                // Esc before the model has moved means "I didn't mean to send
                // that"; an empty editor, or unsending overwrites a line.
                if self.editor.is_empty() && lane.is_running() && !view.state.committed {
                    return Asked::Own(Deed::Unsend);
                }
                return Asked::Own(Deed::Interrupt);
            }
            Some(Action::Rewind) => {
                // Double Esc on an empty line opens the rewind selector:
                // first press arms it, second (in window) asks for messages.
                if !self.editor.is_empty() {
                    return Asked::Own(Deed::Nothing);
                }
                let now = Instant::now();
                if double_tap(&mut self.last_esc, now) {
                    self.last_esc = None;
                    return Asked::Own(Deed::Rewind);
                }
                return Asked::Own(Deed::Nothing);
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
            Some(Action::LineClear) => {
                // A line to lose goes on the one press; with nothing there,
                // the session is what a press would replace, so it takes two.
                if self.editor.is_empty() {
                    let now = Instant::now();
                    if double_tap(&mut self.last_l, now) {
                        self.last_l = None;
                        return Asked::Core(Intent::Builtin(Builtin::New));
                    }
                } else {
                    // The armed half goes with the line: a quick second
                    // press must not start a session on the line just cleared.
                    self.last_l = None;
                    self.editor.clear();
                }
                return Asked::Own(Deed::Nothing);
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
            _ => {}
        }

        match bound {
            Some(Action::InsertNewline) => self.editor.insert('\n'),
            Some(Action::DeleteCharBack) => self.editor.backspace(),
            Some(Action::DeleteCharForward) => self.editor.delete(),
            Some(Action::DeleteWordBack) => self.editor.kill_word_back(),
            Some(Action::DeleteToLineEnd) => self.editor.kill_to_end(),
            Some(Action::DeleteToLineStart) => self.editor.kill_to_start(),
            Some(Action::MoveCharLeft) => self.editor.left(),
            Some(Action::MoveCharRight) => self.editor.right(),
            Some(Action::MoveWordLeft) => self.editor.word_left(),
            Some(Action::MoveWordRight) => self.editor.word_right(),
            Some(Action::MoveWordNext) => self.editor.word_next(),
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

            Some(Action::MenuAccept) => {
                match self.highlighted() {
                    Some(MenuEntry::Message { id, .. }) => {
                        self.rewind.clear();
                        return Asked::Own(Deed::To(id));
                    }
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
                    }) => {
                        self.apply_file(start, end, &path, dir);
                    }
                    None => {}
                }
            }
            Some(Action::MenuNext) => {
                let n = self.menu().len().saturating_sub(1);
                let at = self.picked.unwrap_or(n).min(n);
                self.picked = Some(at.saturating_add(1).min(n));
            }
            Some(Action::MenuPrevious) => {
                let n = self.menu().len().saturating_sub(1);
                let at = self.picked.unwrap_or(n).min(n);
                self.picked = Some(at.saturating_sub(1));
            }
            // Answered in the first match, which returns; named here because
            // this one has no catch-all and should not grow one.
            Some(Action::LaneNext) | Some(Action::LanePrev) | Some(Action::EditExternally) => {}
            Some(Action::MenuDismiss) => {
                let was_rewind = !self.rewind.is_empty();
                self.rewind.clear();
                // Keyed to the text, so any edit brings the list back —
                // "not that", not "never again". Rewind doesn't record this.
                if !was_rewind {
                    self.dismissed_at = Some(self.editor.text().to_string());
                }
            }
            // Unbound and printable is the one thing no table has to say —
            // except in Normal, where it is the table saying no.
            None => {
                if let Some(c) = pi_store::keys::bare_letter(&key) {
                    match self.vim.as_mut().map(|v| v.typed(c, Instant::now())) {
                        Some(Typed::Ignore) => {}
                        // The sequence's first half is on screen already;
                        // take it back so the line always shows what it means.
                        Some(Typed::Escape) => {
                            self.editor.backspace();
                            self.show_mode();
                        }
                        Some(Typed::Command(action)) => match action {
                            Action::DeleteLine => self.editor.delete_line(),
                            Action::ChangeLine => self.change_line(),
                            Action::MoveBufferStart => self.buffer_ends(view, true),
                            _ => unreachable!("a doubled key names one of the three"),
                        },
                        Some(Typed::Insert) | None => self.editor.insert(c),
                    }
                }
            }
            Some(
                Action::LineClear
                | Action::AppCancel
                | Action::LineSubmit
                | Action::RunInterrupt
                | Action::AppExit
                | Action::Rewind,
            ) => unreachable!("handled scrollback"),
            // No binding reaches for these: `dd` and `gg` are doubled keys
            // the `None` arm answers, so the table never sends them here.
            Some(Action::DeleteLine | Action::MoveBufferStart) => {
                unreachable!("doubled keys are answered where they are typed")
            }
        }
        Asked::Own(Deed::Nothing)
    }

    // One key, two meanings: stop the run, or — pressed twice inside the
    // window — leave. The escalation lives with the binding, not Ctrl-C.
    fn interrupt_or_quit(&mut self, running: bool) -> Asked {
        if double_tap(&mut self.last_interrupt, Instant::now()) {
            return Asked::Core(Intent::Builtin(Builtin::Quit));
        }
        if running {
            return Asked::Own(Deed::Interrupt);
        }
        // The next press is the one that leaves, and saying so is what keeps
        // this one from reading as a key that did nothing.
        self.flash("press it again to quit");
        Asked::Own(Deed::Nothing)
    }
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
