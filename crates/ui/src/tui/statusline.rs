//! The rows above and under the input line: the layout at
//! `~/.pi/statusline.json`, whose scripts the frame never waits on.

use std::time::Instant;
use tokio::io::AsyncBufReadExt;

use llm::request::Effort;
use ratatui::layout::{Constraint, Layout as RowLayout, Rect};
use ratatui::text::{Line, Span};

use super::view::Work;
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

/// What a statusline row reads off the lane in front.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Facts {
    /// The model as its endpoint names it: what a line draws.
    pub model: String,
    pub effort: Effort,
    pub tier: TierArg,
    /// Whether this run's ceiling lets a script run at all: an outside command
    /// is the same power the `bash` tool asks for.
    pub may_run_scripts: bool,
    pub running: bool,
    /// What the agent is doing, in the words a row draws: the `state` field.
    pub state: String,
    /// What the session has spent, in dollars, as each call ends.
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
    pub(super) fn of(lane: &Lane, work: Option<&Work>) -> Self {
        Self {
            model: lane.model_name().to_string(),
            // A model that takes no effort thinks at no level the run sets.
            effort: match lane.agent().spec().accepts_effort {
                true => lane.agent().brief.effort,
                false => Effort::Off,
            },
            tier: lane.resolved().ceiling.into(),
            may_run_scripts: pi_store::statusline::may_run_scripts(lane.resolved().ceiling),
            running: lane.is_running(),
            state: state_of(work),
            spent: lane.spent().cost,
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

// The checkout as the `project` field draws it: the branch in front, and the
// lines its worktree has changed against `HEAD`. Absent outside a repository.
pub(super) fn project_of(root: &std::path::Path) -> Option<statusline::Project> {
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
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"])?;
    let mut added = 0;
    let mut removed = 0;
    for line in git(&["diff", "--numstat", "HEAD"])
        .unwrap_or_default()
        .lines()
    {
        let mut cols = line.split('\t');
        added += cols.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
        removed += cols.next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    }
    Some(statusline::Project {
        branch,
        added,
        removed,
    })
}

// What the agent is doing, as a row says it: the run in flight's own word,
// and `idle` between runs, so the part never goes blank.
fn state_of(work: Option<&Work>) -> String {
    let Some(work) = work else {
        return "idle".to_string();
    };
    match work {
        _ if matches!(work.doing, super::view::Doing::Compact) => "compacting".to_string(),
        _ if work.stopping => "stopping".to_string(),
        _ if let Some(retry) = &work.retry => {
            format!(
                "retry {} in {}",
                retry.attempt,
                llm::figures::elapsed(retry.wait)
            )
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
    pub(super) fn statusline_lines(
        &mut self,
        facts: &Facts,
        snap: &Snapshot,
        width: usize,
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
        self.scripts.start_due(&self.layout, facts.may_run_scripts);
        let mut over = Vec::new();
        let mut under = Vec::new();
        for line in &self.layout.lines {
            let (row, hit) = self.bar_row(line, facts, snap, width);
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
        snap: &Snapshot,
        width: usize,
    ) -> (StatuslineRow, Option<FlashHit>) {
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

    fn pieces(&self, items: &[Item], facts: &Facts, snap: &Snapshot) -> Vec<Piece> {
        items
            .iter()
            .filter_map(|item| match item {
                Item::Field(Field::Tabs) => self.tabs_piece(),
                Item::Styled(s) if s.field == Field::Tabs => self.tabs_piece(),
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
            Item::Field(part) => *part == Field::Flash,
            Item::Styled(s) => s.field == Field::Flash,
            _ => false,
        };
        if flash_slot && self.flash.is_none() && !facts.alerts.is_empty() {
            let alerts = facts.alerts.join(icons::PART_SEP);
            return Some(vec![self.paint.span(&self.paint.theme.status.err, alerts)]);
        }
        let (text, look) = match item {
            Item::Field(part @ Field::Status(_)) => {
                let text = self.field(*part, facts, snap)?;
                return Some(crate::status::painted(&text, &self.paint).spans);
            }
            Item::Field(Field::Project) => return self.project_spans(&MUTED),
            Item::Field(part) => (self.field(*part, facts, snap)?, &MUTED),
            Item::Styled(s) if s.field == Field::Project => return self.project_spans(&s.style),
            Item::Styled(s) => (self.field(s.field, facts, snap)?, &s.style),
            Item::Text(t) => return self.text(t),
            Item::Script(s) => (self.scripts.shown(s), &s.style),
            Item::Group(_) => return None,
        };
        self.text(&statusline::Text {
            text,
            style: look.clone(),
        })
    }

    // A group goes whole when it holds parts and none of them had anything to
    // say: its labels would stand alone.
    fn group(
        &self,
        g: &statusline::Group,
        facts: &Facts,
        snap: &Snapshot,
    ) -> Option<Vec<Span<'static>>> {
        let (mut spans, mut parts, mut said) = (Vec::new(), 0, 0);
        for item in &g.group {
            let got = self.spans(item, facts, snap);
            if matches!(item, Item::Field(_) | Item::Styled(_)) {
                parts += 1;
                said += usize::from(got.is_some());
            }
            spans.extend(got.into_iter().flatten());
        }
        (!spans.is_empty() && (parts == 0 || said > 0)).then_some(spans)
    }

    // The checkout: the branch, then what changed, in the diff's own colours.
    fn project_spans(&self, look: &Look) -> Option<Vec<Span<'static>>> {
        let project = self.project.as_ref()?;
        let mut spans = vec![
            self.paint
                .span(self.tone(look).as_ref(), project.branch.clone()),
        ];
        if project.added + project.removed > 0 {
            spans.push(self.paint.span(&self.paint.theme.muted, " "));
            if project.added > 0 {
                spans.push(
                    self.paint
                        .span(&self.paint.theme.diff.add, format!("+{}", project.added)),
                );
            }
            if project.added > 0 && project.removed > 0 {
                spans.push(self.paint.span(&self.paint.theme.muted, "/"));
            }
            if project.removed > 0 {
                spans.push(
                    self.paint
                        .span(&self.paint.theme.diff.del, format!("-{}", project.removed)),
                );
            }
        }
        Some(spans)
    }

    // What a part other than the checkouts reads as, None when nothing.
    fn field(&self, field: Field, facts: &Facts, snap: &Snapshot) -> Option<String> {
        Some(match field {
            Field::Tabs | Field::Project => return None,
            Field::Flash => self.flash.as_ref()?.0.clone(),
            Field::Model => facts.model.clone(),
            Field::Effort if facts.effort == Effort::Off => return None,
            Field::Effort => facts.effort.name().to_string(),
            Field::Tier => facts.tier.name(),
            Field::Update => match (&facts.update, &facts.released) {
                (Some(installed), _) => format!("restart for {installed}"),
                (None, Some(released)) => format!("{released} available"),
                (None, None) => return None,
            },
            Field::Spent if facts.spent <= 0.0 => return None,
            Field::Spent => llm::figures::dollars(facts.spent),
            Field::State => facts.state.clone(),
            Field::Status(segment) => return crate::status::render(segment, snap, facts.running),
        })
    }

    fn text(&self, t: &statusline::Text) -> Option<Vec<Span<'static>>> {
        (!t.text.is_empty()).then(|| {
            vec![
                self.paint
                    .span(self.tone(&t.style).as_ref(), t.text.replace('\n', " ")),
            ]
        })
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
        Item::Text(_) | Item::Script(_) => false,
    })
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
    pub(super) fn start_due(&mut self, layout: &statusline::Layout, may_run: bool) {
        let runnable = scripts_of(layout);
        // Keys the layout no longer names keep neither a slot nor an answer.
        self.answers
            .retain(|key, _| runnable.iter().any(|(_, k)| k == key));
        if !may_run {
            return;
        }
        let mut running = self.answers.values().filter(|a| a.running).count();
        for (script, key) in runnable {
            if running >= SCRIPT_RUNS {
                break;
            }
            let due = self.answers.get(&key).map_or(true, |a| {
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
            tokio::spawn(async move {
                let said = run_once(run, timeout).await;
                let _ = tx.send((key, said));
            });
        }
    }

    /// A finished run into its slot: the answer, or the last good one kept.
    pub(super) fn land(&mut self, key: String, said: Option<String>) {
        let answer = self.answers.entry(key.clone()).or_default();
        answer.running = false;
        answer.ran = Some(Instant::now());
        match said {
            Some(text) => answer.text = Some(text),
            // Nothing said: killed, failed to start, or exited non-zero. The
            // cell keeps its last answer, and the log says which script.
            None => tracing::warn!(target: "pi::statusline", script = %key, "no answer"),
        }
    }

    /// What stands in this script's place: its answer, or its own placeholder.
    fn shown(&self, script: &statusline::Script) -> String {
        let key = key_of(script);
        match self.answers.get(&key).and_then(|a| a.text.clone()) {
            Some(text) => text,
            None => script.waiting().to_string(),
        }
    }
}

// One run: the shell's line, or the named file. Killed at its own deadline.
async fn run_once(run: statusline::Runnable, timeout: std::time::Duration) -> Option<String> {
    let mut command = match run {
        statusline::Runnable::Shell(line) => {
            let mut command = tokio::process::Command::new("sh");
            command.arg("-c").arg(&line);
            command
        }
        statusline::Runnable::File(path) => tokio::process::Command::new(path),
    };
    let command = command.kill_on_drop(true);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    // One line is all that shows: a script that floods costs a few KiB, not
    // the heap, and the deadline still holds over the read.
    let mut reader = tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(stdout, ANSWER_CAP));
    let mut printed = String::new();
    match tokio::time::timeout(timeout, reader.read_line(&mut printed)).await {
        Ok(Ok(0)) | Err(_) | Ok(Err(_)) => {
            let _ = child.start_kill();
            return None;
        }
        Ok(Ok(_)) => {}
    }
    let _ = child.start_kill();
    let first = printed.lines().next().unwrap_or_default();
    // What a program prints is text to draw, never an escape to obey.
    let said: String = first.chars().filter(|c| !c.is_control()).collect();
    (!said.trim().is_empty()).then(|| said.trim().to_string())
}

// Every script in the layout, with the key its answer is kept under.
fn scripts_of(layout: &statusline::Layout) -> Vec<(&statusline::Script, String)> {
    fn walk<'a>(items: &'a [statusline::Item], out: &mut Vec<(&'a statusline::Script, String)>) {
        for item in items {
            match item {
                statusline::Item::Script(s) => out.push((s, key_of(s))),
                statusline::Item::Group(g) => walk(&g.group, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    for line in &layout.lines {
        walk(&line.left, &mut out);
        walk(&line.right, &mut out);
    }
    out
}

fn key_of(script: &statusline::Script) -> String {
    match script.name.is_some() {
        true => format!("name:{}", script.label()),
        false => format!("command:{}", script.label()),
    }
}
