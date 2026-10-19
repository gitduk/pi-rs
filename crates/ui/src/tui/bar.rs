//! The bar over the input line: rows from the layout, or from the script
//! at `~/.pi/bar.rs`, which the frame never waits on.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use llm::request::Effort;
use ratatui::text::{Line, Span};
use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::{Mark, Ui, flash_for, screen};
use pi_core::core::lane::Lane;
use pi_core::core::meter::Snapshot;
use pi_store::args::TierArg;
use pi_store::bar::{self, Item, Layout, Look, Part, Tone};
use pi_store::icons;

const MUTED: Look = Look::Tone(Tone::Muted);
// Room between the two sides of a row.
const GAP: usize = 2;
// A script asking for less would spend a process on every frame or two.
const MIN_REFRESH: f64 = 0.5;
// Retried: the cause may have passed, or the file been fixed.
const RETRY: Duration = Duration::from_secs(5);

/// What the bar reads off the lane in front, and what the script is told.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(super) struct Facts {
    pub model: String,
    pub effort: Effort,
    pub tier: TierArg,
    pub running: bool,
    pub root: PathBuf,
    pub worktree: Option<String>,
    /// Used against budget, in tokens, as the last turn left it.
    pub ctx: Option<(usize, usize)>,
    /// What the session has spent, in dollars: completed turns only, so it
    /// moves a few times a run rather than every frame.
    pub spent: f64,
    /// What is wrong and stays so until it is fixed, drawn where a flash
    /// goes whenever none is up; `/status` says why.
    pub alerts: Vec<String>,
    /// The version installed under this pi, while it is a different one.
    pub update: Option<String>,
    /// The latest release, while it is ahead of this pi.
    pub released: Option<String>,
}

impl Facts {
    pub(super) fn of(lane: &Lane) -> Self {
        Self {
            model: lane.model().to_string(),
            // A model that takes no effort thinks at no level the run sets.
            effort: match lane.agent().spec().accepts_effort {
                true => lane.agent().brief.effort,
                false => Effort::Off,
            },
            tier: lane.resolved().ceiling.into(),
            running: lane.is_running(),
            root: lane.root().to_path_buf(),
            worktree: lane.worktree().map(str::to_string),
            ctx: lane.tally().ctx(),
            spent: lane.tally().session().cost,
            alerts: [
                lane.unsaved().map(|_| "unsaved"),
                pi_store::journal::failed().then_some("journal off"),
            ]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect(),
            update: pi_core::core::update::newer(),
            released: pi_core::core::update::released(),
        }
    }
}

// A part of a row before the row is fitted: the checkouts take whatever
// room the rest leaves, so they are drawn last.
enum Piece {
    Spans(Vec<Span<'static>>),
    Tabs,
}

impl Ui {
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
        screen::clip_line(&Line::from(spans), strip).spans
    }

    /// The bar's rows, one per layout line. The only place an expired flash
    /// is dropped: every frame passes through here. A flash under the
    /// pointer does not expire; its time starts over once the pointer leaves.
    pub(super) fn bar_lines(
        &mut self,
        facts: &Facts,
        snap: &Snapshot,
        width: usize,
    ) -> Vec<Line<'static>> {
        let held = self
            .pointer
            .is_some_and(|(col, row)| self.flash_at(col, row).is_some());
        match &mut self.flash {
            Some((_, at)) if held => *at = Instant::now(),
            Some((line, at)) if at.elapsed() >= flash_for(line) => self.flash = None,
            _ => {}
        }
        self.flash_hit = None;
        let mut rows = Vec::new();
        for (i, line) in self.layout.lines.iter().enumerate() {
            let (row, hit) = self.bar_row(line, facts, snap, width);
            if let Some((cols, cut)) = hit {
                self.flash_hit = Some((i, cols, cut));
            }
            rows.push(row);
        }
        rows
    }

    // The right side keeps up to half a row too narrow for both; the left
    // folds its checkouts, then is cut. With the row, the columns of the side
    // holding a flash shown, and whether that side was cut.
    fn bar_row(
        &self,
        line: &bar::Line,
        facts: &Facts,
        snap: &Snapshot,
        width: usize,
    ) -> (Line<'static>, Option<(std::ops::Range<usize>, bool)>) {
        let left = self.pieces(&line.left, facts, snap);
        let right = self.pieces(&line.right, facts, snap);
        let sep = match &line.sep {
            Some(t) => self.text(t).unwrap_or_default(),
            None => vec![self.tab_sep.clone()],
        };
        let gap = if left.is_empty() || right.is_empty() {
            0
        } else {
            GAP
        };
        let (lmin, rw) = (self.fixed(&left, &sep), self.natural(&right, &sep));
        let rroom = if self.natural(&left, &sep) + gap + rw <= width {
            rw
        } else {
            rw.min(width.saturating_sub(lmin + gap).max(width / 2))
        };
        let lroom = width.saturating_sub(rroom + gap);
        let lcut = self.natural(&left, &sep) > lroom;
        let mut spans = self.join(left, lroom, &sep);
        let right = self.join(right, rroom, &sep);
        let shown = self.flash.is_some();
        let mut hit = (shown && holds_flash(&line.left)).then(|| (0..spans_width(&spans), lcut));
        if !right.is_empty() {
            let used = spans_width(&spans) + spans_width(&right);
            spans.push(Span::raw(" ".repeat(width.saturating_sub(used))));
            if shown && holds_flash(&line.right) {
                hit = Some((width.saturating_sub(spans_width(&right))..width, rw > rroom));
            }
            spans.extend(right);
        }
        (Line::from(spans), hit)
    }

    fn pieces(&self, items: &[Item], facts: &Facts, snap: &Snapshot) -> Vec<Piece> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Part(Part::Tabs) => self.tabs_piece(),
                Item::Styled(s) if s.part == Part::Tabs => self.tabs_piece(),
                Item::Group(g) => self.group(g, facts, snap).map(Piece::Spans),
                item => self.spans(item, facts, snap).map(Piece::Spans),
            })
            .collect()
    }

    // No checkout in front draws nothing, and a separator before nothing
    // would dangle.
    fn tabs_piece(&self) -> Option<Piece> {
        let front = self.tabs.iter().any(|t| t.mark == Mark::Front);
        front.then_some(Piece::Tabs)
    }

    // One item other than the checkouts or a group, None when it has nothing
    // to say.
    fn spans(&self, item: &Item, facts: &Facts, snap: &Snapshot) -> Option<Vec<Span<'static>>> {
        let flash_slot = match item {
            Item::Part(part) => *part == Part::Flash,
            Item::Styled(s) => s.part == Part::Flash,
            _ => false,
        };
        if flash_slot && self.flash.is_none() && !facts.alerts.is_empty() {
            let alerts = facts.alerts.join(icons::PART_SEP);
            return Some(vec![self.paint.span(&self.paint.theme.status.err, alerts)]);
        }
        let (text, look) = match item {
            Item::Part(part) => (self.part(*part, facts, snap)?, &MUTED),
            Item::Styled(s) => (self.part(s.part, facts, snap)?, &s.style),
            Item::Text(t) => return self.text(t),
            Item::Group(_) => return None,
        };
        self.text(&bar::Text {
            text,
            style: look.clone(),
        })
    }

    // A group goes whole when it holds parts and none of them had anything to
    // say: its labels would stand alone.
    fn group(&self, g: &bar::Group, facts: &Facts, snap: &Snapshot) -> Option<Vec<Span<'static>>> {
        let (mut spans, mut parts, mut said) = (Vec::new(), 0, 0);
        for item in &g.group {
            let got = self.spans(item, facts, snap);
            if matches!(item, Item::Part(_) | Item::Styled(_)) {
                parts += 1;
                said += usize::from(got.is_some());
            }
            spans.extend(got.into_iter().flatten());
        }
        (!spans.is_empty() && (parts == 0 || said > 0)).then_some(spans)
    }

    // What a part other than the checkouts reads as, None when nothing.
    fn part(&self, part: Part, facts: &Facts, snap: &Snapshot) -> Option<String> {
        Some(match part {
            Part::Tabs => return None,
            Part::Flash => self.flash.as_ref()?.0.clone(),
            Part::Model => facts.model.clone(),
            Part::Effort if facts.effort == Effort::Off => return None,
            Part::Effort => facts.effort.name().to_string(),
            Part::Tier => facts.tier.name(),
            Part::Update => match (&facts.update, &facts.released) {
                (Some(installed), _) => format!("restart for {installed}"),
                (None, Some(released)) => format!("{released} available"),
                (None, None) => return None,
            },
            Part::Status(segment) => return crate::status::render(segment, snap),
        })
    }

    fn text(&self, t: &bar::Text) -> Option<Vec<Span<'static>>> {
        (!t.text.is_empty()).then(|| {
            vec![
                self.paint
                    .span(self.tone(&t.style), t.text.replace('\n', " ")),
            ]
        })
    }

    fn tone<'a>(&'a self, look: &'a Look) -> &'a pi_store::theme::Style {
        let theme = &self.paint.theme;
        let tone = match look {
            Look::Style(style) => return style,
            Look::Tone(tone) => tone,
        };
        match tone {
            Tone::Muted => &theme.muted,
            Tone::Heading => &theme.heading,
            Tone::Emphasis => &theme.emphasis,
            Tone::Code => &theme.code,
            Tone::Input => &theme.input,
            Tone::Ok => &theme.status.ok,
            Tone::Err => &theme.status.err,
            Tone::Add => &theme.diff.add,
            Tone::Del => &theme.diff.del,
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
        Item::Part(part) => *part == Part::Flash,
        Item::Styled(s) => s.part == Part::Flash,
        Item::Group(g) => holds_flash(&g.group),
        Item::Text(_) => false,
    })
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// The script behind the bar, when there is one: at most one run in flight,
/// and another as soon as what it was told has changed.
pub(super) struct BarScript {
    path: Option<PathBuf>,
    tx: UnboundedSender<Result<Layout, String>>,
    pub(super) rx: UnboundedReceiver<Result<Layout, String>>,
    // What the last run was told, and the script as it stood then: an edit
    // to the file reruns it like any other change.
    sent: Option<(Facts, Option<SystemTime>)>,
    busy: bool,
    due: Option<Instant>,
    failing: bool,
}

impl BarScript {
    /// `bar.rs` in the pi root, if one is there.
    pub(super) fn find() -> Self {
        let path = pi_store::dir()
            .map(|d| d.join("bar.rs"))
            .filter(|p| p.is_file());
        Self::at(path)
    }

    pub(super) fn at(path: Option<PathBuf>) -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            path,
            tx,
            rx,
            sent: None,
            busy: false,
            due: None,
            failing: false,
        }
    }

    /// Run the script if what it would be told changed, the file did, or its
    /// clock is up.
    pub(super) fn poke(&mut self, lane: &Lane) {
        let Some(path) = &self.path else {
            return;
        };
        if self.busy {
            return;
        }
        let now = (
            Facts::of(lane),
            std::fs::metadata(path).and_then(|m| m.modified()).ok(),
        );
        let due = self.due.is_some_and(|at| Instant::now() >= at);
        if self.sent.as_ref() == Some(&now) && !due {
            return;
        }
        let input = serde_json::to_vec(&now.0).unwrap_or_default();
        self.busy = true;
        self.due = None;
        self.sent = Some(now);
        // Its own way out: stopping the lane's run is not stopping the bar.
        let ctx = lane.ctx_for(tokio_util::sync::CancellationToken::new());
        let (path, tx) = (path.clone(), self.tx.clone());
        tokio::spawn(async move {
            let _ = tx.send(pi_core::core::bar::run(&path, input, &ctx).await);
        });
    }

    /// When the loop should wake to run the script again unprompted.
    pub(super) fn due(&self) -> Option<Instant> {
        self.due.filter(|_| !self.busy)
    }

    /// A finished run into the layout to draw. A failure puts the default
    /// back, retries later, and the first time in a row says why.
    pub(super) fn land(
        &mut self,
        out: Result<Layout, String>,
        layout: &mut Layout,
    ) -> Option<String> {
        self.busy = false;
        match out {
            Ok(found) => {
                self.failing = false;
                self.due = found
                    .refresh
                    .filter(|s| s.is_finite() && *s > 0.0)
                    .map(|s| Instant::now() + Duration::from_secs_f64(s.max(MIN_REFRESH)));
                *layout = found;
                None
            }
            Err(why) => {
                tracing::warn!(target: "pi::bar", %why, "bar script failed");
                *layout = Layout::default();
                self.due = Some(Instant::now() + RETRY);
                let first = !std::mem::replace(&mut self.failing, true);
                // Cargo's own warnings come first; the error is what to show.
                let head = why
                    .lines()
                    .find(|l| l.trim_start().starts_with("error"))
                    .or_else(|| why.lines().next())
                    .unwrap_or_default();
                first.then(|| format!("bar.rs: {head}"))
            }
        }
    }
}
