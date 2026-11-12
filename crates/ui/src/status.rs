//! Paints the one-shot run's end line from `status.parts`. What each part
//! reads as is `Facts::value`, the same place the statusline's fields read it,
//! so a figure is worded once however it is asked for.

use std::time::Duration;

use pi_core::core::meter::Snapshot;
use pi_store::icons;
use pi_store::statusline::Field;
use pi_store::theme::Attr;
use ratatui::text::{Line, Span};

use crate::render::Paint;
use crate::tui::statusline::Facts;

pub const SPIN: Duration = Duration::from_millis(90);

// Show both counts: a percentage hides useful detail in large windows.
pub(crate) fn context((used, window): (usize, usize)) -> Option<String> {
    if window == 0 {
        return None;
    }
    Some(format!(
        "ctx {}/{}",
        llm::figures::short(used as u64),
        llm::figures::size(window as u64),
    ))
}

pub(crate) fn compacted(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("compacted".into()),
        count => Some(format!("compacted {}", pi_store::text::times(count))),
    }
}

/// The parts that have something to say, in the order asked for.
pub(crate) fn parts(fields: &[Field], facts: &Facts) -> Vec<String> {
    fields
        .iter()
        .filter_map(|&field| facts.value(field))
        .collect()
}

/// Those parts as one line.
pub fn line(fields: &[Field], snap: &Snapshot) -> String {
    settled(&parts(fields, &Facts::from_snapshot(snap.clone())))
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
        && let (Some(used_value), Some(window_value)) = (number(used), number(window))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn value(snapshot: &Snapshot, field: Field) -> Option<String> {
        Facts::from_snapshot(snapshot.clone()).value(field)
    }

    #[test]
    fn context_and_compaction_parts_keep_their_display_rules() {
        let mut snapshot = Snapshot::default();
        assert_eq!(value(&snapshot, Field::Ctx), None);
        snapshot.ctx = Some((1000, 0));
        assert_eq!(value(&snapshot, Field::Ctx), None);
        snapshot.ctx = Some((1000, 1_000_000));
        assert_eq!(value(&snapshot, Field::Ctx).as_deref(), Some("ctx 1.0k/1m"));
        for (count, expected) in [(0, None), (1, Some("compacted")), (2, Some("compacted ×2"))] {
            snapshot.compactions = count;
            assert_eq!(value(&snapshot, Field::Compacted).as_deref(), expected);
        }
    }
}
