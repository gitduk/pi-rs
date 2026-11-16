//! Context budgets and the effective content sent to the model.

use super::Core;
use crate::input::commands::{Choice, context};
use llm::figures;
use pi_store::listing::{Listing, Role, Row};

mod details;
mod messages;
mod overview;

const ESTIMATES: &str =
    "Estimates use the conservative compaction budget, not a tokenizer or provider count.";

impl Core {
    /// Names from the resolved prompt, including replacements and loaded files.
    pub fn prompt_blocks(&self) -> Vec<Choice> {
        self.lane()
            .resolved()
            .prompt_blocks
            .iter()
            .map(|block| Choice {
                name: block.name.into(),
                note: block.source.clone(),
            })
            .collect()
    }

    pub(super) fn context(&self, args: &str) -> Listing {
        let name = args.trim();
        let lane = self.lane();
        if !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()) {
            return match lane.session() {
                Some(session) => {
                    messages::listing(&session.context_view(), lane.agent().spec(), name)
                }
                None => Listing::say([
                    "Message details are unavailable during a run. Try again after it finishes.",
                ]),
            };
        }
        if !name.is_empty() {
            let items = context::items(&self.prompt_blocks(), Some(&lane.agent().brief.registry));
            return match items.iter().find(|item| item.choice.name == name) {
                Some(item) => details::listing(self, &item.target),
                None => Listing::say([
                    format!("No context item named {name}."),
                    "/context · show all directory names and message numbers".into(),
                ]),
            };
        }
        let mut rows = overview::rows(lane);
        rows.push(Row::new([""]));
        rows.push(
            Row::new(["Estimated tokens · % of full window · /context <name> for details"])
                .with_role(Role::Footer),
        );
        Listing::report(rows)
    }
}

// Whether the transcript holds prior-turn reasoning — worth saying at a
// switch, since it's the one part of history that doesn't survive intact.
pub(super) fn carries_reasoning(session: &agent::session::Session) -> bool {
    // The view, not every entry: what compaction has already dropped is not
    // going to reach the new model in any form, demoted or otherwise.
    session.view().iter().any(|s| {
        s.entry().blocks().is_some_and(|bs| {
            bs.iter()
                .any(|b| matches!(b, llm::message::AssistantContent::Reasoning(_)))
        })
    })
}

fn percentage(part: usize, whole: usize) -> String {
    if whole == 0 {
        String::new()
    } else {
        format!("{:.1}%", part as f64 * 100.0 / whole as f64)
    }
}

fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        String::new()
    } else {
        format!(" · {}", percentage(part, whole))
    }
}

fn share(name: &str, tokens: usize, total: usize) -> Row {
    Row::new([
        name.into(),
        format!(
            "{}  ~{} tokens{}",
            bar(tokens, total, 20),
            figures::short(tokens as u64),
            percent(tokens, total)
        ),
    ])
}

// Whole cells keep the filled segment continuous across terminal fonts.
fn bar(part: usize, whole: usize, width: usize) -> String {
    let filled = part
        .min(whole)
        .saturating_mul(width)
        .saturating_add(whole / 2)
        .checked_div(whole)
        .unwrap_or(0);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}
