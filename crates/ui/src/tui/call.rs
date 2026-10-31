//! The rows a tool call occupies until it settles: the line it holds itself, and
//! the group that draws it when there is one to fold into.

use std::time::Instant;

use llm::message::ToolResult;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};

use super::browse::PageLine;
use super::mouse::Target;
use super::row::{self, PendingTool, Row, Stage, StepMark};
use super::scrollback::{Folds, Scrollback};
use super::ui::OpenCommand;
use crate::render::Outcome;
use crate::render::{Paint, named};
use pi_core::core::tools::modifies;
use pi_core::driver::background::{BackgroundCommand, Invocation, State};
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
    let named = named(&t.name, &t.summary);
    let (mark, out) = match &t.run {
        // A modifying call never folds, so it stays drawn here, still
        // turning, after it ends: a time beside it would read as finished.
        Run::Landed(_) if modifies(&t.name) => (StepMark::Out(tick), String::new()),
        _ => (
            stage.mark(tick),
            row::out_for(stage.progress(), t.out_secs(now)),
        ),
    };
    let mut spans = vec![paint.span(&paint.theme.muted, format!("{} ", mark.glyph()))];
    spans.extend(paint.tool_call(&t.name, &named, false));
    spans.push(paint.span(
        &paint.theme.muted,
        crate::render::placed(t.place.as_deref()) + &out,
    ));
    Line::from(spans)
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
            &llm::message::flatten(&result.content),
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
pub(super) const BACKGROUND_SHOWN: usize = 4;

// What a command's row says, in two: what it is, then how long it has run,
// which a narrow row keeps whole.
fn background_text(execution: &BackgroundCommand) -> (String, String) {
    let title = execution.invocation.title();
    let head = match execution.progress.as_str() {
        "" => format!("#{} {title}", execution.id),
        progress => format!("#{} {}{}{progress}", execution.id, title, icons::PART_SEP),
    };
    let state = match execution.state {
        State::Running => crate::status::running_time(execution.started.elapsed())
            .map_or_else(String::new, |time| format!("{}{time}", icons::PART_SEP)),
        State::Back => format!("{}done, back when idle", icons::PART_SEP),
        State::Stopped => format!("{}stopped", icons::PART_SEP),
        State::Exited => format!("{}exited", icons::PART_SEP),
    };
    (head, state)
}

// Truncation may cut the head inside the id.
fn background_head(
    execution: &BackgroundCommand,
    head: &str,
    hovered: bool,
    paint: &Paint,
) -> Vec<Span<'static>> {
    let id = format!("#{} ", execution.id);
    let Some(rest) = head.strip_prefix(&id) else {
        return vec![paint.span_hovered(hovered, &paint.theme.muted, head)];
    };
    let title = execution.invocation.title();
    let (description, progress) = match rest.strip_prefix(title.as_ref()) {
        Some(progress) => (title.as_ref(), progress),
        None => (rest, ""),
    };
    let mut spans = vec![paint.span_hovered(hovered, &paint.theme.muted, id)];
    spans.extend(match execution.invocation.as_ref() {
        Invocation::Shell { .. } => paint.shell(description, hovered),
        Invocation::Tool { name, .. } => paint.tool_call(name, description, hovered),
    });
    spans.push(paint.span_hovered(hovered, &paint.theme.muted, progress));
    spans
}

// Under an open execution: the last this many rows of what it printed.
const BODY_SHOWN: usize = 8;

// The last lines of `said` wrapped in `room`, in at most `BODY_SHOWN` rows,
// and how many lines are not shown whole: the page has those.
pub(super) fn tail_rows(said: &[String], room: usize) -> (usize, Vec<Line<'static>>) {
    let indent = Line::from("  ");
    let (mut from, mut rows) = (said.len(), Vec::new());
    for l in said.iter().rev() {
        let mut wrapped = super::screen::wrap(Some(&indent), &Line::from(l.clone()), room);
        if rows.len() + wrapped.len() > BODY_SHOWN {
            // A last line taller than all the room keeps its own last rows.
            if rows.is_empty() {
                rows = wrapped.split_off(wrapped.len() - BODY_SHOWN);
            }
            break;
        }
        wrapped.append(&mut rows);
        rows = wrapped;
        from -= 1;
    }
    (from, rows)
}

/// What a row of the execution list is, for a click to find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CommandRow {
    BackgroundCommand(u64),
    // Over the tail of what an open execution printed: the lines cut above it.
    Earlier(u64),
    // What an open execution printed, under its row.
    Under,
    // The row folding the list past `BACKGROUND_SHOWN`, or unfolding it.
    Fold,
}

// What an open execution printed, a line per row, blank ones left out.
fn said(body: &str) -> Vec<String> {
    (body.lines())
        .map(pi_store::text::screen_line)
        .filter(|l| !l.trim().is_empty())
        .collect()
}

// A page of all an open execution printed, headed by its row.
pub(super) fn background_page(
    execution: &BackgroundCommand,
    body: &str,
    paint: &Paint,
) -> Vec<PageLine> {
    let (head, tail) = background_text(execution);
    let mut spans = background_head(execution, &head, false, paint);
    spans.push(paint.span(&paint.theme.muted, tail));
    let head = Line::from(spans);
    let said = said(body).into_iter().map(|l| Line::from(l).into());
    std::iter::once(head.into()).chain(said).collect()
}

// Finished executions can accumulate while a turn holds their delivery.
pub(super) fn background_results(
    executions: &[BackgroundCommand],
    open: bool,
    hovered: Option<Target>,
    paint: &Paint,
    width: usize,
    at: usize,
) -> (Vec<Line<'static>>, Vec<Target>) {
    let executions = executions.iter().filter(|j| j.state == State::Back);
    let count = executions.clone().count();
    if count == 0 {
        return (Vec::new(), Vec::new());
    }
    let room = width.saturating_sub(1);
    let text = format!(
        "{} {} waiting{}click to {}",
        icons::RELAYED_MARK,
        row::count(count, "execution result"),
        icons::PART_SEP,
        if open { "collapse" } else { "expand" },
    );
    let shine = matches!(hovered, Some(Target::BackgroundResults(_)));
    let mut spans = vec![paint.action_hint(shine, within(&text, room))];
    if !open && let Some(execution) = executions.clone().rev().find(|j| !j.last.is_empty()) {
        let latest = format!("{}#{} {}", icons::PART_SEP, execution.id, execution.last);
        spans.push(paint.span(
            &paint.theme.muted,
            within(&latest, room.saturating_sub(pi_store::text::width(&text))),
        ));
    }
    let mut lines = vec![Line::from(spans)];
    let mut targets = vec![Target::BackgroundResults(at)];
    if open {
        for execution in executions {
            let (head, _) = background_text(execution);
            let ([head, last, tail], _) =
                fit_background_row(head, String::new(), &execution.last, width);
            let shine =
                matches!(hovered, Some(Target::BackgroundResult(_, id)) if id == execution.id);
            lines.push(closed_command_line(
                execution, &head, last, tail, shine, paint,
            ));
            targets.push(Target::BackgroundResult(at + targets.len(), execution.id));
        }
    }
    (lines, targets)
}

fn closed_command_line(
    execution: &BackgroundCommand,
    head: &str,
    last: String,
    tail: String,
    shine: bool,
    paint: &Paint,
) -> Line<'static> {
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
    let mut spans = background_head(execution, head, shine, paint);
    spans.extend([muted(sep), said, muted(tail)]);
    Line::from(spans)
}

// One row per execution, `hovered` bold; the `open` one whole, wrapped, with the
// tail of `body` under it. Also the execution whose own row each is, for a click
// to find; what it printed under it is no one's.
pub(super) fn background_lines(
    executions: &[BackgroundCommand],
    hovered: Option<CommandRow>,
    open: Option<&OpenCommand>,
    all: bool,
    paint: &Paint,
    width: usize,
) -> (Vec<Line<'static>>, Vec<CommandRow>) {
    let room = width.saturating_sub(1);
    let (mut lines, mut ids) = (Vec::new(), Vec::new());
    let folds = llm::figures::shown(executions.len(), BACKGROUND_SHOWN) < executions.len();
    let shown = if all || !folds {
        executions.len()
    } else {
        BACKGROUND_SHOWN
    };
    for execution in executions.iter().take(shown) {
        let shine = hovered == Some(CommandRow::BackgroundCommand(execution.id));
        let rows = if let Some(OpenCommand { body, .. }) = open.filter(|o| o.id == execution.id) {
            let (head, tail) = background_text(execution);
            let mut spans = background_head(execution, &head, shine, paint);
            spans.push(paint.span_hovered(shine, &paint.theme.muted, tail));
            let row = Line::from(spans);
            let (from, tail) = tail_rows(&said(body), room);
            let mut rows = super::screen::fit(&row, room);
            ids.extend(std::iter::repeat_n(
                CommandRow::BackgroundCommand(execution.id),
                rows.len(),
            ));
            if from > 0 {
                let more = format!("  {from} more");
                let shine = hovered == Some(CommandRow::Earlier(execution.id));
                rows.push(Line::from(
                    paint.action_hint(shine, pi_store::text::clip(&more, room)),
                ));
                ids.push(CommandRow::Earlier(execution.id));
            }
            let own = rows.len();
            rows.extend(tail);
            ids.extend(std::iter::repeat_n(CommandRow::Under, rows.len() - own));
            rows
        } else {
            let ([head, last, tail], _) = background_row(execution, width);
            let row = closed_command_line(execution, &head, last, tail, shine, paint);
            ids.push(CommandRow::BackgroundCommand(execution.id));
            vec![row]
        };
        lines.extend(rows);
    }
    if folds {
        let fold = match all {
            true => "click to collapse".to_string(),
            false => format!("{} more", executions.len() - BACKGROUND_SHOWN),
        };
        let shine = hovered == Some(CommandRow::Fold);
        lines.push(Line::from(paint.action_hint(shine, fold)));
        ids.push(CommandRow::Fold);
    }
    (lines, ids)
}

// A command's row and whether its command was cut: the command, the last line
// it printed, then its clock. A long command keeps half of what the clock
// leaves; the line it printed takes the rest.
fn background_row(execution: &BackgroundCommand, width: usize) -> ([String; 3], bool) {
    let (head, tail) = background_text(execution);
    fit_background_row(head, tail, &execution.last, width)
}

fn fit_background_row(head: String, tail: String, last: &str, width: usize) -> ([String; 3], bool) {
    let room = width.saturating_sub(1);
    let last = match last {
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

// Whether a click on a command's row shows more than the row does.
pub(super) fn background_opens(execution: &BackgroundCommand, width: usize) -> bool {
    !execution.last.is_empty() || background_row(execution, width).1
}
