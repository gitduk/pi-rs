//! What the surface keeps for itself across lanes: the editor and the rows
//! around it, the bar, the open panel or reply, the theme it paints in.

use std::sync::Arc;
use std::time::Instant;

use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};

use super::editor::Editor;
use super::menu::{Lists, MenuEntry};
use super::mouse::{Regions, Target};
use super::panel::Panel;
use super::reply::Reply;
use super::row::Row;
use super::screen::{self, Screen};
use super::scrollback::body;
use super::tool::{drawn, pending_line};
use super::view::{StreamKind, View, snapshot};
use super::vim::Vim;
use crate::core::Core;
use crate::core::lane::Lane;
use crate::input::commands::{Choice, Command};
use crate::store::icons;
use crate::store::keys::Keys;
use crate::store::status::{Segment, default_done, default_live};
use crate::store::theme::Style as ThemeStyle;
use crate::store::theme::{Theme, panels_for};
use crate::ui::render::Paint;
use crate::ui::status;

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
    // Which row of the open list is highlighted; kept rather than the list
    // itself, which is a function of what has been typed. `None` anchors a
    // fresh list on its bottom row, the best match, beside the input line.
    pub(super) picked: Option<usize>,
    // The text the list was dismissed at. Any edit changes the text and the
    // list comes back, which is what makes Esc mean "not that" rather than
    // "never again".
    pub(super) dismissed_at: Option<String>,
    // A line was submitted through the editor. The echo is this side's — the
    // view is in hand — but the recall list is written by the surface, so it is
    // told once per line rather than left to guess.
    pub(super) submitted: bool,
    // What `/model` can complete to. A copy rather than a borrow of the
    // config: the loop holds the session mutably while it draws.
    pub(super) choices: Vec<Choice>,
    pub(super) lists: Lists,
    // The same copy, of the same list `/help` prints.
    pub(super) commands: Arc<Vec<Command>>,
    // The open panel, or None — one at a time, which is what one field
    // rather than one per panel is for. While it is up it owns the menu rows
    // and intercepts the menu keys before the editor does.
    pub(super) panel: Option<Panel>,
    // What a slash command last answered, or None. Read-only, and drawn over
    // the panel while it is up: a command asked for from a screen the panel
    // covers must still be answerable.
    pub(super) reply: Option<Reply>,
    // When the last `ctrl+l` was pressed, for the new-session double-tap.
    pub(super) last_l: Option<Instant>,
    pub(super) last_interrupt: Option<Instant>,
    // When the last Esc was pressed, for the rewind selector's double-tap.
    pub(super) last_esc: Option<Instant>,
    // The rewind selector's rows, session order, newest last. Empty is closed;
    // while it is open it replaces the completion list in the same rows.
    pub(super) rewind: Vec<MenuEntry>,
    // The @-completion cache, keyed by the query the walk was built for —
    // a directory walk sits behind every keystroke otherwise.
    pub(super) at_menu: Option<(String, Vec<crate::input::complete::FileEntry>)>,
    // The directory @ paths resolve against: the lane's workspace root.
    pub(super) at_root: std::path::PathBuf,
    pub(super) spinner: usize,
    // The modal keys, or None while they are off.
    pub(super) vim: Option<Vim>,
    // The segments each line shows, in the order the config named them.
    pub(super) live: Vec<Segment>,
    pub(super) done: Vec<Segment>,
    // What the bar says, in ring order. Rebuilt before every draw.
    pub(super) tabs: Vec<Tab>,
    // A note answering the last keypress, painted, and when it landed. It
    // takes the bar's row for `FLASH` and then goes — see `flash`.
    pub(super) flash: Option<(Line<'static>, Instant)>,
    pub(super) hovered_scrollback: Option<usize>,
    pub(super) row_targets: Vec<Target>,
    // Where the frame's regions landed last. The click handler reads them
    // back: a screen row only means something inside a named region.
    pub(super) regions: Regions,
    // Whether the live block lists every call in flight it draws or only the
    // newest with a count — the ones the summary row holds are drawn there
    // whatever this says. A click on a pending row flips it; it outlives the
    // calls.
    pub(super) live_tools_shown: bool,
    // The conversation alone. The one thing on this surface that hides the
    // editor rather than sitting over it; `browse.rs` draws it.
    pub(super) browsing: bool,
}

// What to call a checkout. The root answers to its directory name, as
// `worktree list` already names it — a fixed word would collide with a
// checkout that happens to be called that.
pub(super) fn lane_name(lane: &Lane) -> String {
    lane.worktree().map(str::to_string).unwrap_or_else(|| {
        lane.root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

// How a checkout shows on the bottom bar: whether it is the one in front, how
// its last run ended, its plain name and nothing more, or `Unopened` — one the
// disk has and no lane has opened. A run in flight shows nothing here — the bar
// answers what a checkout has finished, not what it is doing, and one working
// out of sight is still just a checkout.
//
// A lane that ended says so in colour and not in a glyph, and only until you
// look at the checkout it belongs to — `Done`/`Failed` are unread, not history.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Mark {
    Front,
    Done,
    Failed,
    Plain,
    Unopened,
}

impl Mark {
    // Whether a checkout is wearing a mark someone may not have read. The bar
    // drops a quiet one first — a coloured name off the edge is a run that
    // ended unseen.
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
    // What leaves with the checkout being left: the half-typed line, parked
    // in its lane's view — the editor is the surface's, and a line left
    // standing in it would be filed in whichever checkout came next — and
    // the state built against that lane: a flash, the rewind selector over
    // its transcript.
    pub(super) fn leave_lane(&mut self, view: &mut View) {
        view.draft = self.editor.take_composing();
        self.flash = None;
        self.rewind.clear();
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
            last_l: None,
            picked: None,
            dismissed_at: None,
            last_interrupt: None,
            last_esc: None,
            rewind: Vec::new(),
            at_menu: None,
            at_root: std::path::PathBuf::new(),
            vim: None,
            panel: None,
            reply: None,
            spinner: 0,
            live: default_live(),
            done: default_done(),
            tabs: Vec::new(),
            flash: None,
            hovered_scrollback: None,
            row_targets: Vec::new(),
            live_tools_shown: false,
            browsing: false,
            regions: Regions::default(),
        }
    }

    // The separator between lanes on the bar: the one every other line on
    // this surface uses, dimmed so the names it divides are what the eye
    // lands on.
    pub(super) fn paint_sep(paint: &Paint) -> Span<'static> {
        paint.span(&paint.theme.muted, icons::PART_SEP)
    }

    // The prompt sigil as the terminal shows it, colour and all.
    pub(super) fn paint_prompt(paint: &Paint, icon: &str) -> Span<'static> {
        paint.span(&paint.theme.prompt.color, icons::bar(icon))
    }

    // A theme style, as ratatui sees it.
    pub(super) fn rat_style(&self, s: &ThemeStyle) -> RStyle {
        crate::store::theme::style_to_ratatui(s)
    }

    // The rows above the input line: the calls in flight the summary row is
    // not drawing, the open stream, and the status line. The editor draws
    // separately, pinned to the bottom. With the rows comes the count that
    // leads them: the pending calls', which a click opens — only the producer
    // knows which rows those are.
    pub(super) fn live(
        &self,
        lane: &Lane,
        view: &View,
        row_holds: bool,
    ) -> (Vec<Line<'static>>, usize) {
        let width = self.screen.usable();
        let mut rows: Vec<Line<'static>> = Vec::new();
        // Browse mode shows the conversation and nothing that happened on the
        // way to it: the calls in flight, the thinking and the spinner all go.
        let thinking = view.surface.stream.kind == StreamKind::Reasoning;

        // The calls that keep a line here: a foldable one does not while the
        // summary row above is holding it. Collapsed the newest of them is
        // named with a count for the rest, opened one each, in the shape it
        // will fold into.
        let shown = drawn(&view.state.tools, row_holds);
        let mut pending = Vec::new();
        if !self.browsing {
            if self.live_tools_shown {
                pending.extend(shown.iter().copied().map(|t| pending_line(self.spinner, t)));
            } else if let Some(t) = shown.last() {
                let extra = if shown.len() > 1 {
                    format!(" (+{})", shown.len() - 1)
                } else {
                    String::new()
                };
                pending.push(format!("{}{extra}", pending_line(self.spinner, t)));
            }
        }
        rows.extend(pending.into_iter().flat_map(|line| {
            let muted = Line::from(self.paint.span(&self.paint.theme.muted, line));
            screen::fit(&muted, width)
        }));
        // The draw tags screen rows by index, and a long summary wraps:
        // count the rows the block takes, not the lines before they did.
        let pending_rows = rows.len();

        rows.extend(if thinking && self.browsing {
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

        if lane.is_running() && !self.browsing {
            let mut parts = status::parts(&self.live, &snapshot(lane, view));
            // A run that is stopping says so; an ordinary running line needs
            // no word for it — the spinner is what says the turn is on.
            if view.state.stopping {
                parts.push(format!("stopping{}", icons::ELLIPSIS));
            }
            let spin = if view.state.stopping {
                icons::SPIN_STOPPED
            } else {
                icons::SPINNER_FRAMES[self.spinner % icons::SPINNER_FRAMES.len()]
            };
            let line = format!("{spin} {}", parts.join(icons::PART_SEP));
            let muted = Line::from(self.paint.span(&self.paint.theme.muted, line));
            rows.extend(screen::fit(&muted, width));
        }

        (rows, pending_rows)
    }

    // The bar: one entry per checkout in `refresh_tabs` order, the lane's model
    // last. Too narrow, the front one stays and each dropped end keeps its `…`.
    pub(super) fn lane_bar(&self, model: &str, width: usize) -> Option<Line<'static>> {
        let theme = &self.paint.theme;
        let sep = self.tab_sep.width();
        // The model is the row's end whatever else it holds, so its share comes
        // off the checkouts' before they are fitted.
        let model = (!model.is_empty()).then(|| self.paint.span(&theme.muted, model.to_string()));
        let Some(front) = self.tabs.iter().position(|t| t.mark == Mark::Front) else {
            return model.map(Line::from);
        };
        let strip = model
            .as_ref()
            .map_or(width, |m| width.saturating_sub(m.width() + sep));
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
            // A quiet end goes before a marked one, and of two alike the one
            // farther from the front; a tie goes right, which leaves the
            // checkouts that opened earlier standing.
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
        // A strip wider than its share is cut here: a front checkout too long
        // for its share would otherwise eat the model rather than fold itself.
        let mut spans = screen::fit(&Line::from(spans), strip).remove(0).spans;
        if let Some(model) = model {
            spans.push(self.tab_sep.clone());
            spans.push(model);
        }
        screen::fit(&Line::from(spans), width).into_iter().next()
    }

    // The checkout a step from this one, wrapping at either end — where the
    // Normal `L`/`H` go; None when there is nowhere else to go. The ring is the
    // order the bar shows, which the bar is built to carry whole.
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
        context: &[String],
        theme: Arc<crate::store::theme::Theme>,
    ) {
        self.paint.theme = theme;
        self.bang_prompt = Self::paint_prompt(&self.paint, icons::BANG_SIGIL);
        self.tab_sep = Self::paint_sep(&self.paint);
        self.band = self.paint.band(&self.paint.theme.prompt.panel.input);
        self.show_mode();
        // The opening block is painted once at construction; rebuild it so a
        // /reload lands on the new theme instead of the old.
        let opening = Row::banner(context, &self.paint);
        let rest = view.surface.scrollback.split_off(view.surface.opened);
        view.surface.opened = opening.len();
        view.surface.scrollback = opening.into_iter().chain(rest).collect();
    }

    // Every copy of the config the surface keeps, brought up to date — the one
    // landing `/reload` and everything the settings panel does share.
    pub(super) fn adopt_config(&mut self, core: &Core, view: &mut View) {
        // The key map lives in two places; a reload has to reach both or the
        // screen keeps answering to the old bindings.
        if !Arc::ptr_eq(&self.keys, &core.keys) {
            self.keys = core.keys.clone();
        }
        // Likewise the completion list: /reload is allowed to define models —
        // and skills — the last one did not.
        self.choices = core.choices();
        // The theme takes the view it was painted into — the opening block is
        // painted in it — and every copy takes the terminal's band.
        let theme = following_terminal(&core.config.theme, self.tty_bg);
        if self.paint.theme.as_ref() != &theme {
            self.set_theme(view, &core.lane().resolved().context, Arc::new(theme));
        }
        if !Arc::ptr_eq(&self.commands, &core.commands) {
            self.commands = core.commands.clone();
        }
        self.set_vim(&core.config.vim);
        // And the segment lists, copied in at startup.
        self.live = core.config.status.live.clone();
        self.done = core.config.status.done.clone();
    }
}

// The config's theme with the prompt's bands following the terminal, unless the
// config named a band itself — a file that wanted a fixed colour wrote one.
pub(super) fn following_terminal(theme: &Theme, bg: Option<(u8, u8, u8)>) -> Theme {
    let mut theme = theme.clone();
    let Some(bg) = bg else {
        return theme;
    };
    let panels = panels_for(bg);
    let default = crate::store::theme::Panel::default();
    if theme.prompt.panel.input == default.input {
        theme.prompt.panel.input = panels.input;
    }
    if theme.prompt.panel.said == default.said {
        theme.prompt.panel.said = panels.said;
    }
    theme
}
