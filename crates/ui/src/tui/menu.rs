//! The lists a surface opens over the editor — completions, `/model`, the
//! rewind menu: what they offer and how they read. `keys` drives them.
use super::Ui;
use super::mouse::at_row_name;
use super::ui::Focus;
use agent::session::EntryId;
use pi_core::core;
use pi_core::input::commands::{Candidate, Choice};
use pi_store::keys::Action;
use pi_store::session::ResumeChoice;
use pi_store::session::Store;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::ListItem;

impl Ui {
    // What the line could become: a completion while a command word is
    // typed, or (rewind selector open) a message to rewind to.
    pub(super) fn menu(&mut self) -> Vec<MenuEntry> {
        match &self.focus {
            Focus::Rewind(rows) => return rows.clone(),
            Focus::Read(_) => return Vec::new(),
            Focus::Editor => {}
        }
        if self.picked_for.as_deref() != Some(self.editor.text()) {
            self.picked_for = Some(self.editor.text().to_string());
            self.picked = None;
            self.menu_top = 0;
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
        self.lists.catch_up();
        // Bottom-up: the best match belongs on the row right above the input.
        pi_core::input::commands::complete(
            self.editor.text(),
            &self.commands,
            &self.choices,
            || self.lists.sessions(),
            || self.lists.worktrees(),
            || self.lists.editable(),
        )
        .into_iter()
        .rev()
        .map(MenuEntry::Completion)
        .collect()
    }

    // Open the rewind selector on the given messages, newest selected first.
    pub(super) fn open_rewind(&mut self, rows: Vec<MenuEntry>) {
        self.picked = Some(rows.len().saturating_sub(1));
        self.picked_for = None;
        self.menu_top = 0;
        self.focus = Focus::Rewind(rows);
    }

    // The highlighted row, clamped: a list can change under the same line.
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
    pub(super) fn apply_file(&mut self, start: usize, end: usize, path: &str, dir: bool) {
        let token = if dir {
            format!("@{path}/")
        } else {
            format!("@{path} ")
        };
        self.editor.splice(start, end, &token);
    }

    // The menu's rows as ratatui list items. The selected row is styled by
    // the list itself; everything else sits muted.
    // Every row muted but `picked`, which is bright and wears the mark and the
    // theme's `menu.selected` on its name; its band is the list's to draw.
    pub(super) fn menu_items(&self, menu: &[MenuEntry], picked: usize) -> Vec<ListItem<'static>> {
        let head = menu
            .iter()
            .map(|c| unicode_width::UnicodeWidthStr::width(c.show()))
            .max()
            .unwrap_or(0);
        let muted = self.rat_style(&self.paint.theme.muted);
        let mark = self.rat_style(&self.paint.theme.prompt.color);
        let chosen = self.rat_style(&self.paint.theme.menu.selected);
        menu.iter()
            .enumerate()
            .map(|(at, c)| {
                let show = c.show();
                // Only a command word carries an argument hint to set apart; a
                // message or a file name is whole, spaces and all.
                let (name, args) = match c {
                    MenuEntry::Completion(c) if c.more => {
                        show.split_once(' ').unwrap_or((show, ""))
                    }
                    _ => (show, ""),
                };
                let args = if args.is_empty() {
                    String::new()
                } else {
                    format!(" {args}")
                };
                let pad = head.saturating_sub(unicode_width::UnicodeWidthStr::width(show));
                let rest = format!("{args}{}  {}", " ".repeat(pad), c.help());
                if at != picked {
                    return ListItem::new(Line::from(Span::styled(
                        format!("  {name}{rest}"),
                        muted,
                    )));
                }
                ListItem::new(Line::from(vec![
                    Span::styled(MENU_MARK, mark),
                    Span::styled(name.to_string(), chosen),
                    rest.into(),
                ]))
            })
            .collect()
    }

    pub(super) fn step_menu(&mut self, action: Action) {
        let n = self.menu().len().saturating_sub(1);
        let at = self.picked.unwrap_or(n).min(n);
        self.picked = Some(if action == Action::MenuNext {
            at.saturating_add(1).min(n)
        } else {
            at.saturating_sub(1)
        });
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
    // Something the user said, to rewind to. Every row does the same, so
    // none carries a note saying what.
    Message {
        id: EntryId,
        show: String,
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
            MenuEntry::Message { .. } => "",
        }
    }
}

// What heads the picked row of a menu, in the prompt's colour.
const MENU_MARK: &str = "› ";

// What workspace-dependent completions answer with, read lazily: opening
// every archive and forking git upfront was a noticeable startup pause.
pub(super) struct Lists {
    pub(super) store: Store,
    pub(super) workspace: std::path::PathBuf,
    pub(super) sessions: std::cell::OnceCell<Vec<ResumeChoice>>,
    // The store's version `sessions` was read at.
    pub(super) sessions_at: u64,
    pub(super) worktrees: std::cell::OnceCell<Vec<Choice>>,
    pub(super) editable: std::cell::OnceCell<Vec<Choice>>,
    // The flags `/edit`'s list depends on: `--system`, `--no-context-files`.
    pub(super) pinned: pi_core::args::Pinned,
}

impl Lists {
    pub(super) fn new(store: Store, workspace: std::path::PathBuf) -> Self {
        Self {
            store,
            workspace,
            sessions: std::cell::OnceCell::new(),
            sessions_at: 0,
            worktrees: std::cell::OnceCell::new(),
            editable: std::cell::OnceCell::new(),
            pinned: Default::default(),
        }
    }

    // The files `/edit` offers in this workspace.
    pub(super) fn editable(&self) -> &[Choice] {
        self.editable.get_or_init(|| {
            core::resolve::editable(&self.pinned, &self.workspace)
                .into_iter()
                .map(Choice::from)
                .collect()
        })
    }

    // Drops the session list once the store has written since it was read:
    // whatever saved, renamed or moved a session needn't say so.
    pub(super) fn catch_up(&mut self) {
        let now = self.store.version();
        if self.sessions_at != now {
            self.sessions.take();
            self.sessions_at = now;
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

    // A switch, a checkout gone or a file `/edit` made changes them. Dropped,
    // not recomputed — whoever asks next pays, and usually nobody does.
    pub(super) fn forget(&mut self) {
        self.sessions.take();
        self.worktrees.take();
        self.editable.take();
    }

    // Points at a workspace, dropping the last one's answers — both
    // lists are keyed by it, so after a move neither is merely stale.
    pub(super) fn at(&mut self, workspace: &std::path::Path) {
        self.workspace = workspace.to_path_buf();
        self.forget();
    }
}
