//! What the surface keeps for itself across lanes: the editor and the rows
//! around it, the bar, the open reply, the theme it paints in.

use std::sync::Arc;
use std::time::Instant;

use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};

use super::call::{drawn, pending_line};
use super::editor::Editor;
use super::menu::{Lists, MenuEntry};
use super::mouse::{Regions, Target};
use super::reply::Reply;
use super::row::{Row, count};
use super::screen::{self, Screen};
use super::scrollback::body;
use super::view::{StreamKind, View, snapshot};
use crate::render::Paint;
use crate::status;
use pi_core::core::Core;
use pi_core::core::lane::Lane;
use pi_core::core::resolve::Resolved;
use pi_core::input::commands::{Choice, Command};
use pi_store::icons;
use pi_store::keys::Keys;
use pi_store::status::{Segment, default_parts};
use pi_store::theme::Style as ThemeStyle;
use pi_store::theme::{Theme, bands_for};

pub(super) struct Ui {
    pub(super) screen: Screen,
    pub(super) keys: Arc<Keys>,
    pub(super) editor: Editor,
    pub(super) paint: Paint,
    // The prompt sigil, shared by the editor and the echoed lines.
    pub(super) prompt: Span<'static>,
    // The band behind the input line, painted before its rows so the columns
    // the text does not reach carry it too.
    pub(super) band: Option<RStyle>,
    // The terminal's own background, asked for once at startup: the prompt's
    // bands are a lift of it, and a terminal that will not say leaves both be.
    pub(super) tty_bg: Option<(u8, u8, u8)>,
    // The same sigil for a `!` line, where the bang takes the icon's place.
    pub(super) bang_prompt: Span<'static>,
    // The bar's separator, painted once beside the two above it: the bar
    // is rebuilt every frame and this depends only on the theme.
    pub(super) tab_sep: Span<'static>,
    // Which row of the list is highlighted; the list itself is derived
    // from what's typed. `None` anchors a fresh list on its bottom row.
    pub(super) picked: Option<usize>,
    // Text the list was dismissed at. Any edit brings the list back —
    // Esc means "not that", not "never again".
    pub(super) dismissed_at: Option<String>,
    // A line was submitted. Echo happens here (the view is in hand), but
    // the recall list is the surface's, so it's told once per line.
    pub(super) submitted: bool,
    // What `/model` can complete to. A copy rather than a borrow of the
    // config: the loop holds the session mutably while it draws.
    pub(super) choices: Vec<Choice>,
    pub(super) lists: Lists,
    // The same copy, of the same list `/help` prints.
    pub(super) commands: Arc<Vec<Command>>,
    // Where keys land. One place at a time: what draws a caret, takes a
    // paste or answers the wheel is read off this, not off flags beside it.
    pub(super) focus: Focus,
    // The press before this one, for the pairs the key table binds.
    pub(super) last_press: Option<LastPress>,
    // How long a bare character waits for the second of its pair.
    pub(super) pair_window: std::time::Duration,
    // The @-completion cache, keyed by the query the walk was built for —
    // a directory walk sits behind every keystroke otherwise.
    pub(super) at_menu: Option<(String, Vec<pi_core::input::complete::FileEntry>)>,
    // The directory @ paths resolve against: the lane's workspace root.
    pub(super) at_root: std::path::PathBuf,
    pub(super) spinner: usize,
    // The modal keys, or None while they are off.
    pub(super) vim: Option<pi_store::keys::Mode>,
    // The segments the status line shows, in the order the config named them.
    pub(super) status: Vec<Segment>,
    // What the bar says, in ring order. Rebuilt before every draw.
    pub(super) tabs: Vec<Tab>,
    // A note answering the last keypress, and when it landed. It stands
    // where the layout puts it for `FLASH` and then goes — see `bar_lines`.
    pub(super) flash: Option<(String, Instant)>,
    // What the bar's rows hold: the script's last answer, or the default.
    pub(super) layout: pi_store::bar::Layout,
    // The scrollback row and line under the mouse, when a click there
    // opens or closes something.
    pub(super) hovered_scrollback: Option<(usize, usize)>,
    pub(super) row_targets: Vec<Target>,
    // Where the frame's regions landed last. The click handler reads them
    // back: a screen row only means something inside a named region.
    pub(super) regions: Regions,
    // Whether the live block lists every call in flight or just the newest
    // with a count; a click flips it. Outlives the calls themselves.
    pub(super) live_tools_shown: bool,
}

/// Who has the keyboard.
pub(super) enum Focus {
    // The line, with completions over it while what is typed has some.
    Editor,
    // The messages a rewind can go back to, session order, newest last.
    Rewind(Vec<MenuEntry>),
    // What a slash command answered, read until closed.
    Reply(Reply),
    // The conversation alone, the editor hidden; `browse.rs` draws it.
    Browse,
}

/// A press, kept for the one after it.
#[derive(Clone, Copy)]
pub(super) struct LastPress {
    pub(super) press: pi_store::keys::Press,
    pub(super) at: Instant,
    // It put a character on the line, which a pair it starts takes back.
    pub(super) typed: bool,
}

// What to call a checkout: the root answers to its directory name (as
// `worktree list` does), so a fixed word wouldn't collide with one.
pub(super) fn lane_name(lane: &Lane) -> String {
    lane.worktree().map(str::to_string).unwrap_or_else(|| {
        lane.root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

// How a checkout shows on the bar: front, how its last run ended, plain
// name, or `Unopened`. A run in flight shows nothing — only finished state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mark {
    Front,
    Done,
    Failed,
    Plain,
    Unopened,
}

impl Mark {
    // Whether this mark might be unread yet. The bar drops quiet ones
    // first — a colored name pushed off the edge is a run nobody saw end.
    pub(super) fn quiet(self) -> bool {
        matches!(self, Mark::Plain | Mark::Unopened)
    }
}

// One checkout as the bar shows it, rebuilt before every draw — the bar is a
// view of state the surface does not own.
pub(super) struct Tab {
    pub(super) mark: Mark,
    pub(super) name: String,
}

impl Ui {
    // What leaves with the checkout: the half-typed line (parked in its
    // lane's view, not the shared editor) and lane-scoped state (flash, rewind).
    pub(super) fn leave_lane(&mut self, view: &mut View) {
        view.draft = self.editor.take_composing();
        self.flash = None;
        if matches!(self.focus, Focus::Rewind(_)) {
            self.focus = Focus::Editor;
        }
    }

    pub(super) fn new(
        screen: Screen,
        keys: Arc<Keys>,
        choices: Vec<Choice>,
        commands: Arc<Vec<Command>>,
        lists: Lists,
        paint: Paint,
    ) -> Self {
        let prompt = Self::paint_prompt(&paint, &paint.theme.prompt.icon);
        let bang_prompt = Self::paint_prompt(&paint, icons::BANG_SIGIL);
        let band = paint.band(&paint.theme.prompt.panel.input);
        let mut editor = Editor::default();
        editor.set_prompts(prompt.clone(), bang_prompt.clone());
        Self {
            screen,
            keys,
            submitted: false,
            choices,
            commands,
            lists,
            editor,
            tab_sep: Self::paint_sep(&paint),
            paint,
            prompt,
            band,
            tty_bg: None,
            bang_prompt,
            picked: None,
            dismissed_at: None,
            focus: Focus::Editor,
            last_press: None,
            pair_window: std::time::Duration::ZERO,
            at_menu: None,
            at_root: std::path::PathBuf::new(),
            vim: None,
            spinner: 0,
            status: default_parts(),
            tabs: Vec::new(),
            flash: None,
            layout: Default::default(),
            hovered_scrollback: None,
            row_targets: Vec::new(),
            live_tools_shown: false,
            regions: Regions::default(),
        }
    }

    // The separator between lanes on the bar, dimmed so the names it
    // divides are what the eye lands on.
    pub(super) fn paint_sep(paint: &Paint) -> Span<'static> {
        paint.span(&paint.theme.muted, icons::PART_SEP)
    }

    // The prompt sigil as the terminal shows it, colour and all.
    pub(super) fn paint_prompt(paint: &Paint, icon: &str) -> Span<'static> {
        paint.span(&paint.theme.prompt.color, icons::bar(icon))
    }

    // A theme style, as ratatui sees it.
    pub(super) fn rat_style(&self, s: &ThemeStyle) -> RStyle {
        crate::sgr::style_to_ratatui(s)
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

        (rows, pending_rows)
    }

    // The checkouts, one entry each in `refresh_tabs` order. Too narrow, the
    // front one stays and each dropped end keeps its `…`.
    pub(super) fn tabs_strip(&self, strip: usize) -> Vec<Span<'static>> {
        let theme = &self.paint.theme;
        let sep = self.tab_sep.width();
        let Some(front) = self.tabs.iter().position(|t| t.mark == Mark::Front) else {
            return Vec::new();
        };
        let items: Vec<Span<'static>> = self
            .tabs
            .iter()
            .map(|tab| {
                let (sign, style) = match tab.mark {
                    Mark::Front => ("", &theme.input),
                    // Settled lanes are their colour alone: a glyph would repeat
                    // what `Done`/`Failed` already say.
                    Mark::Done => ("", &theme.status.ok),
                    Mark::Failed => ("", &theme.status.err),
                    Mark::Plain => ("", &theme.muted),
                    Mark::Unopened => (icons::UNOPENED_MARK, &theme.muted),
                };
                let label = if sign.is_empty() {
                    tab.name.clone()
                } else {
                    format!("{sign} {}", tab.name)
                };
                self.paint.span(style, label)
            })
            .collect();
        let dots = self.paint.span(&theme.muted, icons::ELLIPSIS);
        let ell = dots.width();
        let n = items.len();
        let widths: Vec<usize> = items.iter().map(Span::width).collect();
        // What the window would take on the row, the `…` a dropped end leaves
        // behind included.
        let fits = |lo: usize, hi: usize| {
            widths[lo..=hi].iter().sum::<usize>()
                + (hi - lo) * sep
                + if lo > 0 { ell + sep } else { 0 }
                + if hi + 1 < n { sep + ell } else { 0 }
                <= strip
        };
        let (mut lo, mut hi) = (0, n - 1);
        while !fits(lo, hi) && (lo < front || hi > front) {
            let quiet = |at: usize| self.tabs[at].mark.quiet();
            let (left, right) = (
                (lo < front).then(|| quiet(lo)),
                (hi > front).then(|| quiet(hi)),
            );
            // A quiet end drops before a marked one; of two alike, the one
            // farther from the front. A tie goes right (older checkouts stay).
            let drop_right = match (left, right) {
                (Some(true), Some(false)) => false,
                (Some(false), Some(true)) => true,
                (None, _) => true,
                (_, None) => false,
                _ => hi - front >= front - lo,
            };
            if drop_right {
                hi -= 1;
            } else {
                lo += 1;
            }
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        if lo > 0 {
            spans.push(dots.clone());
            spans.push(self.tab_sep.clone());
        }
        for (i, item) in items.into_iter().enumerate().take(hi + 1).skip(lo) {
            if i > lo {
                spans.push(self.tab_sep.clone());
            }
            spans.push(item);
        }
        if hi + 1 < n {
            spans.push(self.tab_sep.clone());
            spans.push(dots);
        }
        // A front checkout too long for its share folds itself here rather
        // than eating the parts beside it.
        screen::fit(&Line::from(spans), strip).remove(0).spans
    }

    // The checkout a step from this one (Normal `L`/`H`), wrapping at
    // either end; None with nowhere else to go.
    pub(super) fn step_checkout(&self, lane: &Lane, forward: bool) -> Option<String> {
        let trees = self.lists.worktrees();
        let n = trees.len();
        if n < 2 {
            return None;
        }
        // The bar's order, and a tab that names no checkout is dropped — a lane
        // whose checkout went names nothing to step to.
        let mut order: Vec<&str> = Vec::with_capacity(n);
        for tab in &self.tabs {
            let name = tab.name.as_str();
            if trees.iter().any(|c| c.name == name) && !order.contains(&name) {
                order.push(name);
            }
        }
        // A bar that has not rebuilt since the disk changed leaves the ring
        // short of it: the disk's own order is where the rest belongs.
        for tree in trees {
            if !order.contains(&tree.name.as_str()) {
                order.push(&tree.name);
            }
        }
        // The lane's worktree name — or its directory, in the main checkout.
        let at = order
            .iter()
            .position(|name| *name == lane_name(lane))
            .unwrap_or(0);
        let i = if forward {
            (at + 1) % order.len()
        } else {
            (at + order.len() - 1) % order.len()
        };
        Some(order[i].to_string())
    }

    pub(super) fn set_theme(
        &mut self,
        view: &mut View,
        resolved: &Resolved,
        theme: Arc<pi_store::theme::Theme>,
    ) {
        self.paint.theme = theme;
        self.bang_prompt = Self::paint_prompt(&self.paint, icons::BANG_SIGIL);
        self.tab_sep = Self::paint_sep(&self.paint);
        self.band = self.paint.band(&self.paint.theme.prompt.panel.input);
        self.show_mode();
        // The opening block is painted once at construction; rebuild it so a
        // reload lands on the new theme instead of the old.
        let opening = Row::banner(resolved, &self.paint);
        let rest = view.surface.scrollback.split_off(view.surface.opened);
        view.surface.opened = opening.len();
        view.surface.scrollback = opening.into_iter().chain(rest).collect();
    }

    // Every copy of the config the surface keeps, brought up to date — the one
    // landing a reload and `/settings` share.
    pub(super) fn adopt_config(&mut self, core: &Core, view: &mut View) {
        // The key map lives in two places; a reload has to reach both or the
        // screen keeps answering to the old bindings.
        if !Arc::ptr_eq(&self.keys, &core.keys) {
            self.keys = core.keys.clone();
        }
        // Likewise the completion list: a reload is allowed to define models —
        // and skills — the last one did not.
        self.choices = core.choices();
        // The theme takes the view it was painted into — the opening block is
        // painted in it — and every copy takes the terminal's band.
        let theme = following_terminal(&core.config.theme, self.tty_bg);
        if self.paint.theme.as_ref() != &theme {
            self.set_theme(view, core.lane().resolved(), Arc::new(theme));
        }
        if !Arc::ptr_eq(&self.commands, &core.commands) {
            self.commands = core.commands.clone();
        }
        self.set_vim(&core.config.vim);
        // And the segment lists, copied in at startup.
        self.status = core.config.status.to_vec();
    }
}

// The config's theme with the prompt's bands following the terminal, unless the
// config named a band itself — a file that wanted a fixed colour wrote one.
pub(super) fn following_terminal(theme: &Theme, bg: Option<(u8, u8, u8)>) -> Theme {
    let mut theme = theme.clone();
    let Some(bg) = bg else {
        return theme;
    };
    let bands = bands_for(bg);
    let default = pi_store::theme::Bands::default();
    if theme.prompt.panel.input == default.input {
        theme.prompt.panel.input = bands.input;
    }
    if theme.prompt.panel.said == default.said {
        theme.prompt.panel.said = bands.said;
    }
    theme
}
