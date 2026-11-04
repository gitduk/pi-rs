//! Space between conversation blocks, shared by settled and streaming rows.

use super::row::Row;

#[derive(Clone, Copy)]
pub(super) enum Block {
    Prompt,
    PromptContinuation,
    Relayed,
    Tools,
    Prose,
    Status,
    Other,
}

#[derive(Default)]
pub(super) struct Spacing {
    previous: Option<Block>,
    has_turn: bool,
}

impl Spacing {
    pub(super) fn after(rows: &[Row]) -> Self {
        let mut spacing = Self::default();
        for row in rows.iter().filter(|row| row.len() > 0) {
            spacing.push(row.block());
        }
        spacing
    }

    /// Whether this block needs a blank row before it. Streaming continuations
    /// stay inside their block and must not pass through here again.
    pub(super) fn push(&mut self, next: Block) -> bool {
        let gap = match next {
            Block::PromptContinuation => false,
            _ if matches!(
                self.previous,
                Some(Block::Prompt | Block::PromptContinuation)
            ) =>
            {
                true
            }
            Block::Prompt => self.has_turn,
            Block::Prose => matches!(self.previous, Some(Block::Tools)),
            Block::Tools | Block::Relayed | Block::Status | Block::Other => false,
        };
        self.has_turn |= matches!(next, Block::Prompt | Block::Relayed);
        self.previous = Some(next);
        gap
    }
}
