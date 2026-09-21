//! The gate the loop asks before every tool call, decided by tier.

use crate::seams::{Approver, Decision};
use serde_json::Value;
use tools::Tier;

/// Allows every tier this ceiling reaches. Not a comparison: `Tier` is a
/// lattice, and `write` and `net` sit beside each other rather than in order.
#[derive(Debug, Clone, Copy)]
pub struct Ceiling(pub Tier);

impl Approver for Ceiling {
    fn approve(&self, name: &str, tier: Tier, _args: &Value) -> Decision {
        if tier.under(self.0) {
            Decision::Allow
        } else {
            Decision::Deny(format!(
                "`{name}` needs {tier:?} access; this run is capped at {:?}. \
                 Use a tool within the cap or tell the user what you need.",
                self.0
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Ceiling;
    use crate::seams::{Approver, Decision};
    use serde_json::json;
    use tools::Tier;

    fn allowed(ceiling: Tier, tier: Tier) -> bool {
        matches!(
            Ceiling(ceiling).approve("t", tier, &json!({})),
            Decision::Allow
        )
    }

    #[test]
    fn a_read_only_run_does_not_silently_gain_the_web() {
        assert!(!allowed(Tier::Read, Tier::Net));
        assert!(!allowed(Tier::Write, Tier::Net));
        assert!(allowed(Tier::Net, Tier::Net));
        // `sh` can `curl`, so there is nothing left to refuse here.
        assert!(allowed(Tier::Exec, Tier::Net));
    }

    #[test]
    fn the_web_alone_is_not_a_licence_to_write_or_run() {
        assert!(allowed(Tier::Net, Tier::Read));
        assert!(!allowed(Tier::Net, Tier::Write));
        assert!(!allowed(Tier::Net, Tier::Exec));
    }

    #[test]
    fn a_denial_names_what_was_wanted_and_what_the_run_has() {
        let Decision::Deny(why) = Ceiling(Tier::Read).approve("fetch", Tier::Net, &json!({}))
        else {
            panic!("allowed");
        };
        assert!(why.contains("fetch"), "{why}");
        assert!(why.contains("Net") && why.contains("Read"), "{why}");
    }
}
