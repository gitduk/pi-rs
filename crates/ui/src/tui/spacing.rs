//! Space between conversation sections, shared by settled and streaming rows.

use super::row::Row;

#[derive(Clone, Copy)]
pub(super) enum Section {
    Prompt,
    PromptContinuation,
    Relayed,
    Tools,
    Prose,
    Other,
}

#[derive(Default)]
pub(super) struct Spacing {
    previous: Option<Section>,
    has_turn: bool,
}

impl Spacing {
    pub(super) fn after(rows: &[Row]) -> Self {
        let mut spacing = Self::default();
        for row in rows.iter().filter(|row| row.len() > 0) {
            spacing.push(row.section());
        }
        spacing
    }

    /// Whether the next section needs a blank row before it. Streaming
    /// continuations stay inside their section, never passing here again.
    pub(super) fn push(&mut self, next: Section) -> bool {
        let gap = match next {
            Section::PromptContinuation => false,
            // A prompt stands apart from what follows it, as it does from
            // what precedes it.
            _ if matches!(
                self.previous,
                Some(Section::Prompt | Section::PromptContinuation)
            ) =>
            {
                true
            }
            Section::Prompt => self.has_turn,
            Section::Prose => matches!(self.previous, Some(Section::Tools)),
            Section::Tools | Section::Relayed | Section::Other => false,
        };
        self.has_turn |= matches!(next, Section::Prompt | Section::Relayed);
        self.previous = Some(next);
        gap
    }
}
