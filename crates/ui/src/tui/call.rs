//! The rows a tool call occupies while it runs: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use std::time::{Instant, SystemTime};

use llm::message::ToolResult;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

use super::browse::PageLine;
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
    let named = named(&t.name, &t.summary);
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
        })
        .collect()
}

// Calls the live block draws: the complement of `held`'s foldable set.
pub(super) fn drawn(tools: &[RunTool], row_holds: bool) -> Vec<&RunTool> {
    tools.iter().filter(|t| !(row_holds && holds(t))).collect()
}

// Where a tool's row goes: a call that landed well and is not a write is a
// step of the group; anything else is a line of its own that ends it.
pub(super) fn push_tool_row(
    scrollback: &mut Scrollback,
    folds: &mut Folds,
    row: Row,
    result: &ToolResult,
) {
    let asked = folds.take_asked(&result.call);
    if let Some(name) = row.tool_name().filter(|&n| !modifies(n))
        && row.ok() == Some(true)
    {
        let tool = row::FoldedTool::new(
            name,
            row.tool_preview().unwrap_or_default(),
            &asked,
            &row::result_text(result),
        );
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

// The local time `at`, as HH:MM:SS, its date ahead when not today.
fn clock(at: SystemTime) -> String {
    let (Some(at), Some(now)) = (local(at), local(SystemTime::now())) else {
        return "?".into();
    };
    let time = format!("{:02}:{:02}:{:02}", at.tm_hour, at.tm_min, at.tm_sec);
    if (at.tm_year, at.tm_yday) == (now.tm_year, now.tm_yday) {
        time
    } else {
        format!(
            "{}-{:02}-{:02} {time}",
            at.tm_year + 1900,
            at.tm_mon + 1,
            at.tm_mday
        )
    }
}

fn local(at: SystemTime) -> Option<libc::tm> {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as libc::time_t;
    // SAFETY: `localtime_r` writes only into `tm`, which lives past the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    (!unsafe { libc::localtime_r(&secs, &mut tm) }.is_null()).then_some(tm)
}

// One row per job, `hovered` bold: a click reads it on a page of its own.
// The ticking clock is what says it lives.
pub(super) fn job_lines(
    jobs: &[Job],
    hovered: Option<u64>,
    paint: &Paint,
    width: usize,
) -> Vec<Line<'static>> {
    let room = width.saturating_sub(1);
    let mut lines: Vec<Line<'static>> = jobs
        .iter()
        .take(JOBS_SHOWN)
        .map(|job| {
            let (head, tail) = job_text(job);
            let left = room.saturating_sub(UnicodeWidthStr::width(tail.as_str()));
            let text = pi_store::text::clip(
                &format!(
                    "{}{tail}",
                    pi_store::text::clip(&head, left.saturating_sub(1))
                ),
                room,
            );
            Line::from(paint.span_hovered(hovered == Some(job.id), &paint.theme.muted, text))
        })
        .collect();
    if jobs.len() > JOBS_SHOWN {
        let more = format!("+{} more · /jobs", jobs.len() - JOBS_SHOWN);
        lines.push(Line::from(paint.span(&paint.theme.muted, more)));
    }
    lines
}

// A job read whole: its row uncut, then what it said that is not back yet.
pub(super) fn job_page(job: &Job, waiting: &[String], paint: &Paint) -> Vec<PageLine> {
    let muted = |s: String| Line::from(paint.span(&paint.theme.muted, s));
    // No clock here: the page is not redrawn, so it would stand still.
    let (head, tail) = job_text(job);
    let state = if job.ended { tail.as_str() } else { "" };
    let since = format!("{}started {}", icons::PART_SEP, clock(job.started_at));
    let mut lines = vec![(muted(head + &since + state), None)];
    if waiting.is_empty() {
        lines.push((Line::default(), None));
        lines.push((muted("nothing back from it yet".into()), None));
    }
    for said in waiting {
        lines.push((Line::default(), None));
        lines.extend(said.lines().map(|l| (Line::from(l.to_string()), None)));
    }
    lines
}
