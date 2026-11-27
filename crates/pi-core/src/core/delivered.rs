//! Which skills' instructions already sit in the view the model reads.

use std::collections::{HashMap, HashSet};

use agent::session::{Entry, Seen, Session};
use skills::Load;

/// Names whose instructions are in `session`'s view, wherever they arrived
/// from: an ask that carried them, an attachment, or a load of the skill.
///
/// Read fresh from the view, never kept: a compaction that drops the text drops
/// the name with it, and the next request sends it again.
pub(super) fn names(session: &Session) -> HashSet<&str> {
    let mut names = HashSet::new();
    let mut calls = HashMap::new();
    for seen in session.view() {
        let Seen::As(entry) = seen else { continue };
        match entry {
            Entry::Ask { ask, .. } => names.extend(skills::included(&ask.context)),
            Entry::Context { context, .. } => {
                names.extend(skills::source_name(&context.source));
            }
            Entry::Answer { .. } => {
                calls.clear();
                calls.extend(entry.tool_calls().filter_map(|call| {
                    Load::instruction_name(call).map(|name| (call.id.as_str(), name))
                }));
            }
            Entry::Tool { result, .. } if result.name == Load::NAME => {
                names.extend(
                    calls
                        .remove(result.call.as_str())
                        .filter(|_| !result.is_error),
                );
            }
            _ => {}
        }
    }
    names
}
