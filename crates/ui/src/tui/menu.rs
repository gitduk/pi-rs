//! The lists a surface opens over the editor — completions, `/model`,
//! `/rewind`, a question's options: what they offer and how they read.
//! `keys` drives them.
use super::Ui;
use super::search::{Place, Spot};
use super::view::View;
use pi_core::core;
use pi_core::core::question::Question;
use pi_core::input::commands::{Candidate, Choice, Input, Sources};
use pi_store::keys::Action;
use pi_store::session::ResumeChoice;
use pi_store::session::Store;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::ListItem;
use std::collections::BTreeSet;

impl Ui {
    pub(super) fn menu(&mut self) -> Vec<MenuEntry> {
        self.assistance().into_parts().0
    }

    pub(super) fn assistance(&mut self) -> Assistance {
        if self.search_typing() {
            return Assistance::new(self.search_menu(), None);
        }
        let (line, cursor) = self.list_line();
        let text = line.as_str();
        self.list.follow(text);
        let input = Input::new(
            text,
            cursor,
            &self.commands,
            self.tools.as_ref().map(|brief| &brief.registry),
        );
        let hint = (text == self.editor.text()).then(|| input.hint()).flatten();
        if self.list.dismissed_at.as_deref() == Some(text) {
            return Assistance::new(Vec::new(), hint);
        }
        // Directory names can contain spaces.
        // @ and ~/ complete only the token under the caret.
        use pi_core::input::complete as path;
        let found = input
            .directory()
            .map(|t| (t, Path::Directory))
            .or_else(|| path::at_prefix(text, cursor).map(|t| (t, Path::At)))
            .or_else(|| path::home_prefix(text, cursor).map(|t| (t, Path::Home)));
        if let Some(((start, end, query), kind)) = found {
            let token = &text[start..end];
            if self
                .at_menu
                .as_ref()
                .is_none_or(|(k, t, _)| (*k, t.as_str()) != (kind, token))
            {
                let home = pi_core::core::instructions::home();
                let items = match kind {
                    Path::At => path::candidates(query, &self.at_root),
                    Path::Home => home.map_or_else(Vec::new, |h| path::home_candidates(query, &h)),
                    Path::Directory => path::directories(query, &self.at_root, home.as_deref()),
                };
                self.at_menu = Some((kind, token.to_string(), items));
            }
            let lead = if kind == Path::At { "@" } else { "" };
            let browse = query.is_empty() || query.ends_with('/');
            let (_, _, items) = self.at_menu.as_ref().expect("set above");
            let menu = items
                .iter()
                .rev()
                .map(|f| MenuEntry::File {
                    start,
                    end,
                    show: if f.dir {
                        format!("{}/", f.path)
                    } else {
                        f.path.clone()
                    },
                    lead,
                    path: f.path.clone(),
                    dir: f.dir,
                    picked_only: browse,
                })
                .collect();
            return Assistance::new(menu, hint);
        }
        let picks = self.picks(text);
        if !picks.is_empty() {
            self.list.picked.get_or_insert(0);
            return Assistance::new(picks, None);
        }
        self.lists.catch_up();
        let sessions = || self.lists.sessions();
        let worktrees = || self.lists.worktrees();
        let editable = || self.lists.editable();
        let candidates = input.complete(&Sources {
            models: &self.choices,
            said: &self.said,
            sessions: Some(&sessions),
            worktrees: Some(&worktrees),
            editable: Some(&editable),
            servers: Some(&core::mcp::choices),
        });
        // Bottom-up: the best match belongs right above the input.
        Assistance::new(
            candidates
                .into_iter()
                .rev()
                .map(MenuEntry::Completion)
                .collect(),
            hint,
        )
    }

    // The line as its list reads it, with the caret: a character that may be
    // the first of a pair stays off it until the pair is made or its time is
    // up, so the list does not jump for a key that takes itself back.
    fn list_line(&self) -> (String, usize) {
        let (text, cursor) = (self.editor.text(), self.editor.cursor());
        let held = self.held().and(text[..cursor].chars().next_back());
        match held {
            Some(c) => {
                let at = cursor - c.len_utf8();
                (format!("{}{}", &text[..at], &text[cursor..]), at)
            }
            None => (text.to_string(), cursor),
        }
    }

    // The first question's options the line narrows to — by their words or
    // their number — all of them on an empty line, in the order asked.
    fn picks(&self, line: &str) -> Vec<MenuEntry> {
        // Not over a page or a search: Normal's bare letters are theirs.
        let Some(question) = self.questions.first().filter(|_| !self.reading()) else {
            return Vec::new();
        };
        let typed = line.trim().to_lowercase();
        (question.options.iter().enumerate())
            .filter(|(at, option)| {
                typed.is_empty()
                    || option.to_lowercase().contains(&typed)
                    || typed == (at + 1).to_string()
            })
            .map(|(at, option)| {
                // Boxes only once one is marked: until then it is a pick of one.
                let mark = match (self.list.marked.is_empty(), self.list.marked.contains(&at)) {
                    (true, _) => "",
                    (false, true) => "[x] ",
                    (false, false) => "[ ] ",
                };
                MenuEntry::Pick {
                    at,
                    show: format!("{} {mark}{option}", at + 1),
                }
            })
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

    pub(super) fn apply_completion(&mut self, candidate: Candidate) {
        self.editor.set_line(&candidate.line);
        if candidate.more {
            self.editor.insert(' ');
        }
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
        // The band says which row is picked; without colour there is none,
        // and the mark says it instead, the other rows set in as far.
        let marker = if self.paint.color { "" } else { MENU_MARK };
        let indent = " ".repeat(pi_store::text::width(marker));
        menu.iter()
            .enumerate()
            .map(|(at, c)| {
                if let (MenuEntry::Hit { show, k }, Some(query)) = (c, query) {
                    let room = width.saturating_sub(pi_store::text::width(marker));
                    let text = found_text(show, *k, query, room);
                    let line = if at == picked {
                        Line::from(vec![Span::styled(marker, mark), text.into()])
                    } else {
                        Line::from(Span::styled(format!("{indent}{text}"), muted))
                    };
                    return ListItem::new(super::search::marked(line, query, None, look, look));
                }
                let name = shown[at].as_str();
                let pad = head.saturating_sub(pi_store::text::width(name));
                let lead = format!("{}  ", " ".repeat(pad));
                // Cut where the menu ends, marked, not mid-word at its edge.
                let used = pi_store::text::width(marker)
                    + pi_store::text::width(name)
                    + pi_store::text::width(&lead);
                let help = pi_store::text::screen_line(c.help());
                let help = pi_store::text::clip(&help, width.saturating_sub(used));
                let rest = format!("{lead}{help}");
                let line = if at != picked {
                    Line::from(Span::styled(format!("{indent}{name}{rest}"), muted))
                } else {
                    Line::from(vec![
                        Span::styled(marker, mark),
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

    // Follow the selected prompt once per change, leaving subsequent
    // scrolling free to read its surroundings.
    pub(super) fn rewind_follow(&mut self, view: &View, menu: &[MenuEntry]) -> Option<Place> {
        let picked = self.list.highlighted(menu.len().max(1));
        let id = match menu.get(picked) {
            Some(MenuEntry::Completion(c)) => c
                .line
                .strip_prefix(super::REWIND)
                .and_then(|id| id.parse().ok())
                .map(agent::session::EntryId),
            _ => None,
        };
        if self.list.followed == id {
            return None;
        }
        self.list.followed = id;
        let idx = id.and_then(|id| {
            view.surface
                .scrollback
                .iter()
                .position(|r| r.entry() == Some(id))
        })?;
        self.close_page();
        Some(Place {
            spot: Spot::Row(idx, 0),
            row: 0,
        })
    }
}

// Candidates and placeholders must never appear together.
pub(super) enum Assistance {
    None,
    Menu(Vec<MenuEntry>),
    Hint(String),
}

impl Assistance {
    fn new(menu: Vec<MenuEntry>, hint: Option<String>) -> Self {
        if !menu.is_empty() {
            Self::Menu(menu)
        } else if let Some(hint) = hint {
            Self::Hint(hint)
        } else {
            Self::None
        }
    }

    pub(super) fn into_parts(self) -> (Vec<MenuEntry>, Option<String>) {
        match self {
            Self::None => (Vec::new(), None),
            Self::Menu(menu) => (menu, None),
            Self::Hint(hint) => (Vec::new(), Some(hint)),
        }
    }
}

// What a path token is completed against.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Path {
    At,
    Home,
    Directory,
}

// Where the list over the line stands: its highlight and window, kept
// against the line it grew from, and what was done to it there.
#[derive(Default)]
pub(super) struct List {
    // The prompt the view last followed while its completion was selected.
    followed: Option<agent::session::EntryId>,
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
        self.followed = None;
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

    // Whether enter takes this row when it is the highlighted one, `moved`
    // to or not: one that would turn what was typed into something else
    // waits to be moved to, and until then enter sends the line.
    pub(super) fn taken_by_enter(&self, moved: bool) -> bool {
        match self {
            MenuEntry::Completion(c) => !c.picked_only || moved,
            MenuEntry::File { picked_only, .. } => !picked_only || moved,
            MenuEntry::Hit { .. } | MenuEntry::Pick { .. } => true,
        }
    }

    pub(super) fn help(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.help,
            MenuEntry::File { .. } | MenuEntry::Hit { .. } | MenuEntry::Pick { .. } => "",
        }
    }
}

// What heads the picked row of a menu when no band can show it.
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
