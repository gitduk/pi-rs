//! The rows above and under the input line: the layout at
//! `~/.pi/statusline.json`, whose scripts the frame never waits on.

use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use llm::request::Effort;
use ratatui::layout::{Constraint, Layout as RowLayout, Rect};
use ratatui::text::{Line, Span};
use serde::Serialize;

use super::view::{Ending, View, Work, snapshot};
use super::{Mark, Ui, flash_for, screen};
use pi_core::core::lane::Lane;
use pi_core::core::meter::Snapshot;
use pi_store::args::TierArg;
use pi_store::icons;
use pi_store::statusline::{self, Field, Item, Look, Tone};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const MUTED: Look = Look::Tone(Tone::Muted);
// Room between the two sides of a row.
const GAP: usize = 2;

/// What a statusline row reads off the lane in front, and what its scripts are
/// told: one shape for both, so a value cannot drift between the two.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Facts {
    /// The model as its endpoint names it; absent until a lane names one.
    pub model: Option<String>,
    /// The provider its selection identity names, when it names one.
    pub provider: Option<String>,
    /// Absent when the model thinks at no level, `off` among them.
    pub effort: Option<&'static str>,
    pub tier: Option<String>,
    /// Whether a run is in flight, rather than the one that last ended.
    pub running: bool,
    /// What the agent is doing, in the words the `state` field draws.
    pub state: String,
    /// What is wrong and stays so until it is fixed, drawn where a flash
    /// goes whenever none is up; `/status` says why.
    pub alerts: Vec<String>,
    /// The version installed under this pi, while it is a different one.
    pub update: Option<String>,
    /// The latest release, while it is ahead of this pi.
    pub released: Option<String>,
    /// The session in front; absent on a one-shot line, which has no lane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    pub run: Run,
    /// The checkouts on the bar, in its order: state the screen owns, and a
    /// script cannot read anywhere else.
    pub tabs: Vec<Checkout>,
    /// Where the checkout's background executions stand.
    pub jobs: Jobs,
    /// What the session's services last reported, one line per connection;
    /// drawn joined by the `services` field.
    pub services: Vec<String>,
    /// The checkout's directory name; absent for a root with no name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Where the checkout stands in git; read once a second, and absent
    /// outside a repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<pi_store::statusline::Git>,
    /// Whether this run's ceiling lets a script run at all: an outside command
    /// is the same power the `bash` tool asks for.
    #[serde(skip)]
    pub may_run_scripts: bool,
}

/// The session a row speaks for, as its scripts read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Session {
    /// The id it is saved under.
    pub id: String,
    /// What the user calls it, if anything.
    pub name: Option<String>,
    /// What a row names it by: that name, else the first thing it was asked.
    pub title: Option<String>,
    /// When it began, in seconds since the epoch.
    pub started: u64,
    /// The checkout it works in, absolute.
    pub workspace: String,
    /// What the whole session has spent, children and compactions alike.
    pub spent: llm::Totals,
}

/// The run in flight, or the one that last ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Run {
    /// Seconds it has run, or ran; absent before one starts.
    pub elapsed: Option<f64>,
    pub input: u64,
    pub output: u64,
    /// Percent of the prompt read from cache; absent until a request states it.
    pub cache: Option<u64>,
    /// In dollars; zero is an unpriced model rather than a free run.
    pub cost: f64,
    /// Tokens the last request occupied and the window it did so in, as the
    /// provider counted them.
    pub ctx: Option<(usize, usize)>,
    /// The model's window, known before any request states one.
    pub context_window: Option<usize>,
    pub speed: Option<f64>,
    pub compacted: usize,
    pub queued: usize,
    /// The worktree the session works in; absent in the repository's own.
    pub worktree: Option<String>,
    /// The retry it waits out; `state` says `retry` while one is set.
    pub retry: Option<Retry>,
    /// How the run that last ended went; absent while one runs.
    pub outcome: Option<Ending>,
}

/// A retry the run waits out, before it sends again.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Retry {
    pub attempt: usize,
    /// Seconds until the next attempt.
    pub wait: f64,
}

/// One checkout on the bar: its name and how its last run ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Checkout {
    pub name: String,
    pub mark: Mark,
}

/// The checkout's background executions, counted by where each stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub(crate) struct Jobs {
    pub running: usize,
    /// Ended, its results waiting to be delivered.
    pub back: usize,
    pub stopped: usize,
    pub exited: usize,
}

impl Jobs {
    pub(super) fn of(executions: &[pi_core::driver::background::BackgroundCommand]) -> Self {
        use pi_core::driver::background::State;
        let mut jobs = Jobs::default();
        for execution in executions {
            match execution.state {
                State::Running => jobs.running += 1,
                State::Back => jobs.back += 1,
                State::Stopped => jobs.stopped += 1,
                State::Exited => jobs.exited += 1,
            }
        }
        jobs
    }
}

impl Run {
    /// A snapshot's numbers, plus the window only the lane knows.
    fn of(snapshot: &Snapshot, context_window: Option<usize>) -> Self {
        Self {
            elapsed: snapshot.elapsed.map(|d| d.as_secs_f64()),
            input: snapshot.input,
            output: snapshot.output,
            cache: snapshot.cached,
            cost: snapshot.cost,
            ctx: snapshot.ctx,
            context_window,
            speed: snapshot.speed,
            compacted: snapshot.compactions,
            queued: snapshot.queued,
            worktree: snapshot.worktree.clone(),
            retry: None,
            outcome: None,
        }
    }
}

// What only the screen knows of the run: the retry it waits out, and how the
// run that last ended went.
fn screen_of(work: Option<&Work>, ended: Option<Ending>) -> (Option<Retry>, Option<Ending>) {
    let retry = work.and_then(Work::waiting_retry).map(|retry| Retry {
        attempt: retry.attempt,
        wait: retry.wait.as_secs_f64(),
    });
    // A run in flight has no outcome yet; the one that ended left its own.
    let outcome = match work {
        Some(_) => None,
        None => ended,
    };
    (retry, outcome)
}

// The endpoint's own name for the model, or nothing before a lane names one.
fn endpoint_model(model: &str) -> Option<String> {
    (!model.is_empty()).then(|| model.to_string())
}

impl Facts {
    /// What a field reads as, None when it has nothing to say. One arm per
    /// field: its number has one source and its wording one place.
    pub(crate) fn value(&self, field: Field) -> Option<String> {
        let run = &self.run;
        Some(match field {
            Field::Model => self.model.clone()?,
            Field::Effort => self.effort?.to_string(),
            Field::Tier => self.tier.clone()?,
            Field::Update => match (&self.update, &self.released) {
                (Some(installed), _) => format!("restart for {installed}"),
                (None, Some(released)) => format!("{released} available"),
                (None, None) => return None,
            },
            Field::Spent => {
                let spent = self.session.as_ref()?.spent.cost;
                if spent <= 0.0 {
                    return None;
                }
                llm::figures::dollars(spent)
            }
            Field::State if self.state.is_empty() => return None,
            Field::State => self.state.clone(),
            Field::Session => self.session.as_ref()?.title.clone()?,
            Field::Elapsed => {
                let seconds = std::time::Duration::from_secs_f64(run.elapsed?);
                return match self.running {
                    true => crate::status::running_time(seconds, crate::status::SHOWN),
                    false => Some(llm::figures::elapsed(seconds)),
                };
            }
            // Each half appears once the provider states it: most state both
            // only as the stream ends, and a waiting line of dashes reads as
            // broken.
            Field::InOut if run.input == 0 && run.output == 0 => return None,
            Field::InOut => llm::figures::in_out_so_far(run.input, run.output),
            Field::Speed => format!("{:.0} t/s", run.speed?),
            Field::Cache => format!("{}% cached", run.cache?),
            // An unpriced model reports no cost rather than $0.
            Field::Cost if run.cost <= 0.0 => return None,
            Field::Cost => llm::figures::dollars(run.cost),
            Field::Ctx => return crate::status::context(run.ctx?),
            Field::Compacted => return crate::status::compacted(run.compacted),
            Field::Queued if run.queued == 0 => return None,
            Field::Queued => format!("{} queued", run.queued),
            Field::Worktree => run.worktree.clone()?,
            Field::Workspace => self.workspace.clone()?,
            Field::Services if self.services.is_empty() => return None,
            Field::Services => self.services.join(" · "),
            // The surface renders these itself.
            Field::Tabs | Field::Flash | Field::Branch | Field::Diff => return None,
        })
    }

    /// The facts as its scripts read them: the same shape the fields draw
    /// from, so a script's value and the row's cannot disagree.
    pub(crate) fn json(&self) -> String {
        // A row holds no number JSON cannot carry; a line still must not fall
        // over if one ever turns up, so an empty object stands in.
        serde_json::to_string(self).unwrap_or_else(|error| {
            tracing::warn!(target: "pi::statusline", error = %error, "facts not written");
            "{}".into()
        })
    }

    /// The one-shot line's facts: a run's numbers with no lane behind them,
    /// which is all `status.parts` reads.
    pub(crate) fn from_snapshot(run: Snapshot) -> Self {
        Self {
            model: endpoint_model(&run.model_name),
            provider: None,
            effort: None,
            tier: None,
            running: false,
            state: String::new(),
            alerts: Vec::new(),
            update: None,
            released: None,
            session: None,
            run: Run::of(&run, None),
            tabs: Vec::new(),
            jobs: Jobs::default(),
            services: Vec::new(),
            workspace: None,
            git: None,
            may_run_scripts: false,
        }
    }

    pub(super) fn of(
        lane: &Lane,
        view: &View,
        git: Option<statusline::Git>,
        tabs: Vec<Checkout>,
        jobs: Jobs,
        services: Vec<String>,
    ) -> Self {
        let snapshot = snapshot(lane, view);
        let spec = lane.agent().spec();
        let name = lane.name();
        let work = view.state.work.as_ref();
        let (retry, outcome) = screen_of(work, view.state.outcome);
        Self {
            model: endpoint_model(&snapshot.model_name),
            provider: spec.provider().map(str::to_string),
            // A model that takes no effort thinks at no level the run sets.
            effort: Some(match spec.accepts_effort {
                true => lane.agent().brief.effort,
                false => Effort::Off,
            })
            .filter(|effort| *effort != Effort::Off)
            .map(Effort::name),
            tier: Some(TierArg::name(lane.resolved().ceiling.into())),
            running: lane.is_running(),
            state: state_of(work, view),
            alerts: [
                lane.unsaved().map(|_| "unsaved"),
                pi_store::diagnostics::failed().then_some("diagnostics stopped"),
            ]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect(),
            update: pi_core::core::update::newer(),
            released: pi_core::core::update::released(),
            session: Some(Session {
                id: lane.id().to_string(),
                title: name.clone().or_else(|| view.state.first_ask.clone()),
                name,
                started: lane.created(),
                workspace: lane.root().to_string_lossy().into_owned(),
                spent: lane.spent(),
            }),
            run: Run {
                retry,
                outcome,
                ..Run::of(&snapshot, Some(spec.context_window as usize))
            },
            tabs,
            jobs,
            services,
            workspace: lane
                .root()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            git,
            may_run_scripts: pi_store::statusline::may_run_scripts(lane.resolved().ceiling),
        }
    }
}

// The branch as its git directory names it: a line of `HEAD`. Reading it is a
// file read where naming it by git was a fork.
fn read_head(root: &std::path::Path) -> Option<String> {
    let gitdir = pi_core::core::worktree::git_dir(root)?;
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    Some(
        head.strip_prefix("ref: refs/heads/")?
            .trim_end()
            .to_string(),
    )
}

// Where the checkout stands in git: the branch it is on, then the lines its
// worktree has changed against `HEAD`.
pub(super) fn git_of(root: &std::path::Path) -> Option<statusline::Git> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("--no-optional-locks")
            .args(args)
            .current_dir(root)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // `symbolic-ref` still names the branch where `rev-parse` cannot resolve a
    // `HEAD` with no commit behind it; only a detached head reaches the latter.
    let branch = read_head(root)
        .or_else(|| git(&["symbolic-ref", "--short", "HEAD"]))
        .or_else(|| git(&["rev-parse", "--abbrev-ref", "HEAD"]))?;
    let diff = git(&["diff", "--numstat", "HEAD"]).unwrap_or_default();
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        let mut cols = line.split('\t');
        added += cols.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        removed += cols.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    }
    Some(statusline::Git {
        branch,
        added,
        removed,
    })
}

// What the agent is doing, as a row says it: the run in flight's own word,
// and `idle` between runs, so the part never goes blank.
fn state_of(work: Option<&Work>, view: &View) -> String {
    let Some(work) = work else {
        return "idle".to_string();
    };
    match work {
        _ if matches!(work.doing, super::view::Doing::Compact) => "compacting".to_string(),
        _ if work.stopping => "stopping".to_string(),
        _ if let Some(retry) = work.waiting_retry() => {
            format!(
                "retry {} in {}",
                retry.attempt,
                llm::figures::elapsed(retry.wait)
            )
        }
        // Nothing on screen moving: the row says the turn waits rather than
        // works, so the transcript has no line to grow and take back. How long
        // it has waited is `elapsed`'s to say, not this word's.
        _ if matches!(work.doing, super::view::Doing::Turn { .. })
            && view.surface.stream.text().is_empty()
            && !super::call::running(&view.state.tools)
            && work.quiet.elapsed() >= crate::status::SHOWN =>
        {
            "waiting".to_string()
        }
        _ => "working".to_string(),
    }
}

// A part of a row before the row is fitted: the checkouts take whatever
// room the rest leaves, so they are drawn last.
enum Piece {
    Spans(Vec<Span<'static>>),
    Tabs,
}

pub(super) struct StatuslineRow {
    // The spatial text used for selection. Drawing uses the regions below.
    pub(super) line: Line<'static>,
    pub(super) parts: [(Rect, Line<'static>); 2],
}

pub(super) struct FlashHit {
    pub(super) cols: std::ops::Range<usize>,
    pub(super) more: Option<(Line<'static>, super::browse::MoreAreas)>,
}

impl FlashHit {
    fn of(spans: &[Span<'static>], start: usize, cut: bool) -> Self {
        let more = if cut {
            let area = ratatui::layout::Rect::new(start as u16, 0, spans_width(spans) as u16, 1);
            super::browse::MoreAreas::of(area).map(|areas| (Line::from(spans.to_vec()), areas))
        } else {
            None
        };
        Self {
            cols: start..start + spans_width(spans),
            more,
        }
    }
}

impl Ui {
    // What the session's services last reported, one line per connection.
    pub(super) fn sync_services(&mut self, statuses: Vec<String>) {
        if self.services != statuses {
            self.redraw = true;
        }
        self.services = statuses;
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
                    // The one you are in reads as the current thing.
                    Mark::Front => ("", &theme.heading),
                    // Settled lanes are their colour alone: a glyph would repeat
                    // what `Done`/`Failed` already say.
                    Mark::Done => ("", &theme.status.ok),
                    Mark::Failed => ("", &theme.status.err),
                    Mark::Plain => ("", &theme.muted),
                    Mark::Unopened => (icons::UNOPENED_MARK, &theme.muted),
                };
                let name = tab.branch.as_deref().unwrap_or(&tab.name);
                let label = if sign.is_empty() {
                    name.to_string()
                } else {
                    format!("{sign} {name}")
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
        screen::clip_line(&Line::from(spans), strip).spans
    }

    /// The bar's rows, one per layout line. The only place an expired flash
    /// is dropped: every frame passes through here. A flash under the
    /// pointer does not expire; its time starts over once the pointer leaves.
    pub(super) fn statusline_lines(
        &mut self,
        facts: &Facts,
        width: usize,
        session: &std::path::Path,
    ) -> (Vec<StatuslineRow>, Vec<StatuslineRow>) {
        let held = self
            .pointer
            .is_some_and(|(col, row)| self.flash_at(col, row).is_some());
        match &mut self.flash {
            Some((_, at)) if held => *at = Instant::now(),
            Some((line, at)) if at.elapsed() >= flash_for(line) => self.flash = None,
            _ => {}
        }
        self.flash_hit = None;
        // A script runs off the frame: started here, read back once it says
        // something. What has not answered yet stands as its placeholder.
        // A layout outlives the run that wrote it, and the ceiling does not:
        // a command file may only run where `bash` itself would be offered.
        self.scripts
            .start_due(&self.layout, facts.may_run_scripts, session, width, || {
                facts.json()
            });
        let mut over = Vec::new();
        let mut under = Vec::new();
        for line in &self.layout.lines {
            let (row, hit) = self.bar_row(line, facts, width, session);
            match line.place {
                statusline::Place::Top => {
                    if let Some(hit) = hit {
                        self.flash_hit = Some((line.place, over.len(), hit));
                    }
                    over.push(row);
                }
                statusline::Place::Bottom => {
                    if let Some(hit) = hit {
                        self.flash_hit = Some((line.place, under.len(), hit));
                    }
                    under.push(row);
                }
            }
        }
        (over, under)
    }

    // The right side keeps up to half a row too narrow for both; the left
    // folds its checkouts, then is cut. With the row, the columns of the side
    // holding a flash shown, and its page action's columns when present.
    fn bar_row(
        &self,
        line: &statusline::Line,
        facts: &Facts,
        width: usize,
        session: &std::path::Path,
    ) -> (StatuslineRow, Option<FlashHit>) {
        let left = self.pieces(&line.left, facts, session);
        let right = self.pieces(&line.right, facts, session);
        let sep = match &line.sep {
            Some(t) => self.text(&t.text, &t.style).unwrap_or_default(),
            None => vec![self.tab_sep.clone()],
        };
        let gap = if left.is_empty() || right.is_empty() {
            0
        } else {
            GAP
        };
        let (lmin, rw) = (self.fixed(&left, &sep), self.natural(&right, &sep));
        let area = Rect::new(0, 0, width as u16, 1);
        let rroom = if self.natural(&left, &sep) + gap + rw <= width {
            rw
        } else {
            let [_, _, unused] = RowLayout::horizontal([
                Constraint::Length(lmin.min(u16::MAX as usize) as u16),
                Constraint::Length(gap as u16),
                Constraint::Fill(1),
            ])
            .areas(area);
            let [half, _] =
                RowLayout::horizontal([Constraint::Ratio(1, 2), Constraint::Fill(1)]).areas(area);
            rw.min(unused.width.max(half.width) as usize)
        };
        let [left_area, _, right_area] = RowLayout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(gap as u16),
            Constraint::Length(rroom as u16),
        ])
        .areas(area);
        let lcut = self.natural(&left, &sep) > left_area.width as usize;
        let spans = self.join(left, left_area.width as usize, &sep);
        let right = self.join(right, right_area.width as usize, &sep);
        let [_, right_text] = RowLayout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(spans_width(&right) as u16),
        ])
        .areas(right_area);
        let left_line = Line::from(spans);
        let right_line = Line::from(right);
        let shown = self.flash.is_some();
        let mut hit = (shown && holds_flash(&line.left))
            .then(|| FlashHit::of(&left_line.spans, left_area.x as usize, lcut));
        let mut text = left_line.clone();
        if !right_line.spans.is_empty() {
            // Only the selection's text needs spaces; the widgets use Rects.
            text.spans.push(Span::raw(
                " ".repeat((right_text.x as usize).saturating_sub(text.width())),
            ));
            if shown && holds_flash(&line.right) {
                hit = Some(FlashHit::of(
                    &right_line.spans,
                    right_text.x as usize,
                    rw > right_area.width as usize,
                ));
            }
            text.spans.extend(right_line.spans.clone());
        }
        (
            StatuslineRow {
                line: text,
                parts: [(left_area, left_line), (right_text, right_line)],
            },
            hit,
        )
    }

    // What the row draws in this place, in order. A condition adds its items
    // here rather than inside a piece, so the line's own separator still
    // stands between them.
    fn pieces(&self, items: &[Item], facts: &Facts, session: &std::path::Path) -> Vec<Piece> {
        let mut pieces = Vec::new();
        for item in items {
            match item {
                Item::Field(Field::Tabs) => pieces.extend(self.tabs_piece()),
                Item::Styled(s) if s.field == Field::Tabs => pieces.extend(self.tabs_piece()),
                Item::Group(g) => {
                    pieces.extend(self.group_items(&g.group, facts, session).map(Piece::Spans));
                }
                Item::Condition(c) if self.keeps(c, facts, session) => {
                    pieces.extend(self.pieces(&c.items, facts, session));
                }
                Item::Condition(_) => {}
                item => pieces.extend(self.spans(item, facts, session).map(Piece::Spans)),
            }
        }
        pieces
    }

    // Whether a condition's field answers as the condition asks: `unless`
    // waits for silence, `when` for a word.
    fn keeps(&self, c: &statusline::Condition, facts: &Facts, session: &std::path::Path) -> bool {
        let silent = self.silent(c.field, facts, session);
        match c.keeps {
            statusline::Kept::Silent => silent,
            statusline::Kept::Speaking => !silent,
        }
    }

    // Whether a field has nothing to say this frame: what a condition reads,
    // off the same facts the field draws from.
    fn silent(&self, field: Field, facts: &Facts, session: &std::path::Path) -> bool {
        match field {
            Field::Tabs => self.tabs_piece().is_none(),
            field => self.spans(&Item::Field(field), facts, session).is_none(),
        }
    }

    // A lone checkout draws nothing: `workspace` and `branch` already name it,
    // and a strip of one says no more than they do.
    fn tabs_piece(&self) -> Option<Piece> {
        let front = self.tabs.iter().any(|t| t.mark == Mark::Front);
        (front && self.tabs.len() > 1).then_some(Piece::Tabs)
    }

    // One item other than the checkouts, a group or a condition: None when it
    // has nothing to say.
    fn spans(
        &self,
        item: &Item,
        facts: &Facts,
        session: &std::path::Path,
    ) -> Option<Vec<Span<'static>>> {
        let (field, look) = match item {
            Item::Field(part) if part.is_part() => {
                let text = self.value(*part, facts)?;
                return Some(crate::status::painted(&text, &self.paint).spans);
            }
            Item::Field(part) => (*part, &MUTED),
            Item::Styled(s) => (s.field, &s.style),
            Item::Text(t) => return self.text(&t.text, &t.style),
            Item::Script(s) => {
                let answer = self.scripts.shown(s, session);
                let spans = self.script_spans(&answer, &s.style);
                return (!spans.is_empty()).then_some(spans);
            }
            Item::Group(_) | Item::Condition(_) => return None,
        };
        match field {
            Field::Flash if self.flash.is_none() && !facts.alerts.is_empty() => {
                let alerts = facts.alerts.join(icons::PART_SEP);
                Some(vec![self.paint.span(&self.paint.theme.status.err, alerts)])
            }
            Field::Branch => {
                let branch = &facts.git.as_ref()?.branch;
                self.text(branch, look)
            }
            Field::Diff => self.diff_spans(facts.git.as_ref()?),
            _ => self.text(&self.value(field, facts)?, look),
        }
    }

    // Items joined with nothing between. The whole goes when it holds parts and
    // none of them had anything to say: its labels would stand alone. A
    // condition lays out exactly as a group does, so it opens here.
    fn group_items(
        &self,
        items: &[Item],
        facts: &Facts,
        session: &std::path::Path,
    ) -> Option<Vec<Span<'static>>> {
        let (mut spans, mut parts, mut said) = (Vec::new(), 0, 0);
        for item in items {
            let got = match item {
                Item::Condition(c) if self.keeps(c, facts, session) => {
                    self.group_items(&c.items, facts, session)
                }
                Item::Condition(_) => None,
                item => self.spans(item, facts, session),
            };
            if matches!(
                item,
                Item::Field(_) | Item::Styled(_) | Item::Script(_) | Item::Condition(_)
            ) {
                parts += 1;
                said += usize::from(got.is_some());
            }
            spans.extend(got.into_iter().flatten());
        }
        (!spans.is_empty() && (parts == 0 || said > 0)).then_some(spans)
    }

    // What changed, in the diff's own two colours; nothing while the worktree
    // is clean.
    fn diff_spans(&self, git: &statusline::Git) -> Option<Vec<Span<'static>>> {
        let paint = &self.paint;
        let (has_added, has_removed) = (git.added > 0, git.removed > 0);
        if !has_added && !has_removed {
            return None;
        }
        let mut spans = Vec::new();
        if has_added {
            spans.push(paint.span(&paint.theme.diff.add, format!("+{}", git.added)));
        }
        if has_added && has_removed {
            spans.push(paint.span(&paint.theme.muted, "/"));
        }
        if has_removed {
            spans.push(paint.span(&paint.theme.diff.del, format!("-{}", git.removed)));
        }
        Some(spans)
    }

    // What the screen itself has to say for a field: a flash it holds. Every
    // other field is the facts'.
    fn value(&self, field: Field, facts: &Facts) -> Option<String> {
        match field {
            Field::Flash => Some(self.flash.as_ref()?.0.clone()),
            field => facts.value(field),
        }
    }

    fn text(&self, text: &str, look: &Look) -> Option<Vec<Span<'static>>> {
        (!text.is_empty()).then(|| {
            vec![
                self.paint
                    .span(self.tone(look).as_ref(), text.replace('\n', " ")),
            ]
        })
    }

    // A script writes its own colours: the SGR runs it printed colour its text,
    // over the item's own style. With colour off the runs drop.
    fn script_spans(&self, answer: &str, look: &Look) -> Vec<Span<'static>> {
        let base = match self.paint.color {
            true => crate::sgr::style_to_ratatui(self.tone(look).as_ref()),
            false => ratatui::style::Style::default(),
        };
        sgr_spans(answer, base, self.paint.color)
    }

    fn tone<'a>(&'a self, look: &'a Look) -> std::borrow::Cow<'a, pi_store::theme::Style> {
        let theme = &self.paint.theme;
        let named = |tone: Tone| match tone {
            Tone::Muted => &theme.muted,
            Tone::Heading => &theme.heading,
            Tone::Emphasis => &theme.emphasis,
            Tone::Code => &theme.code,
            Tone::Input => &theme.input,
            Tone::Ok => &theme.status.ok,
            Tone::Err => &theme.status.err,
            Tone::Add => &theme.diff.add,
            Tone::Del => &theme.diff.del,
        };
        use std::borrow::Cow;
        match look {
            Look::Style(style) => Cow::Borrowed(style),
            Look::Tone(tone) => Cow::Borrowed(named(*tone)),
            // `adding` builds a fresh style: its SGR memo must not keep the
            // theme's rendering, which has no such attribute in it.
            Look::Toned { tone, sgr } => {
                Cow::Owned(sgr.iter().fold(named(*tone).clone(), |style, attr| {
                    match style.sgr.contains(attr) {
                        true => style,
                        false => style.adding(attr.clone()),
                    }
                }))
            }
        }
    }

    // The width a side takes with its checkouts folded away entirely.
    fn fixed(&self, pieces: &[Piece], sep: &[Span<'static>]) -> usize {
        let seps = pieces.len().saturating_sub(1) * spans_width(sep);
        let spans: usize = pieces
            .iter()
            .map(|p| match p {
                Piece::Spans(s) => spans_width(s),
                Piece::Tabs => 0,
            })
            .sum();
        spans + seps
    }

    // The width a side takes with every checkout shown.
    fn natural(&self, pieces: &[Piece], sep: &[Span<'static>]) -> usize {
        let tabs = if pieces.iter().any(|p| matches!(p, Piece::Tabs)) {
            spans_width(&self.tabs_strip(usize::MAX))
        } else {
            0
        };
        self.fixed(pieces, sep) + tabs
    }

    fn join(&self, pieces: Vec<Piece>, room: usize, sep: &[Span<'static>]) -> Vec<Span<'static>> {
        let strip = room.saturating_sub(self.fixed(&pieces, sep));
        let mut spans = Vec::new();
        for (i, piece) in pieces.into_iter().enumerate() {
            if i > 0 {
                spans.extend_from_slice(sep);
            }
            match piece {
                Piece::Spans(s) => spans.extend(s),
                Piece::Tabs => spans.extend(self.tabs_strip(strip)),
            }
        }
        if spans_width(&spans) <= room {
            return spans;
        }
        let ellipsis = self.paint.span(&self.paint.theme.muted, icons::ELLIPSIS);
        match room {
            0 => return Vec::new(),
            1 => return vec![ellipsis],
            _ => {}
        }
        let mut cut = screen::clip_line(&Line::from(spans), room - 1).spans;
        cut.push(ellipsis);
        cut
    }
}

// Whether a side's items name the flash, in a group or not.
fn holds_flash(items: &[Item]) -> bool {
    items.iter().any(|item| match item {
        Item::Field(part) => *part == Field::Flash,
        Item::Styled(s) => s.field == Field::Flash,
        Item::Group(g) => holds_flash(&g.group),
        Item::Condition(c) => holds_flash(&c.items),
        Item::Text(_) | Item::Script(_) => false,
    })
}

// A script's answer split at its SGR runs: each text it printed, and the
// parameters of the run that follows — absent when nothing closed one.
fn sgr_runs(answer: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut open: Option<pi_store::text::Escape> = None;
    let mut run = String::new();
    for c in answer.chars() {
        if pi_store::text::escaping(&mut open, c) {
            run.push(c);
            if open.is_none() {
                if let Some(params) = sgr_params(&run) {
                    out.push((std::mem::take(&mut text), Some(params)));
                }
                run.clear();
            }
            continue;
        }
        if !c.is_control() {
            text.push(c);
        }
    }
    out.push((text, None));
    out
}

// The parameters of an `ESC [ … m` run: the only escape the row obeys.
fn sgr_params(run: &str) -> Option<String> {
    let params = run.strip_prefix("\x1b[")?.strip_suffix('m')?;
    params
        .chars()
        .all(|c| c.is_ascii_digit() || c == ';')
        .then(|| params.to_string())
}

// A script's answer as spans: its SGR runs colour the text after them in the
// same vocabulary a theme writes.
pub(super) fn sgr_spans(
    answer: &str,
    base: ratatui::style::Style,
    colored: bool,
) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut style = base;
    for (text, params) in sgr_runs(answer) {
        if !text.is_empty() {
            out.push(Span::styled(text, style));
        }
        if let Some(params) = params.filter(|_| colored) {
            style = match params.as_str() {
                // A bare reset returns to the item's own colour, not the
                // terminal's default.
                "" | "0" => base,
                // Each run adds to the one before: `ESC[1m` then `ESC[31m`
                // keeps the bold.
                params => crate::sgr::parse_sgr(params, style),
            };
        }
    }
    // The answer's own edges are not part of it: a leading or trailing blank
    // would otherwise hold the row's space without drawing anything.
    if let Some(first) = out.first_mut() {
        first.content = first.content.trim_start().to_string().into();
    }
    if let Some(last) = out.last_mut() {
        last.content = last.content.trim_end().to_string().into();
    }
    out.retain(|span| !span.content.is_empty());
    out
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// What each script last said, and the one run of it in flight. The row reads
/// this every frame; a script that has not answered yet stands as its own
/// placeholder, so nothing waits and nothing moves.
#[derive(Default)]
pub(super) struct Scripts {
    answers: std::collections::HashMap<String, Answer>,
    tx: Option<UnboundedSender<(String, Option<String>)>>,
    pub(super) rx: Option<UnboundedReceiver<(String, Option<String>)>>,
}

#[derive(Default)]
struct Answer {
    text: Option<String>,
    ran: Option<Instant>,
    running: bool,
}

// How much of a script's answer is read: one line shows, so a flood is capped.
const ANSWER_CAP: u64 = 4096;
// How many script runs may be out at once: a row is not a job queue.
const SCRIPT_RUNS: usize = 4;

/// What a script is told about where it runs: the session the row draws for,
/// the room that row has, and the facts it cannot read for itself.
struct ScriptEnv {
    session: std::path::PathBuf,
    width: usize,
    facts: String,
}

impl Scripts {
    pub(super) fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            answers: Default::default(),
            tx: Some(tx),
            rx: Some(rx),
        }
    }

    /// One run of each script that is due, and none twice over.
    pub(super) fn start_due(
        &mut self,
        layout: &statusline::Layout,
        may_run: bool,
        session: &std::path::Path,
        width: usize,
        facts: impl Fn() -> String,
    ) {
        let runnable = scripts_of(layout, session);
        // Keys the layout no longer names keep neither a slot nor an answer.
        self.answers
            .retain(|key, _| runnable.iter().any(|(_, k)| k == key));
        if !may_run {
            return;
        }
        let mut running = self.answers.values().filter(|a| a.running).count();
        // One document per call, however many scripts read it.
        let mut facts_once: Option<String> = None;
        for (script, key) in runnable {
            if running >= SCRIPT_RUNS {
                break;
            }
            let due = self.answers.get(&key).is_none_or(|a| {
                !a.running
                    && a.ran
                        .is_none_or(|at| at.elapsed().as_secs() >= script.every)
            });
            if !due {
                continue;
            }
            let (Ok(run), Some(tx)) = (script.runnable(), self.tx.clone()) else {
                continue;
            };
            let timeout =
                std::time::Duration::from_secs(script.timeout.clamp(1, statusline::TIMEOUT_MOST));
            let answer = self.answers.entry(key.clone()).or_default();
            answer.running = true;
            answer.ran = Some(Instant::now());
            running += 1;
            let env = ScriptEnv {
                session: session.to_path_buf(),
                width,
                facts: facts_once.get_or_insert_with(&facts).clone(),
            };
            tokio::spawn(async move {
                let said = run_once(run, timeout, env).await;
                let _ = tx.send((key, said));
            });
        }
    }

    /// An empty answer clears the cell. Without an answer, keep its last line.
    pub(super) fn land(&mut self, key: String, said: Option<String>) {
        if said.is_none() {
            tracing::warn!(target: "pi::statusline", script = %key, "no answer");
        }
        let answer = self.answers.entry(key).or_default();
        answer.running = false;
        answer.ran = Some(Instant::now());
        if let Some(text) = said {
            answer.text = Some(text);
        }
    }

    /// What stands in this script's place: its answer, or its own placeholder.
    fn shown(&self, script: &statusline::Script, session: &std::path::Path) -> String {
        let key = key_of(script, session);
        match self.answers.get(&key).and_then(|a| a.text.clone()) {
            Some(text) => text,
            None => script.waiting().to_string(),
        }
    }
}

// A successful read supplies the first line, even when it is blank.
async fn run_once(
    run: statusline::Runnable,
    timeout: std::time::Duration,
    env: ScriptEnv,
) -> Option<String> {
    let mut command = match run {
        statusline::Runnable::Shell(line) => {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(&line);
            command
        }
        statusline::Runnable::File(path) => tokio::process::Command::new(path),
    };
    command
        .env("PI_SESSION_DIR", &env.session)
        .env("PI_COLUMNS", env.width.to_string())
        .env("PI_FACTS", &env.facts);
    let command = command.kill_on_drop(true);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    // One line is all that shows: a script that floods costs a few KiB, not
    // the heap, and the deadline still holds over the read.
    let mut reader = tokio::io::BufReader::new(stdout);
    let answer = async {
        let mut printed = String::new();
        let read = (&mut reader)
            .take(ANSWER_CAP)
            .read_line(&mut printed)
            .await
            .ok()?;
        if let Some(line) = spoken_line(&printed) {
            return Some(line);
        }
        // A blank prefix at the cap only clears the cell if the line ends here.
        if read as u64 == ANSWER_CAP && !printed.ends_with('\n') {
            let rest = reader.fill_buf().await.ok()?;
            if !matches!(rest.first(), None | Some(b'\n')) {
                return None;
            }
        }
        Some(String::new())
    };
    tokio::time::timeout(timeout, answer).await.ok().flatten()
}

// The one line a script printed, or none when it said nothing but colour and
// blanks.
pub(super) fn spoken_line(found: &str) -> Option<String> {
    let spoken = sgr_runs(found)
        .iter()
        .any(|(text, _)| !text.trim().is_empty());
    spoken.then(|| found.trim().to_string())
}

// Every script in the layout, with the key its answer is kept under.
fn scripts_of<'a>(
    layout: &'a statusline::Layout,
    session: &std::path::Path,
) -> Vec<(&'a statusline::Script, String)> {
    layout
        .items()
        .filter_map(|item| match item {
            statusline::Item::Script(s) => Some((s, key_of(s, session))),
            _ => None,
        })
        .collect()
}

fn key_of(script: &statusline::Script, session: &std::path::Path) -> String {
    let label = match script.name.is_some() {
        true => format!("name:{}", script.label()),
        false => format!("command:{}", script.label()),
    };
    // A script's answer belongs to the session it answered for: a lane switch
    // must not pair one session's env with another's output.
    format!("{}|{label}", session.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A script that stopped printing blanks its cell: its condition went away, and
    // the word it said before would otherwise stand there for the rest of the
    // session. Only a run that never answered keeps what it said last time.
    #[test]
    fn a_silent_script_clears_its_cell() {
        use pi_store::statusline::{Item, parse};

        let layout =
            parse(r#"{"lines":[{"left":[{"script":{"command":"printf '%swt' 2","every":30}}]}]}"#)
                .expect("a layout");
        let session = std::path::Path::new("/nowhere");
        let script = match &layout.lines[0].left[0] {
            Item::Script(s) => s,
            other => panic!("not a script: {other:?}"),
        };
        let key = key_of(script, session);
        let mut scripts = Scripts::new();

        assert_eq!(
            scripts.shown(script, session),
            script.waiting(),
            "no answer yet is the placeholder"
        );
        scripts.land(key.clone(), Some("2wt".into()));
        assert_eq!(scripts.shown(script, session), "2wt");

        scripts.land(key.clone(), Some(String::new()));
        assert_eq!(scripts.shown(script, session), "", "the stale count went");

        scripts.land(key.clone(), Some("3wt".into()));
        scripts.land(key, None);
        assert_eq!(scripts.shown(script, session), "3wt", "a lost run keeps it");
    }

    // The three outcomes a run can land in, read off the shell that produces them:
    // a line is drawn, printing nothing blanks the cell whatever the exit status,
    // and a run that reaches its deadline keeps its previous answer.
    #[tokio::test]
    async fn a_run_reads_its_silence_as_nothing_to_say() {
        use pi_store::statusline::Runnable;
        use std::time::Duration;

        let env = || ScriptEnv {
            session: "/nowhere".into(),
            width: 80,
            facts: "{}".into(),
        };
        let run = |line: &str| Runnable::Shell(line.to_string());
        let deadline = Duration::from_secs(10);

        assert_eq!(
            run_once(run("printf 2wt"), deadline, env())
                .await
                .as_deref(),
            Some("2wt")
        );
        let blank_at_cap = format!("printf '%{ANSWER_CAP}s' ''");
        let newline_after_cap = format!("printf '%{ANSWER_CAP}s\\n' ''");
        for silent in [
            "false",
            r"printf '\033[31m '",
            &blank_at_cap,
            &newline_after_cap,
        ] {
            assert_eq!(
                run_once(run(silent), deadline, env()).await.as_deref(),
                Some("")
            );
        }
        let text_after_cap = format!("printf '%{ANSWER_CAP}sOK\\n' ''");
        assert_eq!(run_once(run(&text_after_cap), deadline, env()).await, None);
        assert_eq!(
            run_once(run("exec sleep 30"), Duration::from_millis(200), env()).await,
            None
        );
    }
}
