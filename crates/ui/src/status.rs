//! Draws the status line from parts; which parts show is the config's, in
//! `pi_store::status`. Repainted while a turn runs, its last frame kept in
//! scrollback once it ends.
//!
//! Shows only the run in flight, not the whole session's total.

use std::time::Duration;

use pi_core::core::meter::Snapshot;
use pi_store::icons;
use pi_store::status::Segment;
use pi_store::theme::{Attr, Color, Style};
use ratatui::text::{Line, Span};

use crate::render::Paint;

pub const SPIN: Duration = Duration::from_millis(90);

/// A bold pulse that changes hue each breath while staying easy to see.
pub fn breath_style(tick: usize, code: &Style) -> Style {
    let phase = tick % 60;
    let breath = tick / 60 % 5;
    let hue_phase = phase as f64 / 60.0;
    let light = if phase < 28 {
        (1.0 - (phase as f64 / 28.0 * std::f64::consts::PI).cos()) * 0.5
    } else if phase < 32 {
        1.0
    } else {
        (1.0 + ((phase - 32) as f64 / 28.0 * std::f64::consts::PI).cos()) * 0.5
    };
    let colors = [
        (99.0, 168.0, 248.0),
        (70.0, 205.0, 205.0),
        (177.0, 130.0, 245.0),
        (244.0, 166.0, 92.0),
        (130.0, 200.0, 125.0),
    ];
    let from = colors[breath];
    let to = colors[(breath + 1) % colors.len()];
    let blend = hue_phase * hue_phase * (3.0 - 2.0 * hue_phase);
    let base = (
        from.0 + (to.0 - from.0) * blend,
        from.1 + (to.1 - from.1) * blend,
        from.2 + (to.2 - from.2) * blend,
    );
    let color = match code.color.as_ref() {
        Some(Color::Rgb(_, _, _)) => Color::Rgb(
            (base.0 * (0.55 + 0.45 * light)).round() as u8,
            (base.1 * (0.55 + 0.45 * light)).round() as u8,
            (base.2 * (0.55 + 0.45 * light)).round() as u8,
        ),
        Some(color) => color.clone(),
        None => return code.clone().adding(Attr::Bold),
    };
    Style::color(color).adding(Attr::Bold)
}

/// What this part reads as, or None when the run has nothing to say for it.
pub fn render(segment: Segment, s: &Snapshot, running: bool) -> Option<String> {
    Some(match segment {
        Segment::Elapsed if running => return running_time(s.elapsed?),
        Segment::Elapsed => llm::figures::elapsed(s.elapsed?),
        // Each half appears once the provider states it: most state both only
        // as the stream ends, and a waiting line of dashes reads as broken.
        Segment::InOut if s.input == 0 && s.output == 0 => return None,
        Segment::InOut => llm::figures::in_out_so_far(s.input, s.output),
        Segment::Speed => format!("{:.0} t/s", s.speed?),
        Segment::Cache => format!("{}% cached", s.cached?),
        // An unpriced model reports no cost rather than $0.
        Segment::Cost if s.cost <= 0.0 => return None,
        Segment::Cost => llm::figures::dollars(s.cost),
        // Both numbers rather than the share between them: a percentage of
        // a million-token window reads as 0% for most of a session.
        Segment::Ctx => match s.ctx? {
            (_, 0) => return None,
            // `↓`: the run resent smaller than its budget allowed; `/status` says why.
            (used, window) => format!(
                "ctx {}/{}{}",
                llm::figures::short(used as u64),
                llm::figures::size(window as u64),
                if s.refit { "↓" } else { "" }
            ),
        },
        Segment::Compacted if s.compactions == 0 => return None,
        // A count only past one: a second pass in a run is the thing to notice.
        Segment::Compacted if s.compactions == 1 => "compacted".to_string(),
        Segment::Compacted => format!("compacted {}", pi_store::text::times(s.compactions)),
        Segment::Queued if s.queued == 0 => return None,
        Segment::Queued => format!("{} queued", s.queued),
        Segment::Model if s.model.is_empty() => return None,
        Segment::Model => s.model.clone(),
        // Absent in the repository's own checkout, so the line reads as it
        // always did for anyone not using worktrees.
        Segment::Worktree => s.worktree.clone()?,
    })
}

/// The parts that have something to say, in the order asked for.
pub fn parts(segments: &[Segment], s: &Snapshot, running: bool) -> Vec<String> {
    segments
        .iter()
        .filter_map(|&seg| render(seg, s, running))
        .collect()
}

/// Those parts as one line.
pub fn line(segments: &[Segment], s: &Snapshot) -> String {
    settled(&parts(segments, s, false))
}

/// A running clock starts at one second and advances in whole seconds.
pub fn running_time(d: Duration) -> Option<String> {
    (d.as_secs() > 0).then(|| llm::figures::elapsed(d))
}

/// The static mark replaces the live spinner without shifting the figures.
pub fn settled(parts: &[String]) -> String {
    if parts.is_empty() {
        return String::new();
    }
    icons::bar(icons::PI_MARK) + &parts.join(icons::PART_SEP)
}

/// Style the displayed figures, including plain text kept by older runs.
/// Labels, separators and unrecognized notices keep their muted appearance.
pub fn painted(text: &str, paint: &Paint) -> Line<'static> {
    let mut spans = Vec::new();
    let mark = icons::bar(icons::PI_MARK);
    let text = match text.strip_prefix(&mark) {
        Some(rest) => {
            spans.push(paint.span(&paint.theme.muted, mark));
            rest
        }
        None => text,
    };
    for (i, part) in text.split(pi_store::icons::PART_SEP).enumerate() {
        if i > 0 {
            spans.push(paint.span(&paint.theme.muted, pi_store::icons::PART_SEP));
        }
        spans.extend(painted_part(part, paint));
    }
    Line::from(spans)
}

fn painted_part(text: &str, paint: &Paint) -> Vec<Span<'static>> {
    let theme = &paint.theme;
    for suffix in [" t/s", "% cached", " in", " out"] {
        if let Some(value) = text.strip_suffix(suffix)
            && number(value).is_some()
        {
            return vec![
                paint.span(&theme.code, value),
                paint.span(&theme.muted, suffix),
            ];
        }
    }
    if let Some((input, output)) = text.split_once(" in / ")
        && let Some(output) = output.strip_suffix(" out")
        && number(input).is_some()
        && number(output).is_some()
    {
        return vec![
            paint.span(&theme.code, input),
            paint.span(&theme.muted, " in / "),
            paint.span(&theme.code, output),
            paint.span(&theme.muted, " out"),
        ];
    }
    if let Some(value) = text.strip_prefix('$')
        && number(value).is_some()
    {
        return vec![paint.span(&theme.status.ok.adding(Attr::Dim), text)];
    }
    if let Some(context) = text.strip_prefix("ctx ")
        && let Some((used, window)) = context.split_once('/')
        && let (Some(used_value), Some(window_value)) =
            (number(used), number(window.trim_end_matches('↓')))
        && window_value > 0.0
    {
        let share = used_value / window_value;
        let style = if share >= 0.95 {
            &theme.status.err
        } else if share >= 0.8 {
            &theme.status.warn
        } else {
            &theme.code
        };
        return vec![
            paint.span(&theme.muted, "ctx "),
            paint.span(style, used),
            paint.span(&theme.muted, format!("/{window}")),
        ];
    }
    vec![paint.span(&theme.muted, text)]
}

// The same abbreviated counts that `render` writes; reject surrounding words
// so ordinary notices do not acquire status colours.
fn number(text: &str) -> Option<f64> {
    let (digits, scale) = match text.strip_suffix('k') {
        Some(digits) => (digits, 1_000.0),
        None => match text.strip_suffix('m') {
            Some(digits) => (digits, 1_000_000.0),
            None => (text, 1.0),
        },
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    Some(digits.parse::<f64>().ok()? * scale)
}
