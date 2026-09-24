//! The interactive surface: one owner of the terminal for the whole session.
//!
//! The line-editing library that used to sit here owned the terminal only while
//! it was reading a line, which is what made a key press during a run
//! unreachable and left the renderer writing into a terminal nobody was
//! managing. Here a single loop holds raw mode from start to finish and
//! services three sources at once — the agent's events, the keyboard, and a
//! timer for the spinner — so nothing has to be bolted on beside it.

mod browse;
mod editor;
mod job;
mod menu;
mod mouse;
mod panel;
mod reply;
mod row;
mod screen;
mod scrollback;
mod stream;
mod term;
mod tool;
mod view;
mod vim;

use std::time::Instant;

use agent::Event;
use agent::session::EntryId;
use anyhow::Result;
use crossterm::event::Event as TermEvent;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::lane::{Lane, Run};
use crate::app::looping::{Cut, Round};
use crate::app::{self, App};
use crate::input::commands::{Choice, Command};
use crate::input::{self, Builtin, Fate, Intent, Rewound, Step};
use crate::store::icons;
use crate::store::keys::Keys;
use crate::store::listing::Listing;
use crate::store::status::{Segment, default_done, default_live};
use crate::store::theme::Style as ThemeStyle;
use crate::store::theme::{Theme, panels_for};
use crate::ui::render::Paint;
use crate::ui::status;
use crate::ui::tty;
use editor::Editor;
use panel::Panel;
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use reply::Reply;
use row::Row;
use screen::Screen;
use std::sync::Arc;

use job::Done;
use menu::{Lists, MenuEntry};
use mouse::{Regions, Target};
use scrollback::body;
use term::{HISTORY_KEEP, Hold, drop_shared_history, history_of, reader};
use tool::pending_line;
use view::{Queued, StreamKind, View, Views, front_view, prune_views, snapshot, view_at};
use vim::Vim;

// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking...";

// How close two Ctrl-C presses must be to read as one deliberate quit.
//
// Borrowed from pi, which uses the same 500ms. A latching flag looks simpler
// and is wrong: clear one half-typed line, type another, clear that — and the
// second clear reads as the second half of a double-tap and quits.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

// How long a flash stays on the bar row: long enough to read a short line
// without looking for it, short enough that a second try lands after it.
const FLASH: std::time::Duration = std::time::Duration::from_secs(1);

// The bar's own row: present whatever the bar has to say, because a row that
// came and went would take the transcript above it along on every key that
// missed. Every terminal tall enough to hold it gives it this one.
const BAR_H: usize = 1;

// One text each: three and two call sites had their own copy of these, and a
// reworded one would have drifted.
const NO_TRANSCRIPT: &str = "this checkout has no transcript — /new or /resume first";
const NOTHING_TO_REWIND: &str = "nothing to rewind to";

struct Ui {
    screen: Screen,
    keys: Arc<Keys>,
    editor: Editor,
    paint: Paint,
    // The prompt sigil, shared by the editor and the echoed lines.
    prompt: Span<'static>,
    // The band behind the input line, painted before its rows so the columns
    // the text does not reach carry it too.
    band: Option<RStyle>,
    // The terminal's own background, asked for once at startup: the prompt's
    // bands are a lift of it, and a terminal that will not say leaves both be.
    tty_bg: Option<(u8, u8, u8)>,
    // The same sigil for a `!` line, where the bang takes the icon's place.
    bang_prompt: Span<'static>,
    // The bar's separator, painted once beside the two above it: the bar
    // is rebuilt every frame and this depends only on the theme.
    tab_sep: Span<'static>,
    // Which row of the open list is highlighted; kept rather than the list
    // itself, which is a function of what has been typed. `None` anchors a
    // fresh list on its bottom row, the best match, beside the input line.
    picked: Option<usize>,
    // The text the list was dismissed at. Any edit changes the text and the
    // list comes back, which is what makes Esc mean "not that" rather than
    // "never again".
    dismissed_at: Option<String>,
    // A line was submitted through the editor. The echo is this side's — the
    // view is in hand — but the recall list is written by the surface, so it is
    // told once per line rather than left to guess.
    submitted: bool,
    // What `/model` can complete to. A copy rather than a borrow of the
    // config: the loop holds the session mutably while it draws.
    choices: Vec<Choice>,
    lists: Lists,
    // The same copy, of the same list `/help` prints.
    commands: Arc<Vec<Command>>,
    // The open panel, or None — one at a time, which is what one field
    // rather than one per panel is for. While it is up it owns the menu rows
    // and intercepts the menu keys before the editor does.
    panel: Option<Panel>,
    // What a slash command last answered, or None. Read-only, and drawn over
    // the panel while it is up: a command asked for from a screen the panel
    // covers must still be answerable.
    reply: Option<Reply>,
    // When the last `ctrl+l` was pressed, for the new-session double-tap.
    last_l: Option<Instant>,
    last_interrupt: Option<Instant>,
    // When the last Esc was pressed, for the rewind selector's double-tap.
    last_esc: Option<Instant>,
    // The rewind selector's rows, session order, newest last. Empty is closed;
    // while it is open it replaces the completion list in the same rows.
    rewind: Vec<MenuEntry>,
    // The @-completion cache, keyed by the query the walk was built for —
    // a directory walk sits behind every keystroke otherwise.
    at_menu: Option<(String, Vec<crate::input::complete::FileEntry>)>,
    // The directory @ paths resolve against: the lane's workspace root.
    at_root: std::path::PathBuf,
    spinner: usize,
    // The modal keys, or None while they are off.
    vim: Option<Vim>,
    // The segments each line shows, in the order the config named them.
    live: Vec<Segment>,
    done: Vec<Segment>,
    // What the bar says, in ring order. Rebuilt before every draw.
    tabs: Vec<Tab>,
    // A note answering the last keypress, painted, and when it landed. It
    // takes the bar's row for `FLASH` and then goes — see `flash`.
    flash: Option<(Line<'static>, Instant)>,
    hovered_scrollback: Option<usize>,
    row_targets: Vec<Target>,
    // Where the frame's regions landed last. The click handler reads them
    // back: a screen row only means something inside a named region.
    regions: Regions,
    // Whether the live block lists every call in flight it draws or only the
    // newest with a count — the ones the summary row holds are drawn there
    // whatever this says. A click on a pending row flips it; it outlives the
    // calls.
    live_tools_shown: bool,
    // The conversation alone. The one thing on this surface that hides the
    // editor rather than sitting over it; `browse.rs` draws it.
    browsing: bool,
}

// What to call a checkout. The root answers to its directory name, as
// `worktree list` already names it — a fixed word would collide with a
// checkout that happens to be called that.
fn lane_name(lane: &Lane) -> String {
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
enum Mark {
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
    fn quiet(self) -> bool {
        matches!(self, Mark::Plain | Mark::Unopened)
    }
}

// One checkout as the bar shows it, rebuilt before every draw — the bar is a
// view of state the surface does not own.
struct Tab {
    mark: Mark,
    name: String,
}

impl Ui {
    // What leaves with the checkout being left: the half-typed line, parked
    // in its lane's view — the editor is the surface's, and a line left
    // standing in it would be filed in whichever checkout came next — and
    // the state built against that lane: a flash, the rewind selector over
    // its transcript.
    fn leave_lane(&mut self, view: &mut View) {
        view.draft = self.editor.take_composing();
        self.flash = None;
        self.rewind.clear();
    }

    fn new(
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

    // The values both lines draw on, as this surface currently knows them.
    // The separator between lanes on the bar: the one every other line on
    // this surface uses, dimmed so the names it divides are what the eye
    // lands on.
    fn paint_sep(paint: &Paint) -> Span<'static> {
        paint.span(&paint.theme.muted, icons::PART_SEP)
    }

    // The prompt sigil as the terminal shows it, colour and all.
    pub(super) fn paint_prompt(paint: &Paint, icon: &str) -> Span<'static> {
        paint.span(&paint.theme.prompt.color, icons::bar(icon))
    }

    // A theme style, as ratatui sees it.
    fn rat_style(&self, s: &ThemeStyle) -> RStyle {
        crate::store::theme::style_to_ratatui(s)
    }

    // The rows above the input line: the calls in flight the summary row is
    // not drawing, the open stream, and the status line. The editor draws
    // separately, pinned to the bottom. With the rows comes the count that
    // leads them: the pending calls', which a click opens — only the producer
    // knows which rows those are.
    fn live(&self, lane: &Lane, view: &View, row_holds: bool) -> (Vec<Line<'static>>, usize) {
        let width = self.screen.usable();
        let mut rows: Vec<Line<'static>> = Vec::new();
        // Browse mode shows the conversation and nothing that happened on the
        // way to it: the calls in flight, the thinking and the spinner all go.
        let thinking = view.surface.stream.kind == StreamKind::Reasoning;

        // The calls that keep a line here: a foldable one does not while the
        // summary row above is holding it. Collapsed the newest of them is
        // named with a count for the rest, opened one each, in the shape it
        // will fold into.
        let shown = tool::drawn(&view.state.tools, row_holds);
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
    fn lane_bar(&self, model: &str, width: usize) -> Option<Line<'static>> {
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
    fn step_checkout(&self, lane: &Lane, forward: bool) -> Option<String> {
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

    fn set_theme(
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
    fn adopt_config(&mut self, core: &App, view: &mut View) {
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
            self.set_theme(view, &core.lane().context, Arc::new(theme));
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
fn following_terminal(theme: &Theme, bg: Option<(u8, u8, u8)>) -> Theme {
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

// What a `Step::Handled` leaves behind: its lines as the reply, and whatever
// the command changed under the surface. A free function because a run in
// flight lands them from inside its own borrow, where `self` is in pieces.
fn land_handled(ui: &mut Ui, core: &App, view: &mut View, rows: Listing) {
    ui.open_reply(rows);
    ui.adopt_config(core, view);
}

// A line submitted while the lane in front is working. What it may do is
// settled before it runs: `command` cannot be asked and then ignored, because
// asking is doing.
//
// Only queueing and refusing happen here. Anything that may run now goes back
// to the loop's own dispatch, so a command takes the same path whether or not
// a run is under way — and a `/worktree` that opens a lane is landed by the
// same code that lands it from an idle prompt.

// What the screen does to the lane itself, rather than asking the core to
// answer something. A key can mean these and a line cannot: there is no
// `/rewind` word, and `/new` and ctrl+l twice are one intent that goes the
// other way round. They are answered one step early — `admit` says whether the
// run in flight allows them — and then carried out here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Deed {
    // A key that moved the editor and nothing else.
    Nothing,
    // The line being typed wants `$EDITOR`. The surface's own: the editor takes
    // the terminal, which only the surface knows how to give away.
    External,
    // The rewind selector wants everywhere the session can go back to: only
    // the surface can put a screen over the transcript.
    Rewind,
    // A row chosen from it: the conversation rewinds there, and what the row
    // was decides whether it is kept or unsent.
    To(agent::session::EntryId),
    // The settings panel's edit line kept a value: the session takes it, and
    // the file does not — `SettingWrite` is what moves it to the file.
    SettingEdit(String, String),
    // The panel's space: the session value replaces the file's line.
    SettingWrite(String),
    // The panel's r: the file's value takes the session back.
    SettingRevert(String),
    // Stop the run: from esc, or from the phone's `/stop`.
    Interrupt,
    // Esc caught a prompt on its way out: stop the run, then unsend it.
    Unsend,
}

impl Deed {
    // Whether it may happen now. Only the two that rewrite the transcript
    // care: the run in flight is writing it.
    fn fate(&self) -> Fate {
        match self {
            Deed::Rewind | Deed::To(_) => {
                Fate::Refused("rewinding needs the transcript this run is writing — esc first")
            }
            _ => Fate::Now,
        }
    }
}

// What a key asked for. The core answers an `Intent` however it arrived — the
// keyboard, the phone, a typed line — and `Deed` is what the screen keeps for
// itself, so the two never have to be told apart again downstream.
#[derive(Debug)]
enum Asked {
    Core(Intent),
    Own(Deed),
}

enum Wake {
    // Something to carry out, once the select's borrows are gone.
    Do(Asked),
    // A turn ended and has to be settled, whichever lane it belongs to.
    Turn(Done),
    // Something only the screen cares about.
    Nothing,
    // Leave. Not a `break` at the arm: the way out has lanes to settle.
    Leave,
}

pub struct Tui {
    core: App,
    // What each lane looks like, keyed by lane token. Held here rather than on
    // the lane because a screen is the surface's: the lane list reorders and
    // drops lanes, and a screen joined to a lane by identity cannot end up
    // drawn for the wrong one when it does.
    views: Views,
    ui: Ui,
    events: UnboundedReceiver<TermEvent>,
    // Stops the reader while a child holds the terminal.
    hold: Hold,
    bridge: app::wechat::Bridge,
}

impl Tui {
    pub fn new(mut core: App, keys: Arc<Keys>, bridge: app::wechat::Bridge) -> Result<Self> {
        // The screen first, for raw mode: the answer to the background query
        // carries no newline, so a cooked read would wait for one forever.
        let screen = Screen::new()?;
        let asked = tty::background();
        let paint = Paint::with_theme(
            true,
            Arc::new(following_terminal(&core.config.theme, asked.bg)),
        );
        let mut ui = Ui::new(
            screen,
            keys,
            core.choices(),
            core.commands.clone(),
            Lists::new(core.store.clone(), core.lane_mut().root().to_path_buf()),
            paint,
        );
        ui.tty_bg = asked.bg;
        // Asking the terminal read it, so it is typed in here: the keyboard
        // reader would never see those bytes again.
        ui.editor.insert_str(&asked.typed);
        ui.at_root = core.lane().root().to_path_buf();
        ui.live = core.config.status.live.clone();
        ui.done = core.config.status.done.clone();
        ui.set_vim(&core.config.vim);
        let context = core.lane().context.clone();
        let mut opening = View::opening(&context, &ui.paint);
        opening.model = core.lane().model().to_string();
        let mut views = Views::new();
        views.insert(core.lane().token(), opening);
        drop_shared_history();
        {
            let lines = history_of(&core.store, core.lane().root());
            ui.editor.seed_history(lines);
        }
        // A resumed session shows its transcript from the start: the whole
        // screen is rebuildable now, so there is no reason to hide it.
        let token = core.lane().token();
        if let Some(session) = core.lane().session().filter(|s| !s.is_empty()) {
            ui.rebuild(view_at(&mut views, token), session);
        }
        let (events, hold) = reader();
        Ok(Self {
            core,
            views,
            ui,
            events,
            hold,
            bridge,
        })
    }

    // A surface on an in-memory screen, for tests that drive the loop's
    // settle side. No reader thread and no history file: the terminal the
    // test runner owns is not this test's to touch.
    #[cfg(test)]
    fn on_test_screen(mut core: App, keys: Arc<Keys>) -> Self {
        let paint = Paint::with_theme(false, Arc::new(core.config.theme.clone()));
        let mut ui = Ui::new(
            screen::Screen::test(80, 24),
            keys,
            core.choices(),
            core.commands.clone(),
            Lists::new(core.store.clone(), core.lane_mut().root().to_path_buf()),
            paint,
        );
        ui.at_root = core.lane_mut().root().to_path_buf();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            core,
            views: Views::new(),
            ui,
            events: rx,
            hold: Hold::default(),
            bridge: app::wechat::Bridge::new(),
        }
    }

    // A line whose answer will land under it: onto the screen, at the newest
    // row, and into the history file.
    //
    // One place rather than one per door. A fresh turn starts at the newest
    // row — a view scrolled up to read would otherwise stream output out of
    // sight — and the history is written per line rather than on the way out,
    // because quitting with two Ctrl-Cs skips every tidy exit path there is.
    // Every door a line can be submitted through calls this — the keyboard and
    // the phone alike — because a line the user cannot see they sent is one
    // they send twice.
    //
    // Which lines those are is `Intent::echoed`'s to say: a command answers
    // over the menu in the reply, which is dismissed rather than kept, and a
    // row left above an answer that never comes is the question standing alone.
    fn echo_sent(&mut self, line: &str) {
        let view = front_view(&mut self.views, self.core.lane());
        self.ui.submit(view, line);
        view.surface.scroll = 0;
        self.save_history();
    }

    // Best effort: losing a recall list is not worth a message on the way out.
    fn save_history(&self) {
        let path = self.core.store.history_path(self.core.lane().root());
        let all = self.ui.editor.history();
        let keep = &all[all.len().saturating_sub(HISTORY_KEEP)..];
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, editor::encode(keep));
    }

    // Sessions change on the commands that create, delete or switch them;
    // refresh the copy the completion menu reads.
    // A turn or a switch can change what `/resume` would list. Dropped
    // rather than recomputed: whoever asks next pays, and most of the time
    // nobody does.
    fn refresh_sessions(&mut self) {
        self.ui.lists.forget();
    }

    // Bring the screen into step with the lanes after a command. A switch
    // parks the line being typed in the view of the lane it was typed at
    // and takes the lane in front's own parked line back up; the lists
    // follow, and what that lane posted while away is replayed here.
    fn reconcile(&mut self, was: usize) {
        if was == self.core.current {
            return;
        }
        self.ui
            .leave_lane(view_at(&mut self.views, self.core.lanes[was].token()));
        let parked = std::mem::take(&mut front_view(&mut self.views, self.core.lane()).draft);
        self.ui.editor.set_line(&parked);
        self.ui.lists.at(self.core.lane().root());
        self.ui.at_root = self.core.lane().root().to_path_buf();
        // Recall follows the checkout for the same reason the lists do. The
        // line just typed is already filed: `save_history` runs per line, and
        // ran while this lane was still the one in front.
        let lines = history_of(&self.core.store, self.core.lane().root());
        self.ui.editor.seed_history(lines);
        // A lane opened later has no banner yet, and the files it stands on
        // are its own. Nothing reads the screen here: the events below ask for
        // it again, each in the lane it belongs to.
        view::opened(&mut self.views, self.core.lane(), &self.ui.paint);
        // What this lane's run posted while nobody was looking, in the order it
        // arrived. Not through the bridge: the phone follows the lane in front,
        // and replaying an hour of another one into it would be a second
        // conversation arriving out of nowhere.
        let replayed = self.core.lane_mut().take_pending();
        let heard = !replayed.is_empty();
        for event in replayed {
            let view = front_view(&mut self.views, self.core.lane());
            self.ui.on_event(self.core.lane_mut(), view, event);
        }
        // And the end of it, if it reached one out of sight.
        if let Some((out, unsend)) = self.core.lane_mut().take_ended() {
            self.close_run(out);
            // Esc asked for the prompt back before the screen moved on. The
            // asking does not go stale because the answer arrived late.
            if unsend && let Some(id) = self.core.lane().session().and_then(|s| s.last_ask()) {
                self.rewind_turn(id);
            }
        }
        // The rows that replay filed — the tally line a run ended on, a warning
        // about it — reached the transcript after their own run was saved, and
        // this lane may not run again before it is left: written here, where the
        // screen already has them, rather than left for a turn that may never
        // come.
        if heard && let Err(e) = self.core.save_lane(self.core.current) {
            self.say_of(
                self.core.current,
                format!("warning: the transcript was not saved: {e}"),
            );
        }
    }

    // A handled command's lines are its answer, and the answer goes to the
    // reply — the screen is not a log of what the user typed at the interface.
    fn land_lines(&mut self, rows: Listing) {
        land_handled(
            &mut self.ui,
            &self.core,
            front_view(&mut self.views, self.core.lane()),
            rows,
        );
    }

    fn land_swap(&mut self, said: Listing) {
        if let Some(session) = self.core.lane().session() {
            self.ui
                .rebuild(front_view(&mut self.views, self.core.lane()), session);
        }
        // `at` forgets both lists, so it stands in for `refresh_sessions`: a
        // swap that did not move repeats the root, and drops them either way.
        self.ui.lists.at(self.core.lane_mut().root());
        self.ui.open_reply(said);
    }

    // Take what every lane's run has posted since the last look: into the view
    // when the lane is in front, into its own backlog when it is not.
    //
    // A lane out of sight is not drawn — only its transcript has to be kept
    // whole. What arrived meanwhile is replayed when it comes back.
    async fn serve_lanes(&mut self) {
        for at in 0..self.core.lanes.len() {
            while let Ok(event) = self.core.lanes[at].inbox().try_recv() {
                if at == self.core.current {
                    self.bridge.observe(&event).await;
                    let view = front_view(&mut self.views, self.core.lane());
                    // A retry is transport news, not a step of the answer: it
                    // takes the bar a moment, after the half-stream is landed.
                    if matches!(event, Event::Retrying { .. }) {
                        self.ui.close(view);
                        self.ui.flash_event(&event);
                        continue;
                    }
                    self.ui.on_event(&mut self.core.lanes[at], view, event);
                } else {
                    // Deltas arrive thousands at a time and the backlog is
                    // replayed in one go: a run of them folded into one keeps
                    // it the size of what was written rather than of how many
                    // pieces it came in, and the view cannot tell the two apart.
                    let lane = &mut self.core.lanes[at];
                    let folded = match (lane.pending.last_mut(), &event) {
                        (Some(Event::TextDelta(prev)), Event::TextDelta(next)) => {
                            prev.push_str(next);
                            true
                        }
                        (Some(Event::ReasoningDelta(prev)), Event::ReasoningDelta(next)) => {
                            prev.push_str(next);
                            true
                        }
                        _ => false,
                    };
                    if !folded {
                        lane.pending.push(event);
                    }
                }
            }
        }
    }

    // Rebuild the bar before every draw: a run that ended out of sight has to
    // reach the screen without anyone asking, and a step walks the order this
    // builds — the bar and the ring are one list.
    fn refresh_tabs(&mut self) {
        let current = self.core.current;
        let mut tabs: Vec<Tab> = self
            .core
            .lanes
            .iter()
            .enumerate()
            .map(|(at, lane)| Tab {
                // In front is in front, whatever it is doing: the run row above
                // already says whether this one is working.
                mark: if at == current {
                    Mark::Front
                } else {
                    match lane.run() {
                        // A round of the loop already waits in this lane's
                        // queue, so it is not a lane that finished.
                        Run::Ended { out: Ok(_), .. } if self.round_waiting(lane) => Mark::Plain,
                        Run::Ended { out: Ok(_), .. } => Mark::Done,
                        Run::Ended { out: Err(_), .. } => Mark::Failed,
                        Run::Running { .. } | Run::Idle => Mark::Plain,
                    }
                },
                name: lane_name(lane),
            })
            .collect();
        // The checkouts no lane has open, in git's order: the ring is not the
        // lanes alone. `Lists` holds the read, one fork per list.
        for tree in self.ui.lists.worktrees_read().unwrap_or_default() {
            if tabs.iter().any(|t| t.name == tree.name) {
                continue;
            }
            tabs.push(Tab {
                mark: Mark::Unopened,
                name: tree.name.clone(),
            });
        }
        self.ui.tabs = tabs;
    }

    // Whether the lane's loop has another round waiting in its queue: the run
    // that ended is then one round of a series, not the end of one.
    fn round_waiting(&self, lane: &Lane) -> bool {
        self.views.get(&lane.token()).is_some_and(|view| {
            view.queued
                .iter()
                .any(|q| matches!(q, Queued::Round { .. }))
        })
    }

    // Drop lanes whose checkout was deleted outside pi — idle ones only, a
    // running or looping lane still answering to the index it was given.
    //
    // Silent: the lane going off the bar is what says the checkout is gone.
    fn drop_vanished_lanes(&mut self) {
        let mut gone: Vec<usize> = Vec::new();
        // Back to front, stopping at a working lane: removing one before it
        // would shift the index a run in flight reports back by. That lane's
        // turn over, the next pass drops what this one left.
        for (at, lane) in self.core.lanes.iter().enumerate().rev() {
            if lane.is_running() || lane.looping().is_some() {
                break;
            }
            if at == self.core.current {
                continue;
            }
            // Only a lane with a checkout of its own can go this way: the one
            // pi was started in is no worktree, and one still on disk stands.
            if lane.worktree().is_none() || lane.root().exists() {
                continue;
            }
            gone.push(at);
        }
        if gone.is_empty() {
            return;
        }
        // Already highest first, so earlier indices stay put while they go.
        for at in gone {
            self.core.remove_lane(at);
        }
        // The ring's list is cached; a vanished checkout must not stay in it
        // for a later step to offer — and re-create — by its stale name.
        self.ui.lists.forget();
    }

    // Carry the lane's loop past a round that has just ended: queue the next
    // one, or hand back the row that says why there is no next one. The caller
    // lands it, once the screen it belongs on has settled.
    //
    // Every ending keeps a row on its own lane's screen; a round beginning
    // says nothing, the note it hands the model landing as that round's row.
    //
    // What decides is the tree, never the model: a round that changed a file
    // is a round whose work was not finished, and one that changed nothing
    // has nothing left to do. Asking the model instead would hand back the
    // judgement this exists to take away from it.
    fn step_loop(&mut self, lane: usize, cut: Option<Cut>) -> Option<Line<'static>> {
        let cap = self.core.config.loop_cap();
        let round = self.core.lanes[lane].loop_step(cut, cap)?;
        let said = match round {
            Round::Again { goal } => {
                // How far the loop has got reaches the model as a note, not
                // glued to the goal: the goal must stay exactly what `read`
                // would parse. The note is the row this round lands as.
                let note = self.core.lanes[lane]
                    .looping
                    .as_ref()
                    .map(|l| l.note.clone())
                    .unwrap_or_default();
                view_at(&mut self.views, self.core.lanes[lane].token())
                    .queued
                    .push(Queued::Round { goal, note });
                return None;
            }
            Round::Cut(Cut::Stopped) => "loop stopped — the round was cut short".to_string(),
            Round::Cut(Cut::Failed) => "loop stopped — the round failed".to_string(),
            Round::Cut(Cut::Unsent) => "loop stopped — the prompt came back".to_string(),
            Round::Quiet => "loop done — that round changed nothing".to_string(),
            Round::Oscillating => "loop stopped — a round undid the work before it".to_string(),
            Round::Thin => "loop stopped — rounds are only nibbling now".to_string(),
            Round::Capped(n) => {
                format!("loop stopped at loop_max_rounds ({n}) — rounds were still changing files")
            }
        };
        Some(said.into())
    }

    // A lane's news goes into that lane's own screen, never the one in front:
    // it is read back beside the conversation it happened to. A lane never
    // drawn gets its opening block first, so `reconcile` cannot replace the row.
    fn say_of(&mut self, lane: usize, what: impl Into<Line<'static>>) {
        let view = view::opened(&mut self.views, &self.core.lanes[lane], &self.ui.paint);
        self.ui.say_line(view, what.into());
    }

    // The deeds the screen keeps for itself. They are here rather than in
    // `dispatch` because only the surface can move the screen, the keyboard and
    // the process — and only the surface knows which lane is in front.
    async fn carry(&mut self, deed: Deed) {
        match deed {
            Deed::Nothing => {}
            Deed::Interrupt => self.stop_current(false),
            Deed::Unsend => self.stop_current(true),
            Deed::Rewind => self.open_rewind(),
            Deed::To(id) => self.rewind_turn(id),
            Deed::SettingEdit(path, value) => {
                let said = self.core.edit(&path, &value);
                self.land_setting(said);
            }
            Deed::SettingWrite(path) => {
                let said = self.core.write_to_file(&path);
                self.land_setting(said);
            }
            Deed::SettingRevert(path) => {
                let said = self.core.revert(&path);
                self.land_lines(Listing::say(said));
                self.reload_panel();
            }
            Deed::External => self.edit_externally().await,
        }
    }

    // The one gate every input passes: a key, the phone, or an intent coming
    // back off the queue, so two ways of asking the same thing cannot get two
    // different answers.
    //
    // The answer is in two halves and they live apart on purpose: whether a
    // run is in flight is state, and is read here; what a question may do
    // while one is, is a property of the question, and `fate` holds it. An
    // idle lane admits everything, which is why `fate` never has to mention
    // idleness — and why it is asked only once a run is known to be there.
    fn admit(&mut self, asked: Asked) -> Wake {
        if !self.core.lane().is_running() {
            return Wake::Do(asked);
        }
        let intent = match asked {
            // A deed is the screen's own move: it is never queued and never
            // steered, because waiting is one of the four answers about lines.
            Asked::Own(deed) => match deed.fate() {
                // A key, not a command: a deed's refusal is the answer to a
                // press, and the reply is for what a slash command answered.
                Fate::Refused(why) => {
                    self.ui.flash(why);
                    return Wake::Nothing;
                }
                Fate::Now => return Wake::Do(Asked::Own(deed)),
                Fate::Queued | Fate::Steered(_) => {
                    unreachable!("a deed waits for nothing")
                }
            },
            Asked::Core(intent) => intent,
        };
        match intent.fate() {
            Fate::Now => Wake::Do(Asked::Core(intent)),
            Fate::Queued => {
                front_view(&mut self.views, self.core.lane())
                    .queued
                    .push(Queued::Line(intent));
                Wake::Nothing
            }
            Fate::Steered(text) => {
                // A `!` or a `/compact` holds the lane: nothing is listening,
                // so the line waits for it the way every line used to.
                let Some(steer) = self.core.lane().steer().cloned() else {
                    front_view(&mut self.views, self.core.lane())
                        .queued
                        .push(Queued::Line(intent));
                    return Wake::Nothing;
                };
                // Spends the chance to unsend, exactly as the model's first
                // word does: esc now means stop. Without this, esc after a
                // line was said takes the prompt back and the line — never
                // heard, handed back at `finish` — starts a turn of its own,
                // which is the opposite of what the user just asked for.
                front_view(&mut self.views, self.core.lane())
                    .state
                    .committed = true;
                steer.say(text);
                Wake::Nothing
            }
            // A command refused because a run is in flight. It was typed, so
            // its refusal is that command's answer.
            Fate::Refused(why) => {
                self.ui.open_reply(Listing::say([why]));
                Wake::Nothing
            }
        }
    }

    // A settings-panel action landed: its lines go to the scrollback and the
    // panel sees the fresh rows. A refusal stays in the panel, beside the
    // edit that earned it; there is nowhere else for it to be read.
    fn land_setting(&mut self, said: Result<Vec<String>, String>) {
        match said {
            Ok(lines) => {
                // Said into the scrollback above the panel rather than put up
                // as a reply: a reply draws in the menu region the panel is
                // drawing in and takes its keys, and the panel answers every
                // commit it is given — one `esc` per space or `r` is not a
                // panel that can be used.
                let view = front_view(&mut self.views, self.core.lane());
                for line in lines {
                    self.ui.say(view, line);
                }
                self.ui.adopt_config(&self.core, view);
                self.reload_panel();
            }
            Err(why) => {
                if let Some(panel) = &mut self.ui.panel {
                    panel.refuse(why);
                }
            }
        }
    }

    // Re-read the open panel's rows after a commit changed them underneath.
    fn reload_panel(&mut self) {
        if let Some(panel) = &mut self.ui.panel {
            panel.refresh(self.core.setting_rows());
        }
    }

    // Stop the run in front, if there is one. `unsend` also takes the prompt
    // back once it has stopped.
    fn stop_current(&mut self, unsend: bool) {
        // Said here rather than at the callers: the state is the only thing
        // that knows, and every way of asking to stop arrives through it.
        if !self.core.lane_mut().stop(unsend) {
            self.ui.flash("nothing running to stop");
            return;
        }
        front_view(&mut self.views, self.core.lane()).state.stopping = true;
    }

    /// Drive the terminal until the user leaves.
    ///
    /// No event channel is handed in any more: each lane owns the one its runs
    /// post to, which is what lets a lane the screen has moved on from keep
    /// working without its output landing on somebody else's view.
    pub async fn run(mut self) -> Result<()> {
        // Every lane's runs report here when they end. One channel rather than a
        // handle per lane: the loop waits on it like any other source.
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
        let mut tick = tokio::time::interval(status::SPIN);
        // The branch is off while nothing runs, so the interval falls behind
        // the clock; bursting to catch up would spin the loop the moment a run
        // starts. One late tick, then the ordinary cadence.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            self.serve_lanes().await;
            // Before the bar rebuilds, so a vanished tab goes with its lane.
            self.drop_vanished_lanes();
            prune_views(&self.core, &mut self.views);
            self.refresh_tabs();
            let view = front_view(&mut self.views, self.core.lane());
            self.ui.flush(self.core.lane(), view);
            // After the frame, not before it: a fork here would hold the
            // screen blank. Read again whenever something dropped the list.
            if self.ui.lists.worktrees_read().is_none() {
                let _ = self.ui.lists.worktrees();
                continue;
            }
            let running = self.core.lane().is_running();
            let anywhere = self.core.lanes.iter().any(|lane| lane.is_running());
            // Whether what is about to run is a loop's own round: the queue
            // says so, and the run that ends decides whether the loop goes on.
            let mut from_loop = false;
            // A queued line waits for the lane it was aimed at to come free.
            let woke = if view.queued.is_empty() || running {
                // Every branch must be cancel-safe: a loser is dropped mid-poll.
                // `recv()` and `tick()` are; a blocking read gets its own thread.
                tokio::select! {
                    Some(done) = done_rx.recv() => Wake::Turn(done),
                    // Only while something runs, or a flash is up — an idle
                    // loop waking ten times a second has nothing to spin.
                    _ = tick.tick(), if anywhere || self.ui.flash.is_some() => {
                        self.ui.spinner += 1;
                        Wake::Nothing
                    }
                    key = self.events.recv() => match key {
                        Some(key) => {
                            let lane = self.core.lane();
                            let view = front_view(&mut self.views, lane);
                            let intent = self.ui.key(lane, view, key, running);
                            if self.ui.took_submit() {
                                self.save_history();
                            }
                            self.admit(intent)
                        }
                        None => Wake::Leave,
                    },
                    msg = self.bridge.rx.recv() => match msg {
                        // The phone types at the lane in front, like a hand,
                        // and its `/stop` is esc. Same intents, same gate, so
                        // they cannot drift apart.
                        Some(app::wechat::Inbound::Text { text }) => {
                            let intent = input::read(&text, &self.core.commands);
                            if intent.echoed() {
                                self.echo_sent(&text);
                            }
                            self.admit(Asked::Core(intent))
                        }
                        Some(app::wechat::Inbound::Stop) => self.admit(Asked::Own(Deed::Interrupt)),
                        // The QR, an error, a way out of one: on the lane the
                        // bridge follows, where it lasts and can be re-read.
                        Some(app::wechat::Inbound::Notice(text)) => {
                            self.ui.say(front_view(&mut self.views, self.core.lane()), text);
                            Wake::Nothing
                        }
                        // The bridge saying it is up: one row for a moment.
                        Some(app::wechat::Inbound::Flash(text)) => {
                            self.ui.flash(text);
                            Wake::Nothing
                        }
                        None => Wake::Nothing,
                    },
                }
            } else {
                // One at a time, each still the intent it was read as. Joined
                // as lines, a command and a prompt became one line and `read`
                // saw only the first word.
                match front_view(&mut self.views, self.core.lane())
                    .queued
                    .remove(0)
                {
                    Queued::Line(intent) => Wake::Do(Asked::Core(intent)),
                    Queued::Round { goal, note } => {
                        // The loop that queued this may have been stopped since.
                        // Running it then would be a turn nobody asked for, and
                        // one that reads on screen as if it had been typed.
                        if self.core.lane().looping().is_none() {
                            continue;
                        }
                        from_loop = true;
                        let view = front_view(&mut self.views, self.core.lane());
                        self.ui.submit(view, &goal);
                        view.surface.scroll = 0;
                        if !note.is_empty() {
                            self.core.lane_mut().push_note(&note);
                        }
                        Wake::Do(Asked::Core(input::read(&goal, &self.core.commands)))
                    }
                }
            };
            // Out here, where all of `self` is free again.
            let asked = match woke {
                Wake::Turn(done) => {
                    self.settle(done).await;
                    continue;
                }
                Wake::Nothing => continue,
                Wake::Leave => break,
                Wake::Do(asked) => asked,
            };
            // What the surface answers for itself: the screen, the keyboard
            // and the process are not `App`'s to move.
            let intent = match asked {
                Asked::Own(deed) => {
                    self.carry(deed).await;
                    continue;
                }
                // A loop's own round: echoed and read like a typed line, and
                // marked so the turn it starts is the one the loop counts.
                // A key that means a command — `ctrl+l` twice is `/new` —
                // arrives already read.
                Asked::Core(ready) => ready,
            };
            // A loop is the surface's: it arms the lane, then puts its goal
            // back through the door as a typed line — so what runs each round
            // is read exactly as it would be if it had been typed.
            if let Intent::Builtin(Builtin::Loop(goal)) = intent {
                let goal = goal.trim().to_string();
                if goal.is_empty() {
                    // The round already queued goes with it: run after a stop,
                    // it is a turn nobody asked for and it reads as a typed one.
                    front_view(&mut self.views, self.core.lane())
                        .queued
                        .retain(|q| !matches!(q, Queued::Round { .. }));
                    match self.core.lane_mut().take_looping() {
                        // A loop really ended: that belongs in the transcript.
                        Some(l) => {
                            let said =
                                format!("loop stopped after {} round(s) of `{}`", l.round, l.goal);
                            self.ui
                                .say(front_view(&mut self.views, self.core.lane()), said);
                        }
                        // Nothing ended — a note about the line, not the lane.
                        None => self.ui.open_reply(Listing::say([concat!(
                            "no loop here — /loop <line> runs one again while ",
                            "it keeps changing files"
                        )])),
                    }
                    continue;
                }
                // Refused rather than replacing: the round already queued would
                // still run, and it would be counted against the new loop.
                if let Some(l) = self.core.lane().looping() {
                    let said = format!(
                        "`{}` is already looping here — /loop to stop it first",
                        l.goal
                    );
                    self.ui.open_reply(Listing::say([said]));
                    continue;
                }
                if matches!(
                    input::read(&goal, &self.core.commands),
                    Intent::Builtin(Builtin::Loop(_))
                ) {
                    self.ui
                        .open_reply(Listing::say(["a loop cannot be its own goal"]));
                    continue;
                }
                self.core.lane_mut().loop_start(goal.clone());
                front_view(&mut self.views, self.core.lane())
                    .queued
                    .push(Queued::Round {
                        goal,
                        note: String::new(),
                    });
                continue;
            }
            // Bare `/settings` opens a panel rather than printing the
            // read-only list.
            if matches!(intent, Intent::Builtin(Builtin::Settings(ref rest)) if rest.trim().is_empty())
            {
                let rows = self.core.setting_rows();
                self.ui.panel = Some(Panel::new(rows, &self.core.config.vim));
                continue;
            }
            let was = self.core.current;
            let step = self.core.dispatch(intent);
            self.reconcile(was);
            if from_loop {
                // `was`, not whichever lane is in front now: a step may move
                // the surface to another checkout, and the loop belongs to the
                // one that queued the round. Addressed by index, the lane left
                // behind cannot be left armed and unreachable.
                //
                // A round is a turn, and only a step that starts one leaves
                // anything to measure. A line that answers on the spot would
                // leave the loop armed, and the next turn from anywhere would
                // be taken for its round.
                if matches!(step, Step::Prompt { .. } | Step::Bash(_)) {
                    self.core.lanes[was].loop_running();
                } else if let Some(stale) = self.core.lanes[was].take_looping() {
                    let said = format!("loop ended — `{}` starts no turn to measure", stale.goal);
                    self.say_of(was, said);
                }
            }
            match step {
                Step::Quit => break,
                // A refusal is an answer: it was asked for by a line, so it
                // goes where every other answer does.
                Step::Flash(line) => self.ui.open_reply(Listing::say([line])),
                Step::Bash(command) => self.start_bash(command, &done_tx),
                Step::Swap(said) => self.land_swap(said),
                Step::Worktrees(lines) => {
                    self.land_lines(lines);
                    // A checkout went; the cached list would go on offering it.
                    self.ui.lists.forget();
                }
                Step::Handled(lines) => self.land_lines(lines),
                Step::Compact(focus) => self.start_compact(focus, &done_tx),
                Step::Wechat(cmd) => {
                    // The command's own answer, failure included: `/wechat on`
                    // that could not connect is still what `/wechat on` said.
                    let said = match cmd {
                        input::WechatCmd::Status => self.bridge.status(),
                        // Only local locks and a client build await here; the
                        // login and long poll already run in their own tasks.
                        input::WechatCmd::On => match self.bridge.on().await {
                            Ok(said) => said,
                            Err(e) => vec![format!("wechat: {e:#}")],
                        },
                        input::WechatCmd::Off => self.bridge.off(),
                    };
                    self.ui.open_reply(Listing::say(said));
                }
                // What was submitted while the run worked is taken up by the
                // top of this loop, one entry at a time and each read as what
                // it is. Draining it here instead meant everything queued
                // became the next prompt, whatever it had been typed as.
                Step::Prompt { send, typed } => self.start_turn(send, typed, &done_tx),
            }
        }
        self.save_history();
        self.settle_all(&mut done_rx).await;
        Ok(())
    }

    // Cut the transcript at an entry — chosen from the selector, or the
    // prompt an Esc took back — and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        match self.core.rewind_to(id) {
            Ok(Rewound::Nothing) => {
                self.ui.flash(NOTHING_TO_REWIND);
            }
            Ok(outcome) => {
                // The transcript is the source of truth again: rebuild the
                // whole view from it, so the screen returns to the node the
                // conversation did instead of keeping the forgotten turns.
                // It clears anything said before it: hence the notice after.
                if let Some(session) = self.core.lane().session() {
                    self.ui
                        .rebuild(front_view(&mut self.views, self.core.lane()), session);
                }
                let said = match outcome {
                    Rewound::Unsent(_) if !self.ui.editor.is_empty() => {
                        "unsent — the line you were typing stands; Up recalls it".to_string()
                    }
                    // Unsent is half-typed, not gone: the text goes back to
                    // the editor, and the cursor on it says what happened.
                    Rewound::Unsent(text) => {
                        self.ui.editor.set_line(&text);
                        return;
                    }
                    Rewound::Kept | Rewound::Nothing => {
                        let at = self
                            .core
                            .lane_mut()
                            .session
                            .as_ref()
                            .and_then(|s| s.last_node())
                            .map(|n| crate::store::text::clip(n.show(), 60))
                            .filter(|t| !t.is_empty())
                            .map(|t| format!(" — the transcript now ends at {t}"))
                            .unwrap_or_default();
                        format!("rewound{at}")
                    }
                };
                self.ui
                    .say_muted(front_view(&mut self.views, self.core.lane()), &said);
            }
            Err(e) => {
                self.ui.say(
                    front_view(&mut self.views, self.core.lane()),
                    format!("warning: the transcript was not saved: {e}"),
                );
            }
        }
    }

    // Open the rewind selector on what the user said: a rewind takes a prompt
    // back, and an answer is a place the conversation carries on from.
    fn open_rewind(&mut self) {
        let rows: Vec<MenuEntry> = self
            .core
            .lane_mut()
            .session
            .as_ref()
            .map(|s| s.rewind_nodes())
            .unwrap_or_default()
            .into_iter()
            .map(|node| MenuEntry::Message {
                id: node.id(),
                show: crate::store::text::clip(node.show(), 60),
                help: "you — unsends it",
            })
            .collect();
        if rows.is_empty() {
            self.ui.flash(NOTHING_TO_REWIND);
            return;
        }
        self.ui.open_rewind(rows);
    }
}

#[cfg(test)]
mod tests {
    use super::scrollback::{Folds, ScrollbackRows, absorb_growth, scrollback_from};
    use super::{
        Asked, Deed, Intent, Panel, Queued, Row, Target, View, following_terminal, view_at,
    };
    use crate::app::App;
    use crate::app::lane::{Lane, Run};
    use crate::app::looping::{Cut, Round};
    use crate::input::Builtin;
    use crate::input::Fate;
    use crate::input::commands::{Choice, Command, Source};
    use crate::store::icons;
    use crate::store::keys::{Keys, Mode};
    use crate::store::listing::Listing;
    use crate::store::session::Store;
    use crate::store::settings::row;
    use crate::store::status::Segment;
    use crate::store::theme::{Color, Theme};
    use crate::ui::render::Paint;
    use crate::ui::tui::screen::{self, plain};
    use agent::Event;
    use ratatui::text::Line;

    // Both scrollback producers draw block ids from one counter. They used
    // not to: a rebuilt block was always `0`, which held only while nothing
    // looked one up — and `streaming_row` and `stream_fold` both do, taking the
    // last match, so two blocks sharing a number is two blocks the lookup
    // cannot tell apart.
    #[test]
    fn a_deed_says_whether_a_run_in_flight_allows_it() {
        // Only the two that rewrite the transcript care: the run in flight is
        // writing it. The rest are the screen's own and go through whenever
        // they are asked for; the panel's three rebuild through
        // `Arc::make_mut`, so a run in flight keeps the agent it started on.
        assert!(matches!(Deed::Rewind.fate(), Fate::Refused(_)));
        assert!(matches!(
            Deed::To(agent::session::EntryId(1)).fate(),
            Fate::Refused(_)
        ));
        for deed in [
            Deed::Nothing,
            Deed::External,
            Deed::Interrupt,
            Deed::Unsend,
            Deed::SettingEdit("a.b".into(), "1".into()),
            Deed::SettingWrite("a.b".into()),
            Deed::SettingRevert("a.b".into()),
        ] {
            assert!(matches!(deed.fate(), Fate::Now), "{deed:?} should proceed");
        }
    }

    // The other half of that answer, and the one the deed cannot give itself:
    // `fate` is about a run in flight, so a lane with none admits the rewind
    // whatever the deed says. Consulting it first refused every rewind there
    // was — with the wording for a run that is not there.
    #[tokio::test]
    async fn an_idle_lane_opens_the_rewind_selector() {
        use agent::session::Session;

        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        assert!(
            matches!(tui.admit(Asked::Own(Deed::Rewind)), super::Wake::Nothing),
            "the run in flight refuses it"
        );
        assert!(tui.ui.flash.is_some(), "and says why");

        // `esc` stopped the run and its job came home: the same lane, idle.
        let mut session = Session::new();
        session.prompt("the first question");
        tui.ui.flash = None;
        let token = tui.core.lanes[0].token();
        tui.settle(super::job::Done {
            token,
            kind: super::job::Kind::Turn,
            ran: Some((session, Ok(llm::stream::Usage::default()))),
        })
        .await;

        // `esc esc` on the empty line, as the user presses it: the key has to
        // arrive at the deed before the deed can be admitted.
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let esc = || super::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let armed = tui.ui.key(
            tui.core.lane(),
            view_at(&mut tui.views, token),
            esc(),
            false,
        );
        assert!(
            matches!(&armed, Asked::Own(Deed::Nothing)),
            "the first press only arms it: {armed:?}"
        );
        let asked = tui.ui.key(
            tui.core.lane(),
            view_at(&mut tui.views, token),
            esc(),
            false,
        );
        assert!(
            matches!(&asked, Asked::Own(Deed::Rewind)),
            "the second opens the selector: {asked:?}"
        );

        let super::Wake::Do(Asked::Own(deed)) = tui.admit(asked) else {
            panic!("an idle lane refused the rewind: {:?}", tui.ui.flash);
        };
        tui.carry(deed).await;
        assert!(
            !tui.ui.rewind.is_empty(),
            "the selector opened on what the user said"
        );
        assert!(tui.ui.flash.is_none(), "and nothing was refused");
    }

    #[test]
    fn rebuilt_reasoning_blocks_get_ids_of_their_own() {
        use agent::session::Session;
        use llm::message::{AssistantContent, Reasoning, ReasoningContent};

        let mut s = Session::new();
        s.prompt("go");
        for n in 0..3 {
            s.push_assistant(vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: format!("thought {n}"),
                    signature: None,
                }],
                by: None,
            })]);
        }

        let mut folds = Folds::default();
        let rows = scrollback_from(&s, &Paint::new(false), &mut folds);
        let ids: Vec<u64> = rows.iter().filter_map(Row::block).collect();
        assert_eq!(ids.len(), 3, "{} rows, {ids:?}", rows.len());
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "two blocks share a number: {ids:?}");
        // And the counter moved, so a block streamed after the rebuild cannot
        // land on one of these.
        assert!(!ids.contains(&folds.take_id()), "{ids:?}");
    }

    #[test]
    fn rebuilt_empty_reasoning_blocks_are_ignored() {
        use agent::session::Session;
        use llm::message::{AssistantContent, Reasoning, ReasoningContent};

        let mut s = Session::new();
        s.prompt("go");
        s.push_assistant(vec![AssistantContent::Reasoning(Reasoning {
            id: None,
            content: vec![ReasoningContent::Text {
                text: "".into(),
                signature: None,
            }],
            by: None,
        })]);

        let mut folds = Folds::default();
        let rows = scrollback_from(&s, &Paint::new(false), &mut folds);
        let ids: Vec<u64> = rows.iter().filter_map(Row::block).collect();
        assert_eq!(ids.len(), 0);
    }
    // A multi-line error body reaches the pending live line and the committed
    // entry alike, so adoption's equality check compares two rows born from
    // one source — and a dropped preview would panic right here.
    #[test]
    fn an_errored_tool_adopts_its_full_body() {
        use agent::session::{Entry, EntryId};

        let mut ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();
        let body = "edit refused:\nline one\nline two";

        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolStart {
                id: "c1".into(),
                name: "edit".into(),
                args: serde_json::json!({}),
            },
        );
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolEnd {
                id: "c1".into(),
                name: "edit".into(),
                is_error: true,
                preview: body.into(),
            },
        );
        // What the loop files for the call: error content, and the preview
        // the event showed riding along (see `run_calls`'s error arm).
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::Committed {
                entries: vec![Entry::Tool {
                    id: EntryId(7),
                    at: 0,
                    result: llm::message::ToolResult::error("c1", "edit", body),
                    preview: Some(body.into()),
                }],
            },
        );

        // The pending spinner retired, the ✗ row filed once.
        assert!(view.state.tools.is_empty());
        let after = view.surface.scrollback.len();
        assert_eq!(after, 1, "one adopted row, got {after}");
    }

    // A wrapped pending row is tagged on every screen row it takes: the count
    // is fitted rows, not lines. A long command at 40 columns folds in two.
    #[test]
    fn a_wrapped_pending_batch_tags_every_row_it_takes() {
        let mut ui = test_ui(40, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();

        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolStart {
                id: "c-long".into(),
                name: "bash".into(),
                args: serde_json::json!({"command":
                    "cargo build --release --features wasi-x"}),
            },
        );
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolStart {
                id: "c-read".into(),
                name: "read".into(),
                args: serde_json::json!({}),
            },
        );
        ui.flush(&lane, &mut view);

        // Collapsed, the batch is one row; open it so every call holds a
        // row, then the long one wraps.
        ui.key(
            &lane,
            &mut view,
            mouse_event(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                0,
            ),
            false,
        );
        ui.flush(&lane, &mut view);

        let tagged = live_pending_rows(&ui);
        assert_eq!(
            tagged, 3,
            "both rows of the wrapped one, and the row after it: {tagged}"
        );

        // The tail of the wrapped batch answers — the row the count used to
        // miss — and the status line after the batch does not.
        ui.key(
            &lane,
            &mut view,
            mouse_event(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                2,
            ),
            false,
        );
        assert!(!ui.live_tools_shown, "the wrapped batch's tail answers");
        ui.flush(&lane, &mut view);
        ui.key(
            &lane,
            &mut view,
            mouse_event(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                0,
            ),
            false,
        );
        assert!(ui.live_tools_shown);
        ui.flush(&lane, &mut view);
        let last = ui.row_targets.len() - 1;
        ui.key(
            &lane,
            &mut view,
            mouse_event(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                last as u16,
            ),
            false,
        );
        assert!(ui.live_tools_shown, "the status line is no click target");
    }

    // A call that will fold is drawn by the row it will fold into from the
    // moment it starts: the row names it, spins for it and counts it, so
    // nothing moves when its result lands — the line that used to sit under
    // the row and jump up into it is gone.
    #[test]
    fn a_folding_call_is_drawn_by_the_row_it_will_join() {
        let mut ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();
        // One read lands, which opens the summary row.
        read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
        read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
        // And a second starts, still out.
        read_started(&mut ui, &mut lane, &mut view, "b", "b.rs");
        ui.flush(&lane, &mut view);

        let pending_rows = live_pending_rows(&ui);
        assert_eq!(pending_rows, 0, "the row above draws every call");
        let rows = drawn_rows(&view);
        assert_eq!(rows.len(), 1, "the two calls are one row: {rows:?}");
        assert!(
            icons::SPINNER_FRAMES.iter().any(|f| rows[0].starts_with(f)),
            "the row spins for the call it holds: {}",
            rows[0]
        );
        assert!(
            rows[0].ends_with(&format!("read b.rs {}{}", icons::ELLIPSIS, 2)),
            "named for the call in flight, counted with the one that landed: {}",
            rows[0]
        );

        // It lands: the same row, the same line, the mark now the check.
        read_landed(&mut ui, &mut lane, &mut view, 8, "b", "b.rs");
        ui.flush(&lane, &mut view);
        assert_eq!(
            drawn_rows(&view),
            vec![format!(
                "{} read b.rs {}{}",
                icons::DONE_MARK,
                icons::ELLIPSIS,
                2
            )],
            "the line it was drawn with, and the mark it was waiting for"
        );
    }

    // Two calls out at once hold a line each until one lands. The row that
    // opens then takes the other: the same hand-over as a call that starts
    // under a row already there, and the reason the row is read per frame
    // rather than settled when the call starts.
    #[test]
    fn a_batch_in_flight_hands_its_remainder_to_the_row_that_opens() {
        let mut ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();
        read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
        read_started(&mut ui, &mut lane, &mut view, "b", "b.rs");
        ui.flush(&lane, &mut view);

        // No row yet, so the batch is a line of its own: collapsed, the
        // newest with a count for the rest.
        assert_eq!(live_pending_rows(&ui), 1, "one line for two calls");
        assert!(drawn_rows(&view).is_empty(), "nothing has landed yet");

        // The first lands. Its row opens where the batch was drawn, and takes
        // the call still out with it — no second line, and none to jump up.
        read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
        ui.flush(&lane, &mut view);
        assert_eq!(
            live_pending_rows(&ui),
            0,
            "the row took it rather than leaving a line under it"
        );
        let rows = drawn_rows(&view);
        assert_eq!(rows.len(), 1, "one row, not two: {rows:?}");
        assert!(
            rows[0].ends_with(&format!("read b.rs {}{}", icons::ELLIPSIS, 2)),
            "got: {}",
            rows[0]
        );
    }

    // A call that will not fold keeps its line in the live block: an edit's
    // result is a row of its own, so its line sits where that row will land
    // rather than in the summary row a check is about to leave behind.
    #[test]
    fn a_modifying_call_keeps_its_line() {
        let mut ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();
        read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
        read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolStart {
                id: "e".into(),
                name: "edit".into(),
                args: serde_json::json!({"path": "b.rs"}),
            },
        );
        ui.flush(&lane, &mut view);

        assert_eq!(live_pending_rows(&ui), 1, "the edit holds a row of its own");
        let (live, _) = ui.live(&lane, &view, true);
        assert!(
            plain(&live[0]).contains("edit b.rs"),
            "and that row is the edit's: {:?}",
            plain(&live[0])
        );
        assert_eq!(
            drawn_rows(&view),
            vec![format!("{} read a.rs", icons::DONE_MARK)]
        );
    }

    // A call that lands badly is never the row's: the row would name it, count
    // it and wear its ✗, then drop all three when its own row landed under it
    // — the move this change exists to remove, left standing on the one path
    // where a call does not fold in.
    #[test]
    fn a_failing_call_never_joins_the_row() {
        let mut ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();
        read_started(&mut ui, &mut lane, &mut view, "a", "a.rs");
        read_landed(&mut ui, &mut lane, &mut view, 7, "a", "a.rs");
        read_started(&mut ui, &mut lane, &mut view, "b", "b.rs");
        ui.flush(&lane, &mut view);
        // Out and held: the row draws it.
        assert_eq!(live_pending_rows(&ui), 0);

        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::ToolEnd {
                id: "b".into(),
                name: "read".into(),
                is_error: true,
                preview: "Error: no such file".into(),
            },
        );
        ui.flush(&lane, &mut view);

        // The row is back to what it keeps, and the ✗ holds the line its own
        // row will take.
        assert_eq!(live_pending_rows(&ui), 1, "the ✗ keeps a line here");
        let (live, _) = ui.live(&lane, &view, true);
        assert_eq!(
            plain(&live[0]),
            format!("{} read b.rs", icons::FAIL_MARK),
            "its mark leads, with no frame left animating a call that is over"
        );
        assert_eq!(
            drawn_rows(&view),
            vec![format!("{} read a.rs", icons::DONE_MARK)],
            "and the row never counted it"
        );

        // Filing it leaves the row where it was and puts the ✗ where its line
        // was: nothing moved.
        ui.on_event(
            &mut lane,
            &mut view,
            agent::Event::Committed {
                entries: vec![failed_entry(8, "b", "read", "Error: no such file")],
            },
        );
        ui.flush(&lane, &mut view);
        let rows = drawn_rows(&view);
        assert_eq!(rows.len(), 2, "the row and the ✗ under it: {rows:?}");
        assert_eq!(rows[0], format!("{} read a.rs", icons::DONE_MARK));
        assert!(
            rows[1].contains(&format!("{} read Error: no such file", icons::FAIL_MARK)),
            "got: {}",
            rows[1]
        );
    }

    // The live rows the frame tagged as the calls in flight: what the surface
    // drew for them, as the click sees it.
    fn live_pending_rows(ui: &super::Ui) -> usize {
        ui.row_targets
            .iter()
            .filter(|t| matches!(t, Target::PendingTools))
            .count()
    }

    // A read's start, and the result the loop files under it: the two events
    // that decide what the screen draws.
    fn read_started(ui: &mut super::Ui, lane: &mut Lane, view: &mut View, call: &str, path: &str) {
        ui.on_event(
            lane,
            view,
            agent::Event::ToolStart {
                id: call.into(),
                name: "read".into(),
                args: serde_json::json!({"path": path}),
            },
        );
    }

    fn read_landed(
        ui: &mut super::Ui,
        lane: &mut Lane,
        view: &mut View,
        id: u64,
        call: &str,
        path: &str,
    ) {
        ui.on_event(
            lane,
            view,
            agent::Event::ToolEnd {
                id: call.into(),
                name: "read".into(),
                is_error: false,
                preview: path.into(),
            },
        );
        ui.on_event(
            lane,
            view,
            agent::Event::Committed {
                entries: vec![tool_entry(id, call, "read", path)],
            },
        );
    }

    fn tool_entry(id: u64, call: &str, name: &str, preview: &str) -> agent::session::Entry {
        use agent::session::{Entry, EntryId};
        Entry::Tool {
            id: EntryId(id),
            at: 0,
            result: llm::message::ToolResult::text(call, name, format!("the body of {call}")),
            preview: Some(preview.into()),
        }
    }

    fn failed_entry(id: u64, call: &str, name: &str, body: &str) -> agent::session::Entry {
        use agent::session::{Entry, EntryId};
        Entry::Tool {
            id: EntryId(id),
            at: 0,
            result: llm::message::ToolResult::error(call, name, body),
            preview: Some(body.into()),
        }
    }

    // The rows the scrollback draws, one string each.
    fn drawn_rows(view: &View) -> Vec<String> {
        ScrollbackRows::new(&view.surface.scrollback, &Paint::new(false), 80, |_| true)
            .map(text)
            .collect()
    }

    // A closed reasoning block of id `id` and `n` lines in the scrollback.
    fn block(id: u64, n: usize, folded: bool) -> Row {
        Row::reasoning(
            id,
            (1..=n).map(|i| Line::from(format!("line {i}"))).collect(),
            folded,
        )
    }

    // The text one line of the view shows, read the way a frame reads it: the
    // walk hands over what it has not built yet, so a test asks for the screen
    // rows and joins them.
    fn text(piece: super::scrollback::Piece<'_>) -> String {
        screen::Piece::pieces(piece).iter().map(plain).collect()
    }

    // The rows one lane's screen shows, the way a frame reads them.
    fn lane_rows(tui: &mut super::Tui, token: u64) -> Vec<String> {
        drawn_rows(view_at(&mut tui.views, token))
    }

    // The rows of one result are painted once per width and handed out one at
    // a time, so a stale cache would show the narrow frame's clipping in the
    // wide one — and only below the head row, where the single-row case
    // cannot see it.
    #[test]
    fn every_row_of_a_result_is_repainted_when_the_window_changes() {
        let paint = Paint::new(false);
        let long = "x".repeat(200);
        let rows = [Row::result(true, "edit", format!("head\n+12 {long}"))];

        let narrow: Vec<String> = ScrollbackRows::new(&rows, &paint, 40, |_| true)
            .map(text)
            .collect();
        let wide: Vec<String> = ScrollbackRows::new(&rows, &paint, 160, |_| true)
            .map(text)
            .collect();
        // And back again: widening must not be the only direction that repaints.
        let again: Vec<String> = ScrollbackRows::new(&rows, &paint, 40, |_| true)
            .map(text)
            .collect();

        assert_eq!(narrow.len(), 2, "head plus the one diff row");
        assert!(
            wide[1].len() > narrow[1].len(),
            "{} vs {}",
            wide[1],
            narrow[1]
        );
        assert_eq!(narrow, again, "the narrow frame came back different");
    }

    // A band the config named itself outlives the terminal's, and a terminal
    // that would not say changes nothing. Read on every config adopted.
    #[test]
    fn a_configured_band_outlives_the_terminals() {
        let bg = Some((13, 17, 23));
        let mut theme = Theme::default();
        let derived = following_terminal(&theme, bg);
        assert_ne!(derived.prompt.panel.input, theme.prompt.panel.input);
        assert_eq!(
            following_terminal(&theme, None),
            theme,
            "no answer, no lift"
        );

        theme.prompt.panel.said = Color::Rgb(1, 2, 3);
        let mixed = following_terminal(&theme, bg);
        assert_eq!(mixed.prompt.panel.said, Color::Rgb(1, 2, 3));
        assert_ne!(mixed.prompt.panel.input, theme.prompt.panel.input);
    }

    // What browse mode is built on: a row the caller does not want is passed
    // over by its count alone, and the two walks have to agree about where the
    // rows it does want are — the back walk especially, which starts inside
    // the last row there is.
    #[test]
    fn a_filtered_window_reads_the_rows_it_keeps_and_skips_the_rest() {
        let paint = Paint::new(false);
        let mut rows = vec![Row::notice("a command printed this")];
        rows.extend(Row::answer("the answer", &paint));
        rows.push(Row::notice("and then a warning"));

        let keep = |row: &Row| row.is_conversation();
        let forwards: Vec<String> = ScrollbackRows::new(&rows, &paint, 80, keep)
            .map(text)
            .collect();
        assert_eq!(forwards, ["the answer"]);

        let backwards: Vec<String> = ScrollbackRows::new(&rows, &paint, 80, keep)
            .rev()
            .map(text)
            .collect();
        assert_eq!(backwards, ["the answer"], "the back walk too");

        // Nothing kept at all leaves both walks with nothing, which is how a
        // browse of a screen holding no conversation looks.
        let none: Vec<String> = ScrollbackRows::new(&rows, &paint, 80, |_| false)
            .map(text)
            .collect();
        assert!(none.is_empty());
    }

    #[test]
    fn an_empty_scrollback_iterates_to_nothing() {
        // Both walks index `rows[0]` before comparing their pointers, so an
        // empty scrollback panicked. The back walk kept doing it after the
        // front was fixed, and `screen::window` is the one that walks back.
        let paint = Paint::new(false);
        let rows: Vec<String> = ScrollbackRows::new(&[], &paint, 80, |_| true)
            .map(text)
            .collect();
        assert!(rows.is_empty());
        let back: Vec<String> = ScrollbackRows::new(&[], &paint, 80, |_| true)
            .rev()
            .map(text)
            .collect();
        assert!(back.is_empty(), "the back walk too");
    }

    #[test]
    fn scrollback_rows_walk_from_both_ends() {
        let rows = vec![
            Row::notice("a".to_string()),
            block(1, 2, false),
            Row::notice("d".to_string()),
        ];
        let paint = Paint::new(false);
        let rows = ScrollbackRows::new(&rows, &paint, 80, |_| true);
        let (front, back): (Vec<_>, Vec<_>) = {
            let mut f = Vec::new();
            let mut b = Vec::new();
            let mut it = rows;
            loop {
                match (it.next(), it.next_back()) {
                    (Some(x), Some(y)) => {
                        f.push(x);
                        b.push(y);
                    }
                    (Some(x), None) => f.push(x),
                    (None, Some(y)) => b.push(y),
                    (None, None) => break,
                }
            }
            (f, b)
        };
        let front: Vec<String> = front.into_iter().map(text).collect();
        let back: Vec<String> = back.into_iter().map(text).collect();
        assert_eq!(front, vec!["a", "line 1"]);
        assert_eq!(back, vec!["d", "line 2"]);
    }

    #[test]
    fn toggling_moves_the_last_block_and_nothing_else() {
        // `ctrl+t` flips the block that is last now, and only it: the block
        // pushed out of last by the new one folds back to the switch.
        let mut t = Folds::default();
        let mut scrollback = vec![Row::reasoning(9, vec![Line::from("old")], false)];
        t.start(&mut scrollback);
        scrollback.push(Row::reasoning(1, vec![Line::from("new")], true));
        t.toggle_current(&mut scrollback);
        assert!(t.folded);
        assert!(scrollback[0].folded() == Some(true));
        assert!(scrollback[1].folded() == Some(false));
    }

    #[test]
    fn a_finished_block_keeps_its_fold_until_the_next_question() {
        // An unfold survives the answer — a finished block is still last —
        // and folds back to the switch the moment a new input is submitted.
        let mut t = Folds::default();
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        t.toggle_current(&mut scrollback);
        assert!(scrollback[0].folded() == Some(false));
        t.close_block();
        // Still last until the next question is asked.
        assert!(scrollback[0].folded() == Some(false));
        t.fold_previous(&mut scrollback);
        // The submitted question pushes it out of last: it folds to the
        // switch.
        assert!(scrollback[0].folded() == Some(true));
        assert!(!t.birth_fold());
    }
    #[test]
    fn a_finished_block_follows_a_global_unfold() {
        // The fold follows the switch both ways: a screen the global key
        // opened keeps its block open once the next question takes over.
        let mut t = Folds {
            folded: false,
            ..Default::default()
        };
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        t.close_block();
        t.fold_previous(&mut scrollback);
        assert!(scrollback[0].folded() == Some(false));
    }

    #[test]
    fn a_new_block_in_the_same_answer_folds_the_previous_and_inherits_the_flip() {
        // A second reasoning block in the same answer is the new last: the
        // first one folds back to the switch, and the second is born the way
        // `ctrl+t` left the last block.
        let mut t = Folds::default();
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        t.toggle_current(&mut scrollback);
        assert!(scrollback[0].folded() == Some(false));
        t.close_block();
        t.start(&mut scrollback);
        scrollback.push(block(2, 1, t.birth_fold()));
        assert!(scrollback[0].folded() == Some(true));
        assert!(scrollback[1].folded() == Some(false));
    }

    #[test]
    fn a_flip_before_the_first_line_lands_on_birth() {
        // `ctrl+t` on a block with no entry yet flips the last value, not the
        // switch: it outlives close_block, and it is not a one-shot.
        let mut t = Folds::default();
        t.start(&mut []);
        let mut scrollback: Vec<Row> = Vec::new();
        t.toggle_current(&mut scrollback);
        assert!(t.folded, "the switch itself is not touched");
        assert!(!t.birth_fold());
        t.close_block();
        assert!(!t.birth_fold());
        assert!(!t.birth_fold());
    }

    #[test]
    fn the_live_placeholder_follows_the_streaming_entry() {
        // Once the block has an entry, the live region reads its own state,
        // not the last value: a block the user unfolded streams its lines
        // even though the switch still says folded.
        let mut t = Folds::default();
        t.start(&mut []);
        assert!(t.holds(true, &[]));
        let scrollback = [block(1, 1, false)];
        assert!(!t.holds(true, &scrollback));
    }

    #[test]
    fn a_global_flip_takes_the_current_block_with_it() {
        // The case that named the key: everything else unfolded, the current
        // block folded on its own. The global key folds the whole screen —
        // the current block keeps its fold, because the fold is where the
        // rest are going.
        let mut t = Folds {
            folded: false,
            ..Default::default()
        };
        t.start(&mut []);
        let mut scrollback = vec![Row::reasoning(1, vec![Line::from("new")], true)];
        t.flip_all(&mut scrollback);
        assert!(t.folded);
        assert!(scrollback[0].folded() == Some(true));
    }

    #[test]
    fn flipping_every_block_moves_the_switch_with_them() {
        // The global key folds or unfolds every block, the current one
        // included, and moves the switch with them: rows and switch never
        // disagree, so the screen always folds back to a single state.
        let mut t = Folds::default();
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, true)];
        t.toggle_current(&mut scrollback); // unfold the current block on its own
        t.close_block();
        t.flip_all(&mut scrollback); // global fold
        assert!(!t.folded);
        assert!(scrollback.iter().all(|e| e.folded() == Some(false)));
        // The switch moved with them, so the next block is born unfolded.
        assert!(!t.birth_fold());
        // And a second global press folds the whole screen back.
        t.flip_all(&mut scrollback);
        assert!(t.folded);
        assert!(scrollback.iter().all(|e| e.folded() == Some(true)));
    }

    #[test]
    fn the_answer_is_never_folded() {
        let t = Folds::default();
        assert!(!t.holds(false, &[]));
    }

    #[test]
    fn a_flip_applies_to_each_new_last_block_until_flipped_back() {
        // `ctrl+t` controls the last thinking block, whatever it is: the
        // first one is born unfolded, and each new block that takes over as
        // last is born unfolded too, while the one it displaces folds back to
        // the switch.
        let mut t = Folds::default();

        // Startup: the key names a block that does not exist yet.
        t.toggle_current(&mut []);
        assert!(!t.birth_fold());

        // The first thinking block arrives and is the last one.
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        assert!(scrollback[0].folded() == Some(false));
        t.close_block();

        // A tool call ends the block; the next thinking block is the new
        // last, born unfolded, and the first one folds back to the switch.
        t.start(&mut scrollback);
        scrollback.push(block(2, 1, t.birth_fold()));
        assert!(scrollback[0].folded() == Some(true));
        assert!(scrollback[1].folded() == Some(false));
    }

    // One frame of the scrolled-up view: what the window shows, through the
    // same growth absorption `flush` applies.
    fn frame(
        content: &[String],
        room: usize,
        scroll: &mut usize,
        last_total: &mut Option<usize>,
    ) -> Vec<String> {
        let total = content.len();
        *scroll = absorb_growth(*scroll, *last_total, total);
        let (rows, s) = screen::window(
            content.iter().map(|s| Line::from(s.clone())),
            80,
            room,
            *scroll,
        );
        *scroll = s;
        *last_total = Some(total);
        rows.into_iter().map(|l| plain(&l)).collect()
    }

    #[test]
    fn a_scrolled_up_view_holds_until_the_user_scrolls_back() {
        // Scrolled two rows up from ten rows of history, rows 5-8 stay put
        // as output arrives below; only the user's own scroll moves them.
        let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let (room, mut scroll, mut last_total) = (4usize, 0usize, None);
        scroll = scroll.saturating_add(2);
        let first = frame(&content, room, &mut scroll, &mut last_total);
        assert_eq!(first, vec!["5", "6", "7", "8"]);
        for n in 11..=15 {
            content.push(n.to_string());
            assert_eq!(
                frame(&content, room, &mut scroll, &mut last_total),
                first,
                "row {n} arriving moved the scrolled-up window"
            );
        }
        scroll = scroll.saturating_sub(1);
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            vec!["6", "7", "8", "9"]
        );
    }

    #[test]
    fn rows_gone_below_the_window_leave_the_view_put() {
        // Rows removed below the window shrink the tail; the negative delta
        // is absorbed like a positive one and the window stays put.
        let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let (room, mut scroll, mut last_total) = (4usize, 0usize, None);
        scroll = scroll.saturating_add(2);
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            vec!["5", "6", "7", "8"]
        );
        for n in 11..=15 {
            content.push(n.to_string());
            frame(&content, room, &mut scroll, &mut last_total);
        }
        content.truncate(10);
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            vec!["5", "6", "7", "8"]
        );
    }

    // A row that wraps counts for the rows it takes, not the line it is:
    // both of its rows fold into the scroll, or the window drifts.
    #[test]
    fn a_wrapped_row_landing_below_moves_the_scroll_by_its_rows() {
        let mut ui = test_ui(20, 12);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        view.surface.scrollback = (1..=12).map(|n| Row::notice(format!("row {n}"))).collect();
        ui.flush(&lane, &mut view);

        // Scroll up, and base the measurement on this frame's layout.
        view.surface.scroll = 2;
        view.surface.counted = None;
        ui.flush(&lane, &mut view);
        let rebased = view.surface.scroll;
        assert_eq!(rebased, 2);

        // 25 columns at a 19-column width: one line, two rows.
        view.surface.scrollback.push(Row::notice("x".repeat(25)));
        ui.flush(&lane, &mut view);
        assert_eq!(
            view.surface.scroll,
            rebased + 2,
            "both wrapped rows folded into the scroll"
        );
    }

    // ---------------------------------------------------------- settling

    // A lane wired up enough to be settled: a real transcript, a real agent,
    // and a `Run::Running` standing in for the job that is about to report.
    // A lane and the directory it lives in — the guard comes back so the
    // caller keeps it alive for as long as the lane is used.
    fn a_running_lane() -> (tempfile::TempDir, Lane) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lane = running_lane(dir.path());
        (dir, lane)
    }

    // An idle worktree lane whose checkout has been deleted from disk —
    // exactly what `drop_vanished_lanes` exists to find.
    fn vanished_lane(name: &str) -> Lane {
        let (_dir, mut lane) = a_running_lane();
        lane.run = Run::Idle;
        lane.worktree = Some(name.into());
        std::fs::remove_dir_all(lane.root()).expect("the checkout goes");
        lane
    }

    // A mouse event on the given screen row, at a column over any row's text.
    fn mouse_event(kind: crossterm::event::MouseEventKind, row: u16) -> crossterm::event::Event {
        crossterm::event::Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column: 5,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }
    fn running_lane(dir: &std::path::Path) -> Lane {
        struct Mute;
        #[async_trait::async_trait]
        impl llm::Transport for Mute {
            async fn stream(
                &self,
                _spec: &llm::model::ModelSpec,
                _req: &llm::request::Request,
            ) -> llm::Result<
                futures::stream::BoxStream<'static, llm::Result<llm::stream::StreamEvent>>,
            > {
                Ok(Box::pin(futures::stream::empty()))
            }
        }
        let ws = tools::Workspace::new(dir).expect("a workspace");
        let spec = llm::model::ModelSpec {
            model: "m".into(),
            base_url: "http://localhost".into(),
            format: llm::model::Format::Anthropic {
                cache_control: llm::model::CacheControl::Off,
            },
            context_window: 200_000,
            max_output_tokens: 8_000,
            vision: false,
            thinking: None,
            accepts_temperature: true,
            can_force_tool: true,
            replay_thinking: llm::model::ReplayThinking::Tagged,
            pricing: llm::model::Pricing::default(),
        };
        let (events, inbox) = Lane::channel();
        Lane {
            agent: std::sync::Arc::new(agent::Agent::new(std::sync::Arc::new(Mute), spec)),
            token: crate::app::lane::next_token(),
            session: None,
            id: "s1".into(),
            created: 0,
            name: None,
            totals: agent::Totals::default(),
            tally: Default::default(),
            held_screens: Vec::new(),
            context: Vec::new(),
            standing: std::sync::Arc::from(""),
            ctx: tools::Ctx::new(ws),
            worktree: None,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            // What every `start_*` leaves behind while its job runs.
            run: Run::Running {
                cancel: tokio_util::sync::CancellationToken::new(),
                steer: None,
                unsend: false,
            },
            keys: std::sync::Arc::new(Keys::default()),
            commands: std::sync::Arc::new(Vec::new()),
        }
    }

    fn surface(dir: &std::path::Path) -> super::Tui {
        let keys = std::sync::Arc::new(Keys::default());
        let core = App {
            store: Store::new(dir.join("state")),
            keys: keys.clone(),
            config: std::sync::Arc::new(crate::store::config::Config::default()),
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands: std::sync::Arc::new(Vec::new()),
            settings: crate::store::settings::Settings::new(toml::Value::Table(Default::default())),
            lanes: vec![running_lane(dir)],
            current: 0,
        };
        super::Tui::on_test_screen(core, keys)
    }

    // Put another lane in front, the way a checkout switch would, and
    // reconcile the surface against the lane it displaced.
    fn switch_to(tui: &mut super::Tui, lane: Lane) {
        let was = tui.core.current;
        tui.core.lanes.push(lane);
        tui.core.current = tui.core.lanes.len() - 1;
        tui.reconcile(was);
    }

    // A command that answered with nothing opens nothing: `/new`, `/worktree`
    // to a checkout already open and the `/loop` that only arms a lane all come
    // through here empty, and an overlay the user has to dismiss for nothing is
    // worse than silence.
    #[tokio::test]
    async fn an_empty_reply_is_not_opened() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        tui.ui.open_reply(Listing::default());
        assert!(tui.ui.reply.is_none());
    }

    // A reply is closed the way the menu's other lists are, and the esc goes
    // to it rather than past it: an esc that reached the run would stop a turn
    // the user meant to leave alone.
    #[tokio::test]
    async fn esc_closes_the_reply_rather_than_reaching_the_run() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        tui.ui.open_reply(Listing::say(["/help answered this"]));

        let token = tui.core.lane().token();
        let esc = super::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let asked = tui
            .ui
            .key(tui.core.lane(), view_at(&mut tui.views, token), esc, false);
        assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
        assert!(tui.ui.reply.is_none(), "the esc closed it");
    }

    // A command is not echoed onto the screen: its answer is the reply over the
    // menu, which is dismissed rather than kept, so the line would be left
    // above an answer that never comes. A line that opens a turn still is —
    // the turn streams its rows under it, and a line nobody can see they sent
    // is one they send twice.
    #[tokio::test]
    async fn a_command_is_not_echoed_and_a_prompt_is() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        // The real table, or `/keys` is a word the door does not know and the
        // line is read as a prompt — which is a different test.
        let table = std::sync::Arc::new(crate::input::commands::commands(&[], &mut Vec::new()));
        tui.core.commands = table.clone();
        tui.ui.commands = table;
        let token = tui.core.lane().token();
        let rows = |tui: &mut super::Tui| view_at(&mut tui.views, token).surface.scrollback.len();
        let enter = || super::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let before = rows(&mut tui);
        tui.ui.editor.set_line("/keys");
        let lane = tui.core.lane_mut();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), enter(), false);
        assert_eq!(rows(&mut tui), before, "a command leaves no row");

        let before = rows(&mut tui);
        tui.ui.editor.set_line("what changed?");
        let lane = tui.core.lane_mut();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), enter(), false);
        assert_eq!(
            rows(&mut tui),
            before + 1,
            "a prompt is one, answered under"
        );
    }

    // A reply is an answer over the menu, not a mode over the keyboard: it
    // reads the menu's own keys and nothing else, so a letter typed over it
    // takes it down on the way past and lands in the editor. A letter it read
    // for itself would be a letter missing from the line being typed.
    #[tokio::test]
    async fn a_reply_reads_the_menus_keys_and_yields_everything_else() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        tui.ui.open_reply(Listing::say(["/status answered this"]));
        let token = tui.core.lane().token();

        // The menu's own: the window moves and the reply stays up.
        let down = super::TermEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let lane = tui.core.lane_mut();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), down, false);
        assert!(tui.ui.reply.is_some(), "a menu key is the reply's");

        // Not the menu's: a letter meant for the next line comes down here, and
        // is left for the caller to read as what it was typed as.
        let j = super::TermEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let lane = tui.core.lane_mut();
        let asked = tui.ui.key(lane, view_at(&mut tui.views, token), j, false);
        assert!(tui.ui.reply.is_none(), "the letter took it down");
        assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
    }

    // Enter is not the reply's to swallow: the line is sent, and the answer the
    // reply was showing belongs to the line before it. The same for `ctrl+c`,
    // which is the stop-the-run key whatever is drawn over the editor — a
    // refusal opens a reply while a turn is in flight, and a run that cannot be
    // stopped because a refusal is on screen is worse than the refusal.
    #[tokio::test]
    async fn the_line_and_the_stop_are_not_the_replys_to_take() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        tui.ui.editor.set_line("/help");
        tui.ui.open_reply(Listing::say(["/status answered this"]));

        let token = tui.core.lane().token();
        let enter = super::TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let lane = tui.core.lane_mut();
        tui.ui.key(
            lane,
            view_at(&mut tui.views, token),
            enter,
            /* running */ false,
        );
        assert!(tui.ui.reply.is_none(), "the line took it down");
        assert!(tui.ui.took_submit(), "and the line it was written for went");
        assert!(tui.ui.editor.is_empty(), "taken off the line");

        tui.ui.open_reply(Listing::say(["/status answered this"]));
        let ctrl_c =
            super::TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let lane = tui.core.lane_mut();
        tui.ui
            .key(lane, view_at(&mut tui.views, token), ctrl_c, true);
        assert!(tui.ui.reply.is_none(), "`ctrl+c` took it down too");
    }

    // A row filed for the screen is drawn as it is filed, so the cursor that
    // says what this surface has drawn has to move past the entry. Left where
    // it was, the next turn's adopt draws the row a second time, above the
    // prompt it is answering. And while a run holds the transcript there is no
    // entry to move past: the row waits with the lane instead.
    #[tokio::test]
    async fn a_filed_row_moves_the_cursor_an_adopt_goes_by() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let token = tui.core.lane().token();
        let lane = tui.core.lane_mut();
        let view = view_at(&mut tui.views, token);
        let drawn = view.surface.scrollback.len();

        // The surface lane is running: the transcript is out with the run.
        tui.ui
            .file_screen(lane, view, "! the reply came back short");
        assert_eq!(view.surface.scrollback.len(), drawn + 1, "drawn now");
        assert_eq!(view.surface.tail, None, "with nothing filed to move past");
        assert_eq!(lane.held_screens.len(), 1, "it is waiting with the lane");

        // The run comes home, and the next row can be filed as it is drawn.
        lane.return_session(agent::session::Session::default());
        tui.ui.file_screen(lane, view, "! settled");
        assert_eq!(
            view.surface.tail,
            lane.session()
                .and_then(|s| s.entries().last())
                .map(|e| e.id()),
            "the cursor is past the row just drawn"
        );
    }

    // A browsing panel swallows the keys it does not know: a letter typed
    // over it neither moves a row nor reaches the input line underneath.
    #[tokio::test]
    async fn browsing_panel_does_not_leak_keys_to_the_editor() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let rows = vec![row("model", "flash", false)];
        tui.ui.panel = Some(Panel::new(rows, &crate::store::config::Vim::default()));
        let token = tui.core.lane().token();
        let lane = tui.core.lane_mut();
        let intent = tui
            .ui
            .key(lane, view_at(&mut tui.views, token), typed('z'), false);
        assert!(matches!(intent, Asked::Own(Deed::Nothing)));
        assert!(
            tui.ui.editor.is_empty(),
            "the editor did not take the keystroke"
        );
    }

    // The panel can open onto a browse nobody left — a `/settings` queued
    // during a run, or sent from the phone — and it is drawn over everything:
    // a mode that swallowed its keys would leave a screen nobody can drive.
    #[tokio::test]
    async fn a_panel_opened_onto_browse_still_takes_the_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let rows = vec![row("model", "flash", false)];
        tui.ui.panel = Some(Panel::new(rows, &crate::store::config::Vim::default()));
        tui.ui.browsing = true;
        let token = tui.core.lane().token();
        let lane = tui.core.lane_mut();
        let esc = super::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        let asked = tui.ui.key(lane, view_at(&mut tui.views, token), esc, false);
        assert!(matches!(asked, Asked::Own(Deed::Nothing)));
        assert!(tui.ui.panel.is_none(), "the panel took the esc and closed");
        assert!(tui.ui.browsing, "and the browse behind it is still up");
    }

    // Recall belongs to the checkout, like the transcripts and the completion
    // lists. One file for the whole machine put the lines typed in one project
    // under `k` in another — a leak as much as a nuisance.
    #[tokio::test]
    async fn recall_follows_the_checkout() {
        let first = tempfile::tempdir().expect("a checkout");
        let second = tempfile::tempdir().expect("another checkout");
        let mut tui = surface(first.path());

        let store = tui.core.store.clone();
        let file = |ws: &std::path::Path, line: &str| {
            let path = store.history_path(ws);
            std::fs::create_dir_all(path.parent().expect("a bucket")).expect("the bucket");
            std::fs::write(path, super::editor::encode(&[line.to_string()])).expect("written");
        };
        file(first.path(), "what the first was asked");
        file(second.path(), "what the second was asked");

        // `on_test_screen` skips the startup seed `Tui::new` does, so stand in
        // for it. What is under test is that a switch replaces this, and that
        // it does not reach for the bucket of the checkout being left.
        tui.ui
            .editor
            .seed_history(vec!["what the first was asked".to_string()]);
        switch_to(&mut tui, running_lane(second.path()));

        let landed = tui.ui.editor.history();
        assert_eq!(
            landed.first().map(String::as_str),
            Some("what the second was asked"),
            "the lane in front is what k recalls"
        );
        assert_eq!(landed.len(), 1, "replaced, not appended: {landed:?}");
    }

    // A flash belongs to the lane it answered. Carried across a switch it
    // names the wrong checkout, and it does it on the row the bar uses to say
    // which checkout is in front.
    #[tokio::test]
    async fn a_flash_does_not_follow_the_surface_to_another_lane() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());

        tui.ui.flash("nothing running to stop");
        switch_to(&mut tui, running_lane(dir.path()));
        assert!(tui.ui.flash.is_none(), "the flash was left behind");
    }

    // A rebuilt lane has already been drawn, whatever its row counts say.
    // `rebuild` clears the banner along with the rest — `/resume` and a
    // rewind both do it — so a switch back must not read that as a lane
    // never drawn and lay a fresh opening block over the transcript.
    #[tokio::test]
    async fn switching_back_to_a_rebuilt_lane_keeps_its_transcript() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        let lane = running_lane(dir.path());
        let mut view = super::View::opening(&[], &tui.ui.paint);
        // As `rebuild` leaves it: the conversation, and no banner.
        view.surface.scrollback = vec![Row::notice("what was said before")];
        view.surface.opened = 0;
        // The screen the lane comes back with, keyed by the lane it was built
        // for: a lane no longer carries its own.
        tui.views.insert(lane.token(), view);
        switch_to(&mut tui, lane);

        // By content, not by count: the banner this would lay over it is one
        // row too, so a length check cannot tell them apart.
        let token = tui.core.lanes[1].token();
        let rows = lane_rows(&mut tui, token);
        assert!(
            rows.iter().any(|r| r.contains("what was said before")),
            "the transcript survives the switch: {rows:?}"
        );
    }

    // A lane out of front keeps what its run posts: the transcript holds it
    // either way, but the view is only fed while the screen is on that lane.
    #[tokio::test]
    async fn what_a_lane_out_of_front_posted_waits_for_it() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        let lane = running_lane(dir.path());
        let token = lane.token();
        let sent = lane.sender().clone();
        tui.core.lanes.push(lane);

        sent.send(Event::Warning("said behind you".into()))
            .expect("the lane is listening");
        tui.serve_lanes().await;
        assert!(
            !tui.core.lanes[1].pending.is_empty(),
            "a lane out of front threw away what its run posted"
        );

        // The screen moves to it, the way a checkout switch does.
        tui.core.current = 1;
        tui.reconcile(0);
        let rows = lane_rows(&mut tui, token);
        assert!(
            rows.iter().any(|r| r.contains("said behind you")),
            "what the lane posted never reached its screen: {rows:?}"
        );
    }

    // The bug this guards: `/compact` used to settle without ever putting the
    // lane back to `Idle`, and a lane left `Running` queues every later
    // prompt into a queue that only drains once it is not running — so the
    // checkout was wedged for good. Every kind has to come back idle.
    #[tokio::test]
    async fn every_kind_of_job_leaves_its_lane_idle() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let kinds = || {
            vec![
                ("turn", super::job::Kind::Turn),
                (
                    "bash",
                    super::job::Kind::Bash {
                        lines: vec!["out".into()],
                    },
                ),
                ("compact", super::job::Kind::Compact(None)),
                (
                    "compact with a report",
                    super::job::Kind::Compact(Some((
                        agent::Report::default(),
                        llm::stream::Usage::default(),
                    ))),
                ),
            ]
        };
        for (what, kind) in kinds() {
            let mut tui = surface(dir.path());
            tui.settle(super::job::Done {
                token: tui.core.lanes[0].token(),
                kind,
                ran: Some((
                    agent::session::Session::default(),
                    Ok(llm::stream::Usage::default()),
                )),
            })
            .await;
            assert!(
                !tui.core.lanes[0].is_running(),
                "a {what} left its lane running"
            );
        }
        // And the same when the job panicked and brought no transcript home.
        for (what, kind) in kinds() {
            let mut tui = surface(dir.path());
            tui.settle(super::job::Done {
                token: tui.core.lanes[0].token(),
                kind,
                ran: None,
            })
            .await;
            assert!(
                !tui.core.lanes[0].is_running(),
                "a panicked {what} left its lane running"
            );
        }
    }

    // A `!` that panicked brings no cursor home. Reading that as "nothing is
    // drawn yet" laid the recovered transcript over the screen a second time.
    #[tokio::test]
    async fn a_panicked_bang_does_not_lay_its_transcript_down_again() {
        use agent::session::{Prompt, Session};
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let mut s = Session::new();
        s.push_bash(Prompt {
            text: "Ran `ls`\nfile".into(),
            image: None,
            shown: Some("!ls".into()),
        });
        tui.core.lane_mut().session = Some(s);
        let token = tui.core.lane().token();
        // The rows as the screen has them, and the archive as it was saved.
        {
            let session = tui.core.lane().session().expect("the transcript");
            tui.ui.rebuild(view_at(&mut tui.views, token), session);
        }
        tui.core.save_lane(0).expect("saved");

        // The job panicked: no lines, and no transcript came home.
        tui.settle(super::job::Done {
            token,
            kind: super::job::Kind::Bash { lines: Vec::new() },
            ran: None,
        })
        .await;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tui.start_turn("the next question".into(), None, &tx);

        let rows = lane_rows(&mut tui, token);
        assert_eq!(
            rows.iter().filter(|r| *r == "! ls").count(),
            1,
            "the recovered transcript is not laid down twice: {rows:?}"
        );
    }

    // A stopped command or turn does not inject a cancelled note to the model.
    #[tokio::test]
    async fn a_stopped_run_does_not_tell_the_model_a_request_was_cancelled() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let stopped = |kind: super::job::Kind| {
            let dir = dir.path().to_path_buf();
            async move {
                let mut tui = surface(&dir);
                let mut session = agent::session::Session::new();
                session.prompt("the task the user actually asked for");
                tui.settle(super::job::Done {
                    token: tui.core.lanes[0].token(),
                    kind,
                    ran: Some((session, Err(agent::AgentError::Cancelled))),
                })
                .await;
                let mut back = tui.core.lanes[0]
                    .session
                    .take()
                    .expect("the transcript back");
                back.send_prompt(String::from("now something else"), None::<String>);
                format!("{:?}", back.entries())
            }
        };

        let after_bash = stopped(super::job::Kind::Bash {
            lines: vec!["some output".into()],
        })
        .await;
        assert!(
            !after_bash.contains("stopped the previous run"),
            "a stopped `!` adds no note: {after_bash}"
        );

        let after_turn = stopped(super::job::Kind::Turn).await;
        assert!(
            !after_turn.contains("stopped the previous run"),
            "a stopped turn adds no note: {after_turn}"
        );
    }

    #[test]
    fn a_stopped_tool_call_is_silenced_in_tui() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let tui = surface(dir.path());
        let mut session = agent::session::Session::new();
        session.prompt("run check");
        session.push_assistant(vec![llm::message::AssistantContent::ToolCall(
            llm::message::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                args: serde_json::json!({ "command": "cargo check" }),
            },
        )]);
        session.send_prompt("do something else", None::<String>);

        // The rebuild filter drops the repaired stopped tool entry.
        let stopped_entry = &session.entries()[2];
        assert!(super::scrollback::f_entry(stopped_entry, &tui.ui.paint).is_none());
    }

    // An interrupted turn never states its own word, so the spend the view
    // was already showing is what lands in the totals — the next run's base
    // carries it rather than stepping back to what the session had before.
    #[tokio::test]
    async fn an_interrupted_turn_keeps_its_spend_in_the_session_totals() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        let mut session = agent::session::Session::new();
        session.prompt("the task the user actually asked for");

        tui.core.lanes[0].note(&agent::Event::TurnStart { turn: 1 });
        tui.core.lanes[0].note(&agent::Event::Usage(llm::stream::Usage {
            input: 100,
            output: 20,
            ..Default::default()
        }));

        tui.settle(super::job::Done {
            token: tui.core.lanes[0].token(),
            kind: super::job::Kind::Turn,
            ran: Some((session, Err(agent::AgentError::Cancelled))),
        })
        .await;

        assert_eq!(tui.core.lanes[0].totals.usage.input, 100);
        assert_eq!(tui.core.lanes[0].totals.usage.output, 20);
    }

    // A flash is transient: it is not part of the transcript, and it is
    // dropped by the clock, not by whoever set it — who is long gone by then.
    #[test]
    fn a_flash_is_transient_and_stays_out_of_the_transcript() {
        let mut ui = test_ui(40, 8);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        let before = view.surface.scrollback.len();

        ui.flash("the only checkout there is");
        ui.flush(&lane, &mut view);
        assert!(ui.flash.is_some());
        assert_eq!(
            view.surface.scrollback.len(),
            before,
            "a flash is not part of the transcript"
        );

        // Backdated past the window: the next frame is the one that drops it,
        // which is what an idle screen relies on.
        let (text, _) = ui.flash.take().expect("a flash is up");
        ui.flash = Some((
            text,
            super::Instant::now()
                .checked_sub(super::FLASH)
                .expect("a clock"),
        ));
        ui.flush(&lane, &mut view);
        assert!(ui.flash.is_none(), "the expired flash was dropped");
    }

    // The bar's row is the bar's whether or not it is saying anything: a row
    // that came and went would take the transcript above it along, and the
    // newest line would sit a row lower for the second a flash is up.
    #[test]
    fn the_bar_keeps_its_row_when_a_flash_comes_and_goes() {
        let mut ui = test_ui(40, 8);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.tabs = vec![tab(super::Mark::Front, "main")];

        ui.flush(&lane, &mut view);
        let quiet = ui.regions.history.height;
        assert_eq!(ui.regions.bar.height, 1, "the bar is a row of its own");

        ui.flash("the only checkout there is");
        ui.flush(&lane, &mut view);
        assert_eq!(
            ui.regions.history.height, quiet,
            "a flash moves nothing above it"
        );
    }

    // One lane is on the bar like any other. The row is spent whether or not
    // it is saying something, and its name says which checkout is in front.
    #[test]
    fn one_lane_names_itself_on_the_bar() {
        let mut ui = test_ui(40, 8);
        ui.tabs = vec![tab(super::Mark::Front, "main")];
        assert_eq!(bar(&ui, 39), "main");
    }

    // A strip wider than its row keeps the front lane — the one lane the row is
    // there to name — and says with an `…` which ends it dropped.
    #[test]
    fn a_strip_too_wide_keeps_the_front_and_marks_the_ends() {
        let mut ui = test_ui(20, 8);
        ui.tabs = vec![
            tab(super::Mark::Plain, "alpha"),
            tab(super::Mark::Done, "beta"),
            tab(super::Mark::Front, "gamma"),
            tab(super::Mark::Failed, "delta"),
            tab(super::Mark::Plain, "epsilon"),
        ];
        assert_eq!(bar(&ui, 20), "… · beta · gamma · …");
        assert_eq!(bar(&ui, 30), "… · beta · gamma · delta · …");

        // In front is in front: the first lane is never dropped, and a strip
        // that runs out on one side alone says so on that side alone.
        ui.tabs.rotate_left(2);
        assert_eq!(bar(&ui, 20), "gamma · delta · …");
    }

    // A lane wearing a mark is saying something nobody has read yet: the bar
    // drops the quiet entries first, so a coloured name does not scroll off
    // unseen.
    #[test]
    fn a_marked_lane_outlives_a_quiet_one() {
        let mut ui = test_ui(24, 8);
        ui.tabs = vec![
            tab(super::Mark::Plain, "quietly-idle-here"),
            tab(super::Mark::Front, "gamma"),
            tab(super::Mark::Failed, "delta"),
        ];
        // Only an entry dropped will fit: the idle one goes, not the settled
        // one the colour marks.
        assert_eq!(bar(&ui, 24), "… · gamma · delta");
    }

    // The bar carries the whole ring, the lanes' own and the disk's alike: a
    // checkout no lane has open is on it as soon as the list has been read, so
    // what `H`/`L` would reach is read off the bar before either is pressed.
    #[test]
    fn the_bar_lists_the_checkouts_no_lane_has_open() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        tui.core.lanes[0].worktree = Some("pi-rs".into());
        tui.ui
            .lists
            .worktrees
            .set(vec![
                Choice {
                    name: "pi-rs".into(),
                    note: String::new(),
                },
                Choice {
                    name: "fix-mem".into(),
                    note: String::new(),
                },
                Choice {
                    name: "fw-rm".into(),
                    note: String::new(),
                },
            ])
            .ok();
        tui.refresh_tabs();

        assert_eq!(bar(&tui.ui, 39), "pi-rs · ○ fix-mem · ○ fw-rm");
        // And the order it lists them in is the order a step walks.
        assert_eq!(
            tui.ui.step_checkout(&tui.core.lanes[0], true).as_deref(),
            Some("fix-mem")
        );
    }

    // The same notice landing again with nothing between it and the last one
    // is one row and a count — a screenful of identical lines is the failure
    // this stops, and only an unbroken run folds.
    #[test]
    fn the_same_notice_twice_running_is_one_row_and_a_count() {
        let mut ui = test_ui(40, 8);
        let (_dir, _lane) = a_running_lane();
        let mut view = View::default();
        view.surface.scrollback.clear();

        ui.say(&mut view, "nothing to rewind to");
        ui.say(&mut view, "nothing to rewind to");
        ui.say(&mut view, "nothing to rewind to");
        assert_eq!(
            view.surface.scrollback.len(),
            1,
            "three identical notices file as one row"
        );

        // Broken by another line, the next repeat starts its own row rather
        // than reaching back over it.
        ui.say(&mut view, "stopped");
        ui.say(&mut view, "nothing to rewind to");
        assert_eq!(
            view.surface.scrollback.len(),
            3,
            "the repeat after the break starts its own row"
        );
    }
    // The Normal `L` walks the checkouts in a ring forward; `H` walks it
    // back. Every checkout on disk is in it, not only the open ones
    // — the main one first, because that is the order `app::worktree::list`
    // reports and a lane in it carries no name.
    #[test]
    fn stepping_the_checkouts_walks_the_ring_and_wraps_both_ways() {
        let ring = |at: Option<&str>, forward: bool| {
            let ui = test_ui(80, 24);
            let trees = ["pi-rs", "f1", "f2"]
                .iter()
                .map(|n| Choice {
                    name: n.to_string(),
                    note: String::new(),
                })
                .collect();
            ui.lists.worktrees.set(trees).ok();
            let (_dir, mut lane) = a_running_lane();
            lane.worktree = at.map(str::to_string);
            ui.step_checkout(&lane, forward)
        };
        // The main checkout is the one a lane names as None.
        assert_eq!(ring(None, true).as_deref(), Some("f1"));
        assert_eq!(ring(Some("f1"), true).as_deref(), Some("f2"));
        // And round the end, back to the main one.
        assert_eq!(ring(Some("f2"), true).as_deref(), Some("pi-rs"));
        // The other way round, from the main one.
        assert_eq!(ring(None, false).as_deref(), Some("f2"));
        assert_eq!(ring(Some("f1"), false).as_deref(), Some("pi-rs"));
        assert_eq!(ring(Some("f2"), false).as_deref(), Some("f1"));
    }

    // The ring agrees with the tabs the bar shows: a step lands on the next
    // checkout in the order they were opened, not the order git reports
    // them, with the ones not open yet after the open ones.
    #[test]
    fn the_ring_walks_the_tabs_order_not_gits() {
        let mut ui = test_ui(80, 24);
        let trees = ["pi-rs", "fw-rm", "fix-input", "fix-mem"]
            .iter()
            .map(|n| Choice {
                name: n.to_string(),
                note: String::new(),
            })
            .collect();
        ui.lists.worktrees.set(trees).ok();
        // fix-input was created before fix-mem, but the user opened fix-mem
        // first — the bar's order, which a step from fw-rm must follow.
        ui.tabs = vec![
            super::Tab {
                mark: super::Mark::Plain,
                name: "pi-rs".into(),
            },
            super::Tab {
                mark: super::Mark::Plain,
                name: "fw-rm".into(),
            },
            super::Tab {
                mark: super::Mark::Front,
                name: "fix-mem".into(),
            },
            super::Tab {
                mark: super::Mark::Plain,
                name: "fix-input".into(),
            },
        ];
        let (_dir, mut lane) = a_running_lane();
        lane.worktree = Some("fw-rm".into());

        assert_eq!(ui.step_checkout(&lane, true).as_deref(), Some("fix-mem"));
        lane.worktree = Some("fix-mem".into());
        assert_eq!(ui.step_checkout(&lane, true).as_deref(), Some("fix-input"));
        lane.worktree = Some("fix-input".into());
        assert_eq!(ui.step_checkout(&lane, true).as_deref(), Some("pi-rs"));
        // And the other way, still on the bar's order.
        lane.worktree = Some("fix-mem".into());
        assert_eq!(ui.step_checkout(&lane, false).as_deref(), Some("fw-rm"));
    }

    // A checkout deleted from the shell leaves its lane a dead end; the loop
    // drops idle ones so the bar's tab and the step ring stop pretending it
    // is there, and the lane in front keeps its place.
    #[test]
    fn a_lane_whose_checkout_vanished_is_dropped_and_current_follows() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        // A stale ring entry would let a later step offer — and re-create —
        // the checkout that just went; dropping the lane drops the cache.
        tui.ui
            .lists
            .worktrees
            .set(vec![Choice {
                name: "fix-mem".into(),
                note: String::new(),
            }])
            .ok();
        let gone = vanished_lane("fix-mem");
        tui.core.lanes.push(gone);
        assert_eq!(tui.core.lanes.len(), 2);

        tui.drop_vanished_lanes();
        assert_eq!(tui.core.lanes.len(), 1);
        assert_eq!(tui.core.current, 0);
        assert!(
            tui.ui.flash.is_none(),
            "the lane leaving the bar is the whole notice"
        );
        assert!(
            tui.ui.lists.worktrees().is_empty(),
            "the stale ring entry went too"
        );

        // A vanished lane before the one in front shifts its index down.
        let mut tui = surface(dir.path());
        let earlier = vanished_lane("fix-old");
        tui.core.lanes.insert(0, earlier);
        tui.core.current = 1;
        tui.core.lanes[1].run = Run::Idle;
        tui.drop_vanished_lanes();
        assert_eq!(tui.core.lanes.len(), 1);
        assert_eq!(tui.core.current, 0, "the front lane follows its index");
    }

    // A vanished lane that sits before a running one waits: removing it
    // would shift the index the running lane's end reports back by.
    #[test]
    fn a_vanished_lane_before_a_running_one_waits_for_it() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let gone = vanished_lane("fix-mem");
        tui.core.lanes.push(gone);
        let (_run_dir, running) = a_running_lane();
        tui.core.lanes.push(running);
        assert_eq!(tui.core.lanes.len(), 3);

        tui.drop_vanished_lanes();
        assert_eq!(tui.core.lanes.len(), 3, "the run's lane must not move");

        // The run over, the same pass now reaches the vanished lane.
        tui.core.lanes[2].run = Run::Idle;
        tui.drop_vanished_lanes();
        assert_eq!(tui.core.lanes.len(), 2);
    }

    // The bar answers what a lane has finished, not what it is doing: a lane
    // working out of sight wears its plain name and no frame, and only a run
    // that ended wears a mark.
    #[test]
    fn only_a_finished_lane_wears_a_mark_in_the_bar() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let (_run_dir, mut behind) = a_running_lane();
        behind.worktree = Some("fix-mem".into());
        tui.core.lanes.push(behind);
        tui.refresh_tabs();

        let marks = |tui: &super::Tui| tui.ui.tabs.iter().map(|t| t.mark).collect::<Vec<_>>();
        assert_eq!(
            marks(&tui),
            vec![super::Mark::Front, super::Mark::Plain],
            "a lane working out of sight is just a lane"
        );
        let bar = plain(&tui.ui.lane_bar("", 80).expect("two lanes keep a bar"));
        assert!(
            !icons::SPINNER_FRAMES.iter().any(|f| bar.contains(f)),
            "and the bar does not animate it: {bar}"
        );

        tui.core.lanes[1].run = Run::Ended {
            out: Ok(llm::stream::Usage::default()),
            unsend: false,
        };
        tui.refresh_tabs();
        assert_eq!(marks(&tui), vec![super::Mark::Front, super::Mark::Done]);

        tui.core.lanes[1].run = Run::Ended {
            out: Err(agent::AgentError::Cancelled),
            unsend: false,
        };
        tui.refresh_tabs();
        assert_eq!(marks(&tui), vec![super::Mark::Front, super::Mark::Failed]);
    }

    // The loop going on is not the lane finishing: the round queued behind the
    // one that ended is work this lane still has, so `Done` is not its mark.
    #[test]
    fn a_lane_with_a_round_waiting_has_not_finished() {
        let dir = tempfile::tempdir().expect("a checkout");
        let mut tui = surface(dir.path());
        let (_run_dir, mut behind) = a_running_lane();
        behind.worktree = Some("fix-mem".into());
        behind.loop_start("go".into());
        behind.run = Run::Ended {
            out: Ok(llm::stream::Usage::default()),
            unsend: false,
        };
        let token = behind.token();
        tui.core.lanes.push(behind);
        view_at(&mut tui.views, token).queued.push(Queued::Round {
            goal: "go".into(),
            note: String::new(),
        });
        tui.refresh_tabs();
        assert_eq!(
            tui.ui.tabs[1].mark,
            super::Mark::Plain,
            "the round in the queue is the loop going on"
        );

        // The round that ended was the loop's last: nothing waits behind it,
        // and that is the lane the colour is for.
        view_at(&mut tui.views, token).queued.clear();
        tui.refresh_tabs();
        assert_eq!(tui.ui.tabs[1].mark, super::Mark::Done);
    }

    // Nowhere to go is said, not walked to: one checkout has no next.
    #[test]
    fn a_lone_checkout_has_no_next() {
        let ui = test_ui(80, 24);
        ui.lists
            .worktrees
            .set(vec![Choice {
                name: "pi-rs".into(),
                note: String::new(),
            }])
            .ok();
        let (_dir, lane) = a_running_lane();
        assert_eq!(ui.step_checkout(&lane, true), None);
        assert_eq!(ui.step_checkout(&lane, false), None);
    }

    // The half-typed line belongs to the lane it was typed at. A switch
    // parks it on that lane — the editor is the surface's, and Enter on the
    // checkout just landed on must not file another lane's draft into its
    // session — and it comes back to the editor when the lane does.
    #[tokio::test]
    async fn a_draft_is_parked_on_the_lane_it_was_typed_at_and_comes_back() {
        let first = tempfile::tempdir().expect("a checkout");
        let second = tempfile::tempdir().expect("another checkout");
        let mut tui = surface(first.path());
        tui.ui.editor.set_line("half a thought meant for this lane");

        let was = tui.core.current;
        switch_to(&mut tui, running_lane(second.path()));

        assert_eq!(
            view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
            "half a thought meant for this lane",
            "the draft is parked on the lane that was left"
        );
        assert_eq!(
            tui.ui.editor.text(),
            "",
            "the checkout in front starts its own clean line"
        );

        tui.core.current = was;
        tui.reconcile(was + 1);
        assert_eq!(
            tui.ui.editor.text(),
            "half a thought meant for this lane",
            "the draft is back in the editor with its lane"
        );
        assert_eq!(
            view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
            "",
            "the parked draft was taken up, not left behind"
        );
    }
    // Up browsing recall puts the composed line aside and shows a recalled
    // one; switching then must park the composed line, not the recall.
    #[tokio::test]
    async fn a_switch_mid_recall_keeps_the_line_being_typed() {
        let first = tempfile::tempdir().expect("a checkout");
        let second = tempfile::tempdir().expect("another checkout");
        let mut tui = surface(first.path());
        tui.ui
            .editor
            .seed_history(vec!["an older prompt".to_string()]);
        tui.ui.editor.set_line("half a thought meant for this lane");
        tui.ui.editor.up();
        assert_eq!(tui.ui.editor.text(), "an older prompt");

        let was = tui.core.current;
        switch_to(&mut tui, running_lane(second.path()));

        assert_eq!(
            view_at(&mut tui.views, tui.core.lanes[was].token()).draft,
            "half a thought meant for this lane",
            "the composing line is parked, not the recalled one"
        );
    }

    // Normal mode: `L` is the next checkout and `H` the previous one, and the
    // lowercase pair is left to the caret. `J`/`K` take the window in the
    // same hand, half a screen at a time.
    #[test]
    fn normal_capitals_step_the_checkouts_and_the_window() {
        let mut ui = vim_ui();
        let trees = ["pi-rs", "f1", "f2"]
            .iter()
            .map(|n| Choice {
                name: n.to_string(),
                note: String::new(),
            })
            .collect();
        ui.lists.worktrees.set(trees).ok();
        ui.vim.as_mut().unwrap().mode = Mode::Normal;
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();

        let next = ui.key(&lane, &mut view, typed('L'), false);
        assert!(
            matches!(&next, Asked::Core(Intent::Builtin(Builtin::Worktree(name))) if name == "f1"),
            "{next:?}"
        );
        lane.worktree = Some("f1".into());
        let prev = ui.key(&lane, &mut view, typed('H'), false);
        assert!(
            matches!(&prev, Asked::Core(Intent::Builtin(Builtin::Worktree(name))) if name == "pi-rs"),
            "{prev:?}"
        );

        // The lowercase pair no longer leaves the lane it is typed in.
        for lower in ['h', 'l'] {
            let intent = ui.key(&lane, &mut view, typed(lower), false);
            assert!(
                matches!(intent, Asked::Own(Deed::Nothing)),
                "`{lower}`: {intent:?}"
            );
        }

        // And the window moves without the caret: scrolled up by J, back by K.
        view.surface.scroll = 0;
        ui.key(&lane, &mut view, typed('K'), false);
        let up = view.surface.scroll;
        assert!(up > 0, "K went back through the window: {up}");
        ui.key(&lane, &mut view, typed('J'), false);
        assert!(
            view.surface.scroll < up,
            "J came forward again: {}",
            view.surface.scroll
        );
    }

    // The completion list stays up during a run — `/help` and `/model` answer
    // on the spot then, and the rest queue as what they are. `esc` is the one
    // key it costs, and it costs it for a press: innermost first, so the list
    // goes and the next `esc` reaches the run.
    #[test]
    fn esc_takes_the_list_first_and_the_run_next() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut ui = test_ui(80, 24);
        ui.commands = std::sync::Arc::new(vec![Command {
            word: "/new".into(),
            args: "",
            help: "a fresh session".into(),
            intent: |_, _| Intent::Builtin(Builtin::New),
            source: Source::Builtin,
        }]);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        let esc = || super::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        // Nothing typed raises no list, so the common way to stop a run is one
        // press, as the running row says it is. Committed, or an empty line
        // would mean `Unsend` — a different answer to the same key, and not
        // the one this is about.
        view.state.committed = true;
        assert!(ui.menu().is_empty(), "an empty line completes to nothing");
        let intent = ui.key(&lane, &mut view, esc(), true);
        assert!(matches!(intent, Asked::Own(Deed::Interrupt)), "{intent:?}");

        ui.editor.set_line("/ne");
        assert!(
            !ui.menu().is_empty(),
            "the word is worth completing mid-run"
        );

        // First press: the list, and nothing asked of the loop.
        let intent = ui.key(&lane, &mut view, esc(), true);
        assert!(matches!(intent, Asked::Own(Deed::Nothing)), "{intent:?}");
        assert!(ui.menu().is_empty(), "the list went");
        // Second: through where the list was, to the run.
        let intent = ui.key(&lane, &mut view, esc(), true);
        assert!(matches!(intent, Asked::Own(Deed::Interrupt)), "{intent:?}");

        // Typing past the dismissal brings the list back, and Tab still
        // completes mid-run — the half of the menu the run never claimed.
        ui.editor.set_line("/n");
        let tab = super::TermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        ui.key(&lane, &mut view, tab, true);
        assert_eq!(
            ui.editor.text(),
            "/new",
            "the half-typed word was completed"
        );

        // A panel is modal — `/settings` opens one while a run is in flight —
        // and `esc` there is about the panel, the way it is about the list.
        ui.panel = Some(Panel::new(
            Vec::new(),
            &crate::store::config::Vim::default(),
        ));
        let intent = ui.key(&lane, &mut view, esc(), true);
        assert!(matches!(intent, Asked::Own(Deed::Nothing)), "{intent:?}");
        assert!(
            ui.panel.is_none(),
            "esc closed the panel rather than the run"
        );
    }

    // Whichever door the config comes in by — an edit claimed for the
    // session, or the session value written to the file — it has to land, or
    // the line never moves.
    #[tokio::test]
    async fn a_settings_change_to_the_status_segments_reaches_the_surface() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        assert!(!tui.ui.live.contains(&Segment::Model));

        // The session door: an edit claims the value for this run.
        let said = tui
            .core
            .edit("status.live", r#"["model"]"#)
            .expect("the edit lands");
        tui.land_lines(Listing::say(said));
        assert_eq!(tui.ui.live, vec![Segment::Model]);

        // The file door: the session value goes to the file the config is
        // read from, so the surface's own `--config` is pointed at a temp
        // one. The list it writes is another, to tell the two apart.
        let file = dir.path().join("settings.toml");
        std::fs::write(&file, "").expect("an empty settings file");
        let mut args = <crate::Args as clap::Parser>::parse_from(["pi"]);
        args.config = Some(file.display().to_string());
        tui.core.args = std::sync::Arc::new(args);
        tui.core
            .settings
            .claim("status.done", "[\"cost\"]")
            .expect("a valid claim");
        let said = tui
            .core
            .write_to_file("status.done")
            .expect("the write lands");
        tui.land_lines(Listing::say(said));
        assert_eq!(tui.ui.done, vec![Segment::Cost]);
    }

    // The live region follows the lane's turn, not the clock beside it. One
    // field answering both meant every ending path had to put the clock back
    // or leave a spinner running over a lane that had finished.
    #[test]
    fn the_live_region_ends_with_the_turn_and_not_with_the_clock() {
        let ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        let mut view = View::default();
        view.state.started = Some(std::time::Instant::now());
        assert!(
            ui.live(&lane, &view, false)
                .0
                .iter()
                .any(|r| icons::SPINNER_FRAMES.iter().any(|f| plain(r).contains(f))),
            "a running lane draws the status line"
        );

        lane.run = Run::Idle;
        assert!(
            !ui.live(&lane, &view, false)
                .0
                .iter()
                .any(|r| icons::SPINNER_FRAMES.iter().any(|f| plain(r).contains(f))),
            "the clock is still set; the turn is what says the run is over"
        );
    }

    // The scratch file lives in shared /tmp and carries whatever the user
    // was about to say, so it must not be readable by anyone else.
    #[test]
    fn the_scratch_file_is_private() {
        let path = super::term::scratch_file("hello").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    fn vim_ui() -> super::Ui {
        let mut ui = test_ui(80, 24);
        ui.set_vim(&crate::store::config::Vim {
            enabled: true,
            ..Default::default()
        });
        ui
    }

    fn typed(c: char) -> super::TermEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        super::TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> super::TermEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        super::TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn mode(ui: &super::Ui) -> Option<Mode> {
        ui.vim.as_ref().map(|v| v.mode)
    }

    // The @ completion rides the same menu: Tab lands the path, Enter lands
    // it and stays — the prompt is not sent until the user says so.
    #[test]
    fn an_at_token_completes_by_tab_and_stays_on_enter() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code: KeyCode| super::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let mut ui = test_ui(80, 24);
        let (dir, lane) = a_running_lane();
        let mut view = View::default();
        std::fs::write(dir.path().join("at_probe.rs"), "").unwrap();
        std::fs::write(dir.path().join("at_probe2.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("probe_dir")).unwrap();
        ui.at_root = dir.path().to_path_buf();

        ui.editor.set_line("look at @at_pro");
        let intent = ui.key(&lane, &mut view, key(KeyCode::Tab), false);
        assert_eq!(ui.editor.text(), "look at @at_probe.rs ");
        assert!(matches!(intent, Asked::Own(Deed::Nothing)));

        // A directory keeps completing: no space, so the walk can descend.
        ui.editor.set_line("in @probe_d");
        ui.key(&lane, &mut view, key(KeyCode::Tab), false);
        assert_eq!(ui.editor.text(), "in @probe_dir/");

        // Enter with the list open applies the path instead of sending.
        ui.editor.set_line("look at @at_pro");
        let intent = ui.key(&lane, &mut view, key(KeyCode::Enter), false);
        assert_eq!(ui.editor.text(), "look at @at_probe.rs ");
        assert!(matches!(intent, Asked::Own(Deed::Nothing)));

        // A changed query re-anchors the highlight on the best row: the
        // stale index sat on the worse of the two matches above.
        ui.editor.set_line("look at @at_probe");
        ui.picked = Some(0);
        ui.key(&lane, &mut view, key(KeyCode::Tab), false);
        assert_eq!(ui.editor.text(), "look at @at_probe.rs ");
    }

    // The @ walk follows the checkout in front: a lane on another worktree
    // completes its own files, not the first lane's.
    #[test]
    fn an_at_token_completes_against_the_lane_in_front() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let key = |code: KeyCode| super::TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let was = tempfile::tempdir().unwrap();
        let now = tempfile::tempdir().unwrap();
        std::fs::write(now.path().join("marker.rs"), "").unwrap();
        let mut tui = surface(was.path());
        switch_to(&mut tui, running_lane(now.path()));

        assert_eq!(tui.ui.at_root, now.path());
        tui.ui.editor.set_line("see @mar");
        let token = tui.core.lane().token();
        tui.ui.key(
            tui.core.lane(),
            view_at(&mut tui.views, token),
            key(KeyCode::Tab),
            false,
        );
        assert_eq!(tui.ui.editor.text(), "see @marker.rs ");
    }

    // The sequence is read where unbound characters are typed, and its first
    // half is a real `j` on a real line until the `k` arrives — nothing is
    // held pending, so the screen is never a guess.
    #[test]
    fn jk_leaves_insert_and_takes_its_first_half_back_off_the_line() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();

        ui.key(&lane, &mut view, typed('j'), false);
        assert_eq!(ui.editor.text(), "j", "a lone j is a j");
        assert_eq!(mode(&ui), Some(Mode::Insert));

        ui.key(&lane, &mut view, typed('k'), false);
        assert_eq!(ui.editor.text(), "", "the j goes with the mode change");
        assert_eq!(mode(&ui), Some(Mode::Normal));
    }

    // Outside the window the two characters are just two characters. Without
    // this, a `j` typed minutes ago would still be armed.
    #[test]
    fn a_j_left_behind_does_not_arm_a_later_k() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();

        ui.key(&lane, &mut view, typed('j'), false);
        let stale = super::Instant::now() - std::time::Duration::from_secs(1);
        ui.vim.as_mut().unwrap().last = Some(('j', stale));
        ui.key(&lane, &mut view, typed('k'), false);

        assert_eq!(ui.editor.text(), "jk");
        assert_eq!(mode(&ui), Some(Mode::Insert));
    }

    // A command between the halves breaks the sequence: `j`, a keystroke that
    // means something, then `k` is two commands and a `j`, not a mode change.
    #[test]
    fn a_bound_key_between_the_halves_breaks_the_sequence() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();

        ui.key(&lane, &mut view, typed('j'), false);
        let left = super::TermEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        ui.key(&lane, &mut view, left, false);
        ui.key(&lane, &mut view, typed('k'), false);

        assert_eq!(ui.editor.text(), "kj", "the caret had moved before the k");
        assert_eq!(mode(&ui), Some(Mode::Insert));
    }

    // Normal has to refuse the keys it does not bind. Without this the mode
    // is a costume: `z` would still type a `z` and only the bound keys would
    // behave, which is worse than no mode at all.
    #[test]
    fn an_unbound_character_types_nothing_in_normal() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("hello");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('z'), false);
        assert_eq!(ui.editor.text(), "hello");

        // And the keys it does bind still command.
        ui.key(&lane, &mut view, typed('0'), false);
        ui.key(&lane, &mut view, typed('x'), false);
        assert_eq!(ui.editor.text(), "ello");
    }

    // The way back, and what `a` does that `i` does not.
    #[test]
    fn i_and_a_return_to_insert_on_either_side_of_the_caret() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("ab");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('0'), false);
        ui.key(&lane, &mut view, typed('i'), false);
        assert_eq!(mode(&ui), Some(Mode::Insert));
        ui.key(&lane, &mut view, typed('Z'), false);
        assert_eq!(ui.editor.text(), "Zab", "i types where the caret is");

        ui.vim.as_mut().unwrap().mode = Mode::Normal;
        ui.key(&lane, &mut view, typed('0'), false);
        ui.key(&lane, &mut view, typed('a'), false);
        ui.key(&lane, &mut view, typed('Y'), false);
        assert_eq!(ui.editor.text(), "ZYab", "a types past it");
    }

    // `x` and `D` delete and stay in Normal; these delete the same ranges and
    // leave. The landing is the whole difference, so it is asserted twice.
    #[test]
    fn s_and_c_delete_their_range_and_land_in_insert() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("abcd");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('0'), false);
        ui.key(&lane, &mut view, typed('s'), false);
        assert_eq!(ui.editor.text(), "bcd");
        assert_eq!(mode(&ui), Some(Mode::Insert));
        ui.key(&lane, &mut view, typed('Z'), false);
        assert_eq!(ui.editor.text(), "Zbcd", "s types where the character was");

        ui.vim.as_mut().unwrap().mode = Mode::Normal;
        ui.key(&lane, &mut view, typed('0'), false);
        let right = super::TermEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::NONE,
        ));
        ui.key(&lane, &mut view, right, false);
        ui.key(&lane, &mut view, typed('C'), false);
        assert_eq!(ui.editor.text(), "Z");
        assert_eq!(mode(&ui), Some(Mode::Insert));
        ui.key(&lane, &mut view, typed('Y'), false);
        assert_eq!(ui.editor.text(), "ZY", "C leaves the caret where it cut");
    }

    // `S` and `cc` clear the line and land in Insert.
    #[test]
    fn s_and_cc_change_the_line_without_removing_it() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("ab\ncd\nef");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.editor.buffer_start();
        ui.editor.down();
        ui.key(&lane, &mut view, typed('S'), false);
        assert_eq!(mode(&ui), Some(Mode::Insert));
        assert_eq!(ui.editor.text(), "ab\n\nef");
        ui.key(&lane, &mut view, typed('Z'), false);
        assert_eq!(ui.editor.text(), "ab\nZ\nef");

        ui.vim.as_mut().unwrap().mode = Mode::Normal;
        ui.editor.down();
        ui.key(&lane, &mut view, typed('c'), false);
        ui.key(&lane, &mut view, typed('c'), false);
        assert_eq!(mode(&ui), Some(Mode::Insert));
        assert_eq!(ui.editor.text(), "ab\nZ\n");
        ui.key(&lane, &mut view, typed('Y'), false);
        assert_eq!(ui.editor.text(), "ab\nZ\nY");
    }

    // `o` and `O` open a line on either side of the caret's and land in
    // Insert, which is where the typing goes.
    #[test]
    fn o_and_o_open_a_line_and_land_in_insert() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("ab\ncd");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('O'), false);
        assert_eq!(mode(&ui), Some(Mode::Insert));
        ui.key(&lane, &mut view, typed('Z'), false);
        assert_eq!(
            ui.editor.text(),
            "ab\nZ\ncd",
            "O typed on the line it opened"
        );

        ui.editor.set_line("ab\ncd");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;
        ui.key(&lane, &mut view, typed('o'), false);
        ui.key(&lane, &mut view, typed('Z'), false);
        assert_eq!(
            ui.editor.text(),
            "ab\ncd\nZ",
            "o typed under the caret's line"
        );
    }

    // `dd` takes the line and stays. The first `d` waits, the second fires.
    #[test]
    fn dd_takes_the_whole_line_and_stays_in_normal() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("ab\ncd\nef");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('d'), false);
        assert_eq!(ui.editor.text(), "ab\ncd\nef", "the first d waits");

        ui.key(&lane, &mut view, typed('d'), false);
        assert_eq!(ui.editor.text(), "ab\ncd");
        assert_eq!(mode(&ui), Some(Mode::Normal));

        ui.key(&lane, &mut view, typed('d'), false);
        ui.key(&lane, &mut view, typed('d'), false);
        assert_eq!(ui.editor.text(), "ab");

        ui.key(&lane, &mut view, typed('d'), false);
        ui.key(&lane, &mut view, typed('z'), false);
        ui.key(&lane, &mut view, typed('d'), false);
        assert_eq!(
            ui.editor.text(),
            "ab",
            "an intervening key cancels the sequence"
        );
    }

    // `gg` and `G` run to the buffer's ends, and `^` to the first non-blank.
    #[test]
    fn gg_g_and_caret_walk_the_lines() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.editor.set_line("  ab\ncd");
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('g'), false);
        ui.key(&lane, &mut view, typed('g'), false);
        assert_eq!(ui.editor.cursor(), 0);
        assert_eq!(mode(&ui), Some(Mode::Normal));

        ui.key(&lane, &mut view, typed('^'), false);
        assert_eq!(ui.editor.cursor(), 2, "past the indent");

        ui.key(&lane, &mut view, typed('G'), false);
        assert_eq!(ui.editor.cursor(), ui.editor.text().len());
    }

    // `gg` and `G` take the history's ends while the line is empty and the
    // buffer's once anything is typed.
    #[test]
    fn an_empty_line_sends_gg_and_g_to_the_history_ends() {
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        view.surface.scrollback = (1..=60).map(|n| Row::notice(format!("row {n}"))).collect();
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.key(&lane, &mut view, typed('g'), false);
        ui.key(&lane, &mut view, typed('g'), false);
        ui.flush(&lane, &mut view);
        let top = view.surface.scroll;
        assert!(top > 0, "gg went back through the history: {top}");

        ui.key(&lane, &mut view, typed('K'), false);
        ui.flush(&lane, &mut view);
        assert_eq!(view.surface.scroll, top, "gg is as far back as it goes");

        ui.key(&lane, &mut view, typed('G'), false);
        ui.flush(&lane, &mut view);
        assert_eq!(view.surface.scroll, 0, "G came back to the newest rows");

        // A line to command: the keys stay on it, and the history stays where
        // the user scrolled it to.
        ui.key(&lane, &mut view, typed('K'), false);
        ui.flush(&lane, &mut view);
        let up = view.surface.scroll;
        ui.editor.set_line("ab\ncd");
        ui.key(&lane, &mut view, typed('G'), false);
        assert_eq!(ui.editor.cursor(), 5, "G runs to the text's end");
        ui.key(&lane, &mut view, typed('g'), false);
        ui.key(&lane, &mut view, typed('g'), false);
        ui.flush(&lane, &mut view);
        assert_eq!(ui.editor.cursor(), 0, "and gg to its start");
        assert_eq!(view.surface.scroll, up, "the history stayed where it was");
    }

    // `ctrl+l` weighs the line: with text to lose one press clears it, with
    // nothing there the session is what a press would replace, so it takes
    // two. The press that cleared the line arms nothing toward the second.
    #[test]
    fn ctrl_l_clears_the_line_and_twice_starts_a_session() {
        let mut ui = test_ui(80, 24);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();

        ui.editor.set_line("half-typed");
        let asked = ui.key(&lane, &mut view, ctrl('l'), false);
        assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
        assert!(ui.editor.is_empty(), "the line went on the one press");

        // Pressed again while it is still empty: the clearing press left the
        // double-tap unarmed, so this one only arms it.
        let asked = ui.key(&lane, &mut view, ctrl('l'), false);
        assert!(
            matches!(asked, Asked::Own(Deed::Nothing)),
            "the press that cleared the line started nothing: {asked:?}"
        );
    }

    #[test]
    fn an_empty_line_takes_two_presses_for_a_new_session() {
        let mut ui = test_ui(80, 24);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();

        let asked = ui.key(&lane, &mut view, ctrl('l'), false);
        assert!(
            matches!(asked, Asked::Own(Deed::Nothing)),
            "the first press only arms it: {asked:?}"
        );

        let asked = ui.key(&lane, &mut view, ctrl('l'), false);
        assert!(
            matches!(asked, Asked::Core(Intent::Builtin(Builtin::New))),
            "the second starts the session: {asked:?}"
        );
    }

    // `ctrl+c` gave up clearing the line: it stops the run, and the second
    // press inside the window is the one that leaves.
    #[test]
    fn ctrl_c_stops_the_run_and_leaves_the_line() {
        let mut ui = test_ui(80, 24);
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();

        ui.editor.set_line("half-typed");
        let asked = ui.key(&lane, &mut view, ctrl('c'), false);
        assert!(matches!(asked, Asked::Own(Deed::Nothing)), "{asked:?}");
        assert_eq!(
            ui.editor.text(),
            "half-typed",
            "the line is not its to clear"
        );
    }

    // `v` on an empty line opens the conversation view, and only there: with
    // something on the line it is a letter vim made into a key of its own.
    #[test]
    fn v_opens_the_conversation_view_on_an_empty_line() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let esc = super::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let mut ui = vim_ui();
        let (_dir, lane) = a_running_lane();
        let mut view = View::default();
        ui.vim.as_mut().unwrap().mode = Mode::Normal;
        view.surface.scrollback = (1..=40)
            .flat_map(|n| Row::answer(&format!("answer {n}"), &ui.paint))
            .collect();

        // A line with something on it: `v` is Ignore, and nothing opens.
        ui.editor.set_line("half-typed");
        ui.key(&lane, &mut view, typed('v'), false);
        assert!(!ui.browsing, "a line to command keeps its v");
        assert_eq!(ui.editor.text(), "half-typed");

        ui.editor.clear();
        ui.key(&lane, &mut view, typed('v'), false);
        assert!(ui.browsing);

        // The keys in here are its own: `x` would delete a character in
        // Normal, and `k` scrolls back rather than reaching for history.
        ui.key(&lane, &mut view, typed('x'), false);
        assert_eq!(ui.editor.text(), "", "nothing types into a hidden line");
        ui.key(&lane, &mut view, typed('k'), false);
        assert!(view.surface.scroll > 0, "k walked the conversation back");

        // The window keys are the empty line's own: `K`/`J` half a screen,
        // `G` and `gg` the conversation's two ends.
        let (walked, half) = (view.surface.scroll, ui.half_scroll_step());
        ui.key(&lane, &mut view, typed('K'), false);
        assert_eq!(
            view.surface.scroll,
            walked + half,
            "K is half a window back"
        );
        ui.key(&lane, &mut view, typed('J'), false);
        assert_eq!(view.surface.scroll, walked, "and J undoes it");

        // A lone `g` is half a pair: it moves nothing until the other half
        // arrives.
        ui.key(&lane, &mut view, typed('g'), false);
        assert_eq!(view.surface.scroll, walked, "one g moves nothing");
        ui.key(&lane, &mut view, typed('g'), false);
        assert_eq!(view.surface.scroll, screen::TOP, "gg is the top");
        ui.key(&lane, &mut view, typed('G'), false);
        assert_eq!(view.surface.scroll, 0, "G is the newest rows");

        // A key that is not the second `g` ends the pair: the `k` here scrolls
        // its own row and leaves the `g` before it a first half, no pair.
        ui.key(&lane, &mut view, typed('g'), false);
        ui.key(&lane, &mut view, typed('k'), false);
        ui.key(&lane, &mut view, typed('g'), false);
        assert_eq!(view.surface.scroll, 1, "a k between them is not a gg");

        // The half left armed in here goes with the mode: a `g` on each side
        // of it is not a pair.
        ui.key(&lane, &mut view, typed('g'), false);
        ui.key(&lane, &mut view, esc, false);
        assert!(!ui.browsing);
        assert_eq!(view.surface.scroll, 0, "and leaves at the newest rows");
        ui.key(&lane, &mut view, typed('g'), false);
        assert_eq!(
            view.surface.scroll, 0,
            "a g made after the mode is not its pair"
        );
    }

    // Turning the keys off is the one thing that moves the mode without a
    // keystroke — otherwise switching back on would land in Normal with
    // nothing having asked to go there.
    #[test]
    fn turning_the_keys_off_drops_the_mode_rather_than_parking_it() {
        let mut ui = vim_ui();
        ui.vim.as_mut().unwrap().mode = Mode::Normal;

        ui.set_vim(&crate::store::config::Vim {
            enabled: false,
            ..Default::default()
        });
        assert!(ui.vim.is_none());

        ui.set_vim(&crate::store::config::Vim {
            enabled: true,
            ..Default::default()
        });
        assert_eq!(mode(&ui), Some(Mode::Insert));
    }

    fn test_ui(width: u16, height: u16) -> super::Ui {
        super::Ui::new(
            crate::ui::tui::screen::Screen::test(width, height),
            std::sync::Arc::new(Keys::default()),
            Vec::new(),
            std::sync::Arc::new(Vec::new()),
            super::Lists::new(Store::new(std::env::temp_dir()), std::env::temp_dir()),
            Paint::new(true),
        )
    }

    // A lane as the bar holds one.
    fn tab(mark: super::Mark, name: &str) -> super::Tab {
        super::Tab {
            mark,
            name: name.into(),
        }
    }

    // The bar's row as text, without a screen to read it off.
    fn bar(ui: &super::Ui, width: usize) -> String {
        ui.lane_bar("", width)
            .map(|l| l.to_string())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------- looping

    // Write `body` to `name` in the lane's workspace and record the write, so
    // the tree fingerprint the loop reads has real bytes to hash.
    fn wrote(lane: &mut Lane, name: &str, body: &str) {
        let path = lane.root().join(name);
        std::fs::write(&path, body).expect("writes into the temp workspace");
        lane.ctx.note_write(&path);
    }

    // The whole point of the command: what decides another round is the tree,
    // so a pass that believes it is finished is overruled by the file it just
    // changed.
    #[test]
    fn a_loop_goes_round_while_the_tree_keeps_changing() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("/code-review high".into());

        lane.loop_running();
        wrote(&mut lane, "a.rs", "fn main() {}\n");
        let again = lane.loop_step(None, None).expect("a loop is in force");
        assert!(
            matches!(&again, Round::Again { goal } if goal == "/code-review high"),
            "the goal goes back verbatim",
        );

        // The same file again, with different content — what a loop like this
        // does most of the time is keep working the files it has already
        // touched. A record that only counted writes would call this idle and
        // stop; the fingerprint sees the rewrite.
        lane.loop_running();
        wrote(
            &mut lane,
            "a.rs",
            "fn main() {\n    let x = 1;\n    let y = 2;\n    println!(\"{}\", x + y);\n}\n",
        );
        assert!(matches!(
            lane.loop_step(None, None),
            Some(Round::Again { .. })
        ));

        // Nothing changed: a pass with nothing to do has nothing to do next
        // time either.
        lane.loop_running();
        assert!(matches!(lane.loop_step(None, None), Some(Round::Quiet)));
        assert!(lane.looping().is_none(), "and the loop is gone");
        assert!(
            lane.loop_step(None, None).is_none(),
            "a later turn is not a round"
        );
    }

    // A line typed between rounds ends a turn too. Counting it would move the
    // loop on work it never ran — and end it, if that line wrote nothing.
    #[test]
    fn a_turn_the_loop_did_not_start_is_not_one_of_its_rounds() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());

        // Somebody else's turn settling, mid-loop.
        assert!(lane.loop_step(None, None).is_none(), "not the loop's round");
        assert!(lane.looping().is_some(), "and the loop is untouched");
        assert_eq!(lane.looping().map(|l| l.round), Some(0));

        lane.loop_running();
        wrote(&mut lane, "a.rs", "fn main() {}\n");
        assert!(matches!(
            lane.loop_step(None, None),
            Some(Round::Again { .. })
        ));
    }

    // Esc stops the loop and not merely the round it caught: a cut round is
    // the loop ending, not a pause before the next one.
    #[test]
    fn a_cut_round_takes_the_loop_with_it() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        lane.loop_running();
        wrote(&mut lane, "a.rs", "fn main() {}\n");
        assert!(matches!(
            lane.loop_step(Some(Cut::Stopped), None),
            Some(Round::Cut(Cut::Stopped))
        ));
        assert!(lane.looping().is_none());
    }

    // The ceiling is the floor for a loop no other brake can catch: one that
    // keeps changing files forever without ever repeating itself.
    #[test]
    fn a_loop_stops_at_the_configured_ceiling_with_work_still_left() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        lane.loop_running();
        wrote(&mut lane, "a.rs", "fn main() {}\n");
        assert!(matches!(
            lane.loop_step(None, Some(1)),
            Some(Round::Capped(1))
        ));
        assert!(lane.looping().is_none());

        // The lane primitive itself has no ceiling at `None` — the config's
        // default is layered above it, at `Config::loop_cap`.
        lane.loop_start("go".into());
        lane.loop_running();
        wrote(&mut lane, "b.rs", "fn b() {}\n");
        assert!(matches!(
            lane.loop_step(None, None),
            Some(Round::Again { .. })
        ));
    }

    // A round can outlive the loop that queued it — the surface drops those,
    // and nothing about them may arm a loop that is over.
    #[test]
    fn a_loop_that_has_ended_cannot_be_revived_by_a_stale_round() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        lane.loop_running();
        assert!(matches!(lane.loop_step(None, None), Some(Round::Quiet)));

        // What a queued round would do on its way through if the queue ever let
        // one past a stopped loop: neither of these may bring it back.
        lane.loop_running();
        wrote(&mut lane, "a.rs", "fn main() {}\n");
        assert!(lane.looping().is_none(), "no loop to mark as running");
        assert!(lane.loop_step(None, None).is_none(), "and none to step");
    }

    // A round that restores the tree to a fingerprint it wore earlier is a
    // loop seesawing forever — the round after the rewrite undid it.
    #[test]
    fn a_round_that_undoes_the_last_one_stops_as_oscillating() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        let six = "a\nb\nc\nd\ne\nf\n";
        let six_more = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\n";
        lane.loop_running();
        wrote(&mut lane, "a.rs", six);
        assert!(matches!(
            lane.loop_step(None, None),
            Some(Round::Again { .. })
        ));

        // Round 2 rewrites at full size; round 3 puts the first bytes back —
        // the tree is exactly what round 1 wore, and any later round would
        // seesaw between the two.
        lane.loop_running();
        wrote(&mut lane, "a.rs", six_more);
        assert!(matches!(
            lane.loop_step(None, None),
            Some(Round::Again { .. })
        ));

        lane.loop_running();
        wrote(&mut lane, "a.rs", six);
        assert!(
            matches!(lane.loop_step(None, None), Some(Round::Oscillating)),
            "the tree returned to a fingerprint the loop has already worn"
        );
        assert!(lane.looping().is_none(), "and the loop is gone");
    }

    // The fingerprint cannot catch a loop that keeps nibbling — one line an
    // hour, forever. Two such rounds in a row are the noise floor, and the
    // loop stops rather than polish past the point of return.
    #[test]
    fn rounds_that_only_nibble_stop_as_thin() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());

        lane.loop_running();
        wrote(&mut lane, "a.rs", "one\n");
        assert!(matches!(
            lane.loop_step(None, None),
            Some(Round::Again { .. })
        ));

        lane.loop_running();
        wrote(&mut lane, "a.rs", "one\ntwo\n");
        assert!(matches!(lane.loop_step(None, None), Some(Round::Thin)));
        assert!(lane.looping().is_none());
    }
}
