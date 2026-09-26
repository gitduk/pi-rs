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

use crate::core::lane::{Lane, Run};
use crate::core::looping::{Cut, Round};
use crate::core::{self, Core};
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
    fn adopt_config(&mut self, core: &Core, view: &mut View) {
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
            self.set_theme(view, &core.lane().resolved.context, Arc::new(theme));
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
fn land_handled(ui: &mut Ui, core: &Core, view: &mut View, rows: Listing) {
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
    core: Core,
    // What each lane looks like, keyed by lane token. Held here rather than on
    // the lane because a screen is the surface's: the lane list reorders and
    // drops lanes, and a screen joined to a lane by identity cannot end up
    // drawn for the wrong one when it does.
    views: Views,
    ui: Ui,
    events: UnboundedReceiver<TermEvent>,
    // Stops the reader while a child holds the terminal.
    hold: Hold,
    bridge: core::wechat::Bridge,
}

impl Tui {
    pub fn new(mut core: Core, keys: Arc<Keys>, bridge: core::wechat::Bridge) -> Result<Self> {
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
        let context = core.lane().resolved.context.clone();
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
    fn on_test_screen(mut core: Core, keys: Arc<Keys>) -> Self {
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
            bridge: core::wechat::Bridge::new(),
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
            if unsend && let Some(id) = self.core.lane().last_ask() {
                self.rewind_turn(id);
            }
        }
        // The rows that replay filed — the tally line a run ended on, a warning
        // about it — reached the transcript after their own run was saved, and
        // this lane may not run again before it is left: written here, where the
        // screen already has them, rather than left for a turn that may never
        // come.
        if heard && let Err(e) = self.core.save_lane(self.core.current) {
            self.say_of(self.core.current, core::not_saved(&e));
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
        // A swap lands carrying its own transcript: the lane it moves to was
        // opened with one, so this reads something even when the lane being
        // left has a run writing it.
        self.rebuild_front();
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

    // The front lane's screen out of its transcript: what a swap and a rewind
    // both need, and neither has anything to add to it.
    //
    // A lane whose transcript a job took and could not read back has nothing to
    // rebuild from. That state is named — `NO_TRANSCRIPT` — rather than fatal,
    // so the screen is left as it stands.
    fn rebuild_front(&mut self) {
        let Some(session) = self.core.lane().session() else {
            return;
        };
        self.ui
            .rebuild(front_view(&mut self.views, self.core.lane()), session);
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

    // The panel owns the screen until it is dismissed.
    fn open_panel(&mut self) {
        let rows = self.core.setting_rows();
        self.ui.panel = Some(Panel::new(rows, &self.core.config.vim));
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
                        Some(core::wechat::Inbound::Text { text }) => {
                            let intent = input::read(&text, &self.core.commands);
                            if intent.echoed() {
                                self.echo_sent(&text);
                            }
                            self.admit(Asked::Core(intent))
                        }
                        Some(core::wechat::Inbound::Stop) => self.admit(Asked::Own(Deed::Interrupt)),
                        // The QR, an error, a way out of one: on the lane the
                        // bridge follows, where it lasts and can be re-read.
                        Some(core::wechat::Inbound::Notice(text)) => {
                            self.ui.say(front_view(&mut self.views, self.core.lane()), text);
                            Wake::Nothing
                        }
                        // The bridge saying it is up: one row for a moment.
                        Some(core::wechat::Inbound::Flash(text)) => {
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
            // and the process are not `Core`'s to move.
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
                Step::Panel => self.open_panel(),
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
                // A rewind is refused while a run has the transcript.
                self.rebuild_front();
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
                            .lane()
                            .session()
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
                    core::not_saved(&e),
                );
            }
        }
    }

    // Open the rewind selector on what the user said: a rewind takes a prompt
    // back, and an answer is a place the conversation carries on from.
    fn open_rewind(&mut self) {
        // A lane whose transcript a job took and could not read back has
        // nothing to go back to; said, not panicked.
        let Some(session) = self.core.lane().session() else {
            self.ui.flash(NOTHING_TO_REWIND);
            return;
        };
        let rows: Vec<MenuEntry> = session
            .rewind_nodes()
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
mod tests;
