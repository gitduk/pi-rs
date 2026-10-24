//! The lists a surface opens over the editor — completions, `/model`,
//! `/rewind`, a question's options: what they offer and how they read.
//! `keys` drives them.
use super::Ui;
use super::mouse::at_row_name;
use pi_core::core;
use pi_core::core::question::Question;
use pi_core::input::commands::{Candidate, Choice};
use pi_store::keys::Action;
use pi_store::session::ResumeChoice;
use pi_store::session::Store;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::ListItem;
use std::collections::BTreeSet;

impl Ui {
    // What the line could become: a completion while a command word is typed.
    pub(super) fn menu(&mut self) -> Vec<MenuEntry> {
        // The line's row is the query's while one is typed; the list, the
        // lines it is found on.
        if self.search_typing() {
            return self.search_menu();
        }
        self.list.follow(self.editor.text());
        if self.list.dismissed_at.as_deref() == Some(self.editor.text()) {
            return Vec::new();
        }
        // A path token outranks the word completions: the filesystem holds
        // the answer, not the command tables.
        use pi_core::input::complete as path;
        let (text, cursor) = (self.editor.text(), self.editor.cursor());
        // `/cd`'s argument is read first, `~/` and all: only a directory
        // is somewhere to go there.
        let found = (path::cd_prefix(text, cursor).map(|t| (t, Path::Cd)))
            .or_else(|| path::at_prefix(text, cursor).map(|t| (t, Path::At)))
            .or_else(|| path::home_prefix(text, cursor).map(|t| (t, Path::Home)));
        if let Some(((start, end, query), kind)) = found {
            // Keyed by the whole token: every keystroke moves it, but every
            // frame redraws the menu against the same one.
            let token = &text[start..end];
            if (self.at_menu.as_ref()).is_none_or(|(k, t, _)| (*k, t.as_str()) != (kind, token)) {
                let home = pi_core::core::instructions::home();
                let items = match kind {
                    Path::At => path::candidates(query, &self.at_root),
                    Path::Home => home.map_or_else(Vec::new, |h| path::home_candidates(query, &h)),
                    Path::Cd => path::cd_candidates(query, &self.at_root, home.as_deref()),
                };
                self.at_menu = Some((kind, token.to_string(), items));
            }
            let lead = if kind == Path::At { "@" } else { "" };
            // Nothing typed in the folder yet: the list is to browse, not a guess.
            let browse = query.is_empty() || query.ends_with('/');
            let (_, _, items) = self.at_menu.as_ref().expect("set above");
            return items
                .iter()
                .rev()
                .map(|f| MenuEntry::File {
                    start,
                    end,
                    show: at_row_name(&f.path, f.dir),
                    lead,
                    path: f.path.clone(),
                    dir: f.dir,
                    picked_only: browse,
                })
                .collect();
        }
        // A question's options before the words they would complete: one may
        // be a command, taken onto the line to send.
        let picks = self.picks();
        if !picks.is_empty() {
            return picks;
        }
        self.lists.catch_up();
        // Bottom-up: the best match belongs on the row right above the input.
        pi_core::input::commands::complete(
            self.editor.text(),
            &self.commands,
            &self.choices,
            pi_core::input::commands::Front {
                jobs: &self.every_job,
                tools: self.tools.as_ref().map(|brief| &brief.registry),
                said: &self.said,
            },
            || self.lists.sessions(),
            || self.lists.worktrees(),
            || self.lists.editable(),
        )
        .into_iter()
        .rev()
        .map(MenuEntry::Completion)
        .collect()
    }

    // The first question's options the line narrows to — by their words or
    // their number — all of them on an empty line, the first nearest it.
    fn picks(&self) -> Vec<MenuEntry> {
        // Not over a page or a search: Normal's bare letters are theirs.
        let Some(question) = self.questions.first().filter(|_| !self.reading()) else {
            return Vec::new();
        };
        let typed = self.editor.text().trim().to_lowercase();
        (question.options.iter().enumerate())
            .filter(|(at, option)| {
                typed.is_empty()
                    || option.to_lowercase().contains(&typed)
                    || typed == (at + 1).to_string()
            })
            .map(|(at, option)| {
                let mark = match (question.multiple, self.list.marked.contains(&at)) {
                    (false, _) => "",
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                };
                MenuEntry::Pick {
                    at,
                    show: format!("{} {mark}{option}", at + 1),
                }
            })
            .rev()
            .collect()
    }

    // The questions up where the front lane stands. A new first one starts
    // its list over: nothing marked, the highlight on its first option.
    pub(super) fn sync_questions(&mut self, questions: Vec<Question>) {
        if questions.first().map(|q| q.id) != self.questions.first().map(|q| q.id) {
            self.list.marked.clear();
            self.list.restart();
        }
        self.questions = questions;
    }

    // A command a key starts, on the line for its list to finish: typed into,
    // so Insert. Dismissing the list untouched takes the line back.
    pub(super) fn seed(&mut self, line: &str) {
        self.editor.set_line(line);
        self.list.seeded = Some(line.to_string());
        self.list.dismissed_at = None;
        self.leave_normal();
    }

    // The highlighted row, clamped: a list can change under the same line.
    pub(super) fn highlighted(&mut self) -> Option<MenuEntry> {
        let mut menu = self.menu();
        if menu.is_empty() {
            return None;
        }
        let at = self.list.highlighted(menu.len());
        Some(menu.swap_remove(at))
    }

    // Splice the accepted path over the token the list grew from. A
    // directory keeps completing: no trailing space, the caret stays on it.
    pub(super) fn apply_file(
        &mut self,
        start: usize,
        end: usize,
        lead: &str,
        path: &str,
        dir: bool,
    ) {
        let token = if dir {
            format!("{lead}{path}/")
        } else {
            format!("{lead}{path} ")
        };
        self.editor.splice(start, end, &token);
    }

    // Every row muted but `picked`: the mark, `menu.selected` on its name (a
    // found line's stays plain), and the list's band.
    pub(super) fn menu_items(
        &self,
        menu: &[MenuEntry],
        picked: usize,
        width: usize,
    ) -> Vec<ListItem<'static>> {
        // ratatui draws an escape's tail as text, so the list gets it as the
        // screen would show it; a hit's row was drawn once already.
        let shown: Vec<String> = (menu.iter())
            .map(|c| pi_store::text::screen_line(c.show()))
            .collect();
        let head = pi_store::text::widest(shown.iter().map(String::as_str));
        let muted = self.rat_style(&self.paint.theme.muted);
        let mark = self.rat_style(&self.paint.theme.prompt.color);
        let chosen = self.rat_style(&self.paint.theme.menu.selected);
        let query = self.search_query().map(|(query, _)| query);
        let look = self.rat_style(&self.paint.theme.matched);
        menu.iter()
            .enumerate()
            .map(|(at, c)| {
                if let (MenuEntry::Hit { show, k }, Some(query)) = (c, query) {
                    let room = width.saturating_sub(pi_store::text::width(MENU_MARK));
                    let text = found_text(show, *k, query, room);
                    let line = if at == picked {
                        Line::from(vec![Span::styled(MENU_MARK, mark), text.into()])
                    } else {
                        Line::from(Span::styled(format!("  {text}"), muted))
                    };
                    return ListItem::new(super::search::marked(line, query, None, look, look));
                }
                let show = shown[at].as_str();
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
                let pad = head.saturating_sub(pi_store::text::width(show));
                let lead = format!("{args}{}  ", " ".repeat(pad));
                // Cut where the menu ends, marked, not mid-word at its edge.
                let used = pi_store::text::width(MENU_MARK)
                    + pi_store::text::width(name)
                    + pi_store::text::width(&lead);
                let help = pi_store::text::screen_line(c.help());
                let help = pi_store::text::clip(&help, width.saturating_sub(used));
                let rest = format!("{lead}{help}");
                let line = if at != picked {
                    Line::from(Span::styled(format!("  {name}{rest}"), muted))
                } else {
                    Line::from(vec![
                        Span::styled(MENU_MARK, mark),
                        Span::styled(name.to_string(), chosen),
                        rest.into(),
                    ])
                };
                ListItem::new(line)
            })
            .collect()
    }

    pub(super) fn step_menu(&mut self, action: Action) {
        let len = self.menu().len();
        self.list.step(action == Action::MenuNext, len);
        // A search's list moves the view with it.
        self.search_follow();
    }
}

// What a path token is completed against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Path {
    At,
    Home,
    Cd,
}

// Where the list over the line stands: its highlight and window, kept
// against the line it grew from, and what was done to it there.
#[derive(Default)]
pub(super) struct List {
    // Which row is highlighted. `None` anchors a fresh list on its bottom
    // row, and says nobody moved to one.
    pub(super) picked: Option<usize>,
    // The first shown row, kept across frames: rebuilt from zero, the window
    // would pin the highlight to its bottom edge.
    pub(super) top: usize,
    // The line `picked` and `top` were set against: both are positions in
    // the list it grew, so another line starts over.
    picked_for: Option<String>,
    // Text the list was dismissed at. Any edit brings the list back —
    // Esc means "not that", not "never again".
    pub(super) dismissed_at: Option<String>,
    // A line a key put there, which dismissing its list takes back untouched.
    pub(super) seeded: Option<String>,
    // A question's options marked so far, where several may be.
    pub(super) marked: BTreeSet<usize>,
}

impl List {
    // A fresh list: the highlight back on the bottom row, in sight.
    pub(super) fn restart(&mut self) {
        (self.picked, self.top) = (None, 0);
    }

    // Kept against `line`: another one starts the list over.
    fn follow(&mut self, line: &str) {
        if self.picked_for.as_deref() != Some(line) {
            self.picked_for = Some(line.to_string());
            self.restart();
        }
    }

    // The highlighted row of `len`, clamped: a list can change under the
    // same line. `len` is never zero.
    pub(super) fn highlighted(&self, len: usize) -> usize {
        self.picked.unwrap_or(len - 1).min(len - 1)
    }

    fn step(&mut self, down: bool, len: usize) {
        let n = len.saturating_sub(1);
        let at = self.picked.unwrap_or(n).min(n);
        self.picked = Some(match down {
            true => at.saturating_add(1).min(n),
            false => at.saturating_sub(1),
        });
    }

    // Where several may be picked, the option at `at` in or out.
    pub(super) fn mark(&mut self, at: usize) {
        if !self.marked.remove(&at) {
            self.marked.insert(at);
        }
    }
}

// One row the menu can offer: a completion of the line, a path, or a line
// a search found.
#[derive(Clone)]
pub(super) enum MenuEntry {
    Completion(Candidate),
    // A path: accepting splices `lead` and `path` over the token it grew
    // from — Enter applies it and stays, where a command submits.
    File {
        start: usize,
        end: usize,
        // The row's left column: the file's own name, `/` for a directory.
        show: String,
        lead: &'static str,
        path: String,
        dir: bool,
        // As a `Candidate`'s: Enter takes it only once moved to.
        picked_only: bool,
    },
    // A hit: the row it is on, and which of that row's matches it is.
    Hit {
        show: String,
        k: usize,
    },
    // One of the first question's options, by its place.
    Pick {
        at: usize,
        show: String,
    },
}

impl MenuEntry {
    pub(super) fn show(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.show,
            MenuEntry::File { show, .. } => show,
            MenuEntry::Hit { show, .. } | MenuEntry::Pick { show, .. } => show,
        }
    }

    pub(super) fn help(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.help,
            MenuEntry::File { path, .. } => path,
            MenuEntry::Hit { .. } | MenuEntry::Pick { .. } => "",
        }
    }
}

// What heads the picked row of a menu, in the prompt's colour.
const MENU_MARK: &str = "› ";

// A hit as the list shows it in `room` columns: its whole row, cut on the
// left only as far as keeps the hit from running off the right.
fn found_text(show: &str, k: usize, query: &str, room: usize) -> String {
    let hit = pi_store::text::found(show, query).get(k).cloned();
    let text = match hit {
        Some(r) if pi_store::text::width(&show[..r.end]) > room => {
            pi_store::text::clip_left(show, r.start, room / 2)
        }
        _ => show.to_string(),
    };
    pi_store::text::clip(&text, room)
}

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
