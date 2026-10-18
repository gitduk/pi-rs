//! The rows a tool call occupies while it runs: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use std::time::Instant;

use llm::message::ToolResult;
use ratatui::style::Stylize;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::row::{self, PendingTool, Row};
use super::scrollback::{Folds, Scrollback};
use crate::render::{Paint, named};
use pi_core::core::tools::modifies;
use pi_core::driver::jobs::Job;
use pi_store::icons;

// A tool call still running. Its line is drawn by the group it will fold
// into when there is one, and by the live block when there is not.
pub(super) struct RunTool {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) summary: String,
    // Where it runs, when not the workspace root.
    pub(super) place: Option<String>,
    pub(super) started: Instant,
    // What the call last said of its progress, if it says any.
    pub(super) progress: Option<String>,
    // Set when the call ended: the row its entry will adopt, parked until the
    // committed entries arrive — and what decides whether it can still fold.
    pub(super) done: Option<Row>,
}

fn out_secs(t: &RunTool, now: Instant) -> Option<u64> {
    let secs = now.saturating_duration_since(t.started).as_secs();
    (t.done.is_none() && secs >= SHOWN_AFTER).then_some(secs)
}

// A quick call is over before a count could be read, and one that shows `1s`
// only to vanish reads as a flicker.
const SHOWN_AFTER: u64 = 2;

// The call's line in the live block: mark, text, elapsed time.
// A modifying call never folds, so it stays drawn here after it ends.
pub(super) fn pending_line(now: Instant, tick: usize, t: &RunTool, paint: &Paint) -> Line<'static> {
    let ended = if modifies(&t.name) {
        None
    } else {
        t.done.as_ref().and_then(Row::ok)
    };
    let muted = |s: String| paint.span(&paint.theme.muted, s);
    let named = named(&t.name, &t.summary) + &crate::render::placed(t.place.as_deref());
    match ended {
        // Mark leads, matching the row this call lands as once folded.
        Some(ok) => {
            let mark = if ok {
                icons::DONE_MARK
            } else {
                icons::FAIL_MARK
            };
            Line::from(muted(format!("{mark} {named}")))
        }
        None => {
            let out = row::out_for(t.progress.as_deref(), out_secs(t, now));
            Line::from(muted(format!("{} {named}{out}", row::call_frame(tick))))
        }
    }
}

// Whether the group draws this call: true from when a foldable call
// starts until it ends badly — a failed call is on its way to its own line.
fn holds(t: &RunTool) -> bool {
    !modifies(&t.name) && t.done.as_ref().is_none_or(|row| row.ok() == Some(true))
}

// Foldable calls, shown from the start so the row doesn't visually jump
// when the result lands.
pub(super) fn held(tools: &[RunTool], now: Instant) -> Vec<PendingTool> {
    tools
        .iter()
        .filter(|t| holds(t))
        .map(|t| PendingTool {
            name: t.name.clone(),
            preview: t.summary.clone(),
            landed: t.done.is_some(),
            progress: t.progress.clone(),
            secs: out_secs(t, now),
            place: t.place.clone(),
            warned: t.done.as_ref().is_some_and(Row::warned),
        })
        .collect()
}

// Calls the live block draws: the complement of `held`'s foldable set.
pub(super) fn drawn(tools: &[RunTool], row_holds: bool) -> Vec<&RunTool> {
    tools.iter().filter(|t| !(row_holds && holds(t))).collect()
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
        && row.ok() == Some(true)
    {
        let tool = row::FoldedTool::new(
            name,
            row.tool_preview().unwrap_or_default(),
            asked,
            &row::result_text(result),
        )
        .at(row.place().map(str::to_string))
        .warned(row.warned());
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
    let state = if job.ended {
        format!("{}done, back when idle", icons::PART_SEP)
    } else {
        row::out_for(None, Some(job.started.elapsed().as_secs()))
    };
    (head, state)
}

// Under an open job: the last this many lines of what it printed.
const BODY_SHOWN: usize = 8;

// One row per job, `hovered` bold; the `open` one whole, wrapped, with the
// tail of `body` under it. Also the job whose own row each is, for a click
// to find; what it printed under it is no one's.
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

pub(super) fn job_lines(
    jobs: &[Job],
    hovered: Option<JobRow>,
    (open, body): (Option<u64>, &str),
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
        let rows = if open == Some(job.id) {
            let (head, tail) = job_text(job);
            let row = Line::from(paint.span_hovered(shine, &paint.theme.muted, head + &tail));
            let said: Vec<String> = (body.lines())
                .map(pi_store::text::screen_line)
                .filter(|l| !l.trim().is_empty())
                .collect();
            let from = said.len().saturating_sub(BODY_SHOWN);
            let mut rows = super::screen::fit(&row, room);
            ids.extend(std::iter::repeat_n(JobRow::Job(job.id), rows.len()));
            if from > 0 {
                let more = format!("  {from} lines before · click to read all");
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
    let w = |s: &str| UnicodeWidthStr::width(s);
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
