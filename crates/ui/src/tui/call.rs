//! The rows a tool call occupies until it settles: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use std::time::Instant;

use llm::message::ToolResult;
use ratatui::style::Stylize;
use ratatui::text::Line;

use super::browse::PageLine;
use super::row::{self, PendingTool, Row, Stage, StepMark};
use super::scrollback::{Folds, Scrollback};
use super::ui::OpenJob;
use crate::render::Outcome;
use crate::render::{Paint, named};
use pi_core::core::tools::modifies;
use pi_core::driver::jobs::{Job, State};
use pi_store::icons;

// A tool call not yet settled. Its line is drawn by the group it will fold
// into when there is one, and by the live block when there is not.
pub(super) struct RunTool {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) summary: String,
    // Where it runs, when not the workspace root.
    pub(super) place: Option<String>,
    pub(super) run: Run,
}

// Where a call stands, each stage holding what only it has.
pub(super) enum Run {
    // Taken into its batch, waiting its turn.
    Queued,
    // Running since `since`, with what it last said of its progress.
    Running {
        since: Instant,
        progress: Option<String>,
    },
    // Ended: the row its entry will adopt, parked until the committed
    // entries arrive — and what decides whether it can still fold.
    Landed(Row),
}

impl RunTool {
    // Running, how long so far; landed, how long it ran; queued, nothing.
    fn out_secs(&self, now: Instant) -> Option<u64> {
        let secs = match &self.run {
            Run::Queued => return None,
            Run::Running { since, .. } => now.saturating_duration_since(*since),
            Run::Landed(row) => row.took()?,
        };
        Some(secs.as_secs())
    }

    // The stage as a line draws it.
    fn stage(&self) -> Stage {
        match &self.run {
            Run::Queued => Stage::Queued,
            Run::Running { progress, .. } => Stage::Running {
                progress: progress.clone(),
            },
            // A parked row is always a result's, so it has an outcome.
            Run::Landed(row) => Stage::Landed(row.outcome().unwrap_or(Outcome::Failed)),
        }
    }

    // Whether the group draws this call: true from when a foldable call is
    // queued until it ends badly — a failed call is on its way to its own line.
    fn held(&self) -> bool {
        !modifies(&self.name)
            && match &self.run {
                Run::Landed(row) => row.outcome() != Some(Outcome::Failed),
                Run::Queued | Run::Running { .. } => true,
            }
    }
}

// The call's line in the live block: mark, text, elapsed time. Mark leads,
// matching the row this call lands as once folded.
pub(super) fn pending_line(now: Instant, tick: usize, t: &RunTool, paint: &Paint) -> Line<'static> {
    let stage = t.stage();
    let named = named(&t.name, &t.summary) + &crate::render::placed(t.place.as_deref());
    let (mark, out) = match &t.run {
        // A modifying call never folds, so it stays drawn here, still
        // turning, after it ends: a time beside it would read as finished.
        Run::Landed(_) if modifies(&t.name) => (StepMark::Out(tick), String::new()),
        _ => (
            stage.mark(tick),
            row::out_for(stage.progress(), t.out_secs(now)),
        ),
    };
    let text = format!("{} {named}{out}", mark.glyph());
    Line::from(paint.span(&paint.theme.muted, text))
}

// Foldable calls, shown from when they are queued so the row doesn't jump
// when the result lands.
pub(super) fn held(tools: &[RunTool], now: Instant) -> Vec<PendingTool> {
    tools
        .iter()
        .filter(|t| t.held())
        .map(|t| PendingTool {
            name: t.name.clone(),
            preview: t.summary.clone(),
            stage: t.stage(),
            secs: t.out_secs(now),
            place: t.place.clone(),
        })
        .collect()
}

// Calls the live block draws: the complement of `held`'s foldable set.
pub(super) fn drawn(tools: &[RunTool], row_holds: bool) -> Vec<&RunTool> {
    tools.iter().filter(|t| !(row_holds && t.held())).collect()
}

// Where a tool's row goes: a call that landed well and is not a write is a
// step of the group; anything else is a line of its own that ends it.
// `asked` is what the call asked for, as `Folds::take_asked` keeps it.
pub(super) fn push_tool_row(
    scrollback: &mut Scrollback,
    folds: &mut Folds,
    row: Row,
    asked: &str,
    result: &ToolResult,
) {
    if let Some(name) = row.tool_name().filter(|&n| !modifies(n))
        && let Some(outcome @ (Outcome::Done | Outcome::Warned)) = row.outcome()
    {
        let tool = row::FoldedTool::new(
            name,
            row.tool_preview().unwrap_or_default(),
            asked,
            &row::result_text(result),
        )
        .at(row.place().map(str::to_string))
        .ended(outcome)
        .timed(row.took());
        folds.join(scrollback, row::Step::Tool(tool));
        return;
    }
    scrollback.push(row);
}

// More than this and the list would crowd out the history it sits under.
pub(super) const JOBS_SHOWN: usize = 4;

// What a job's row says, in two: what it is, then how long it has run,
// which a narrow row keeps whole.
fn job_text(job: &Job) -> (String, String) {
    let head = match job.progress.as_str() {
        "" => format!("#{} {}", job.id, job.description),
        progress => format!(
            "#{} {}{}{progress}",
            job.id,
            job.description,
            icons::PART_SEP
        ),
    };
    let state = match job.state {
        State::Running => crate::render::lasted(job.started.elapsed()),
        State::Back => format!("{}done, back when idle", icons::PART_SEP),
        State::Stopped => format!("{}stopped", icons::PART_SEP),
        State::Exited => format!("{}exited", icons::PART_SEP),
    };
    (head, state)
}

// Under an open job: the last this many lines of what it printed.
const BODY_SHOWN: usize = 8;

/// What a row of the job list is, for a click to find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum JobRow {
    Job(u64),
    // Over the tail of what an open job printed: the lines cut above it.
    Earlier(u64),
    // What an open job printed, under its row.
    Under,
    // The row folding the list past `JOBS_SHOWN`, or unfolding it.
    Fold,
}

// What an open job printed, a line per row, blank ones left out.
fn said(body: &str) -> Vec<String> {
    (body.lines())
        .map(pi_store::text::screen_line)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

// A page of all an open job printed, headed by its row.
pub(super) fn job_page(job: &Job, body: &str, paint: &Paint) -> Vec<PageLine> {
    let (head, tail) = job_text(job);
    let head = Line::from(paint.span(&paint.theme.muted, head + &tail));
    let said = said(body).into_iter().map(|l| Line::from(l).into());
    std::iter::once(head.into()).chain(said).collect()
}

// One row per job, `hovered` bold; the `open` one whole, wrapped, with the
// tail of `body` under it. Also the job whose own row each is, for a click
// to find; what it printed under it is no one's.
pub(super) fn job_lines(
    jobs: &[Job],
    hovered: Option<JobRow>,
    open: Option<&OpenJob>,
    all: bool,
    paint: &Paint,
    width: usize,
) -> (Vec<Line<'static>>, Vec<JobRow>) {
    let room = width.saturating_sub(1);
    let (mut lines, mut ids) = (Vec::new(), Vec::new());
    let folds = llm::figures::shown(jobs.len(), JOBS_SHOWN) < jobs.len();
    let shown = if all || !folds {
        jobs.len()
    } else {
        JOBS_SHOWN
    };
    for job in jobs.iter().take(shown) {
        let shine = hovered == Some(JobRow::Job(job.id));
        let rows = if let Some(OpenJob { body, .. }) = open.filter(|o| o.id == job.id) {
            let (head, tail) = job_text(job);
            let row = Line::from(paint.span_hovered(shine, &paint.theme.muted, head + &tail));
            let said = said(body);
            let from = said.len().saturating_sub(BODY_SHOWN);
            let mut rows = super::screen::fit(&row, room);
            ids.extend(std::iter::repeat_n(JobRow::Job(job.id), rows.len()));
            if from > 0 {
                let more = format!("  {from} more");
                let shine = hovered == Some(JobRow::Earlier(job.id));
                rows.push(Line::from(paint.span_hovered(
                    shine,
                    &paint.theme.muted,
                    pi_store::text::clip(&more, room),
                )));
                ids.push(JobRow::Earlier(job.id));
            }
            let tail = (said[from..].iter())
                .map(|l| Line::from(pi_store::text::clip(&format!("  {l}"), room)));
            let own = rows.len();
            rows.extend(tail);
            ids.extend(std::iter::repeat_n(JobRow::Under, rows.len() - own));
            rows
        } else {
            let ([head, last, tail], _) = job_row(job, width);
            let muted = |s: String| paint.span_hovered(shine, &paint.theme.muted, s);
            // What it printed in italics, read apart from what was run.
            let (sep, said) = match last.strip_prefix(icons::PART_SEP) {
                Some(said) => (icons::PART_SEP.to_string(), said.to_string()),
                None => (String::new(), last),
            };
            let mut said = muted(said);
            if paint.color {
                said = said.italic();
            }
            let row = Line::from(vec![muted(head), muted(sep), said, muted(tail)]);
            ids.push(JobRow::Job(job.id));
            vec![row]
        };
        lines.extend(rows);
    }
    if folds {
        let fold = match all {
            true => "click to collapse".to_string(),
            false => format!("{} more", jobs.len() - JOBS_SHOWN),
        };
        let shine = hovered == Some(JobRow::Fold);
        lines.push(Line::from(paint.span_hovered(
            shine,
            &paint.theme.muted,
            fold,
        )));
        ids.push(JobRow::Fold);
    }
    (lines, ids)
}

// A job's row and whether its command was cut: the command, the last line
// it printed, then its clock. A long command keeps half of what the clock
// leaves; the line it printed takes the rest.
fn job_row(job: &Job, width: usize) -> ([String; 3], bool) {
    let room = width.saturating_sub(1);
    let (head, tail) = job_text(job);
    let last = match job.last.as_str() {
        "" => String::new(),
        last => format!("{}{last}", icons::PART_SEP),
    };
    let w = pi_store::text::width;
    let avail = room.saturating_sub(w(&tail));
    let out = w(&last).min((avail / 2).max(avail.saturating_sub(w(&head))));
    let left = avail - out;
    let cut = w(&head) > left;
    ([within(&head, left), within(&last, out), tail], cut)
}

// `s` in at most `cols` columns, an ellipsis ending it when cut.
fn within(s: &str, cols: usize) -> String {
    pi_store::text::clip(s, cols)
}

// Whether a click on a job's row shows more than the row does.
pub(super) fn job_opens(job: &Job, width: usize) -> bool {
    !job.last.is_empty() || job_row(job, width).1
}
