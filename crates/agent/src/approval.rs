use serde_json::Value;
use tools::Tier;

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Allow,
    /// The model reads this and can pick another route; a denial is a result,
    /// not the end of the turn.
    Deny(String),
}

/// Gate consulted before every call. Implementations may prompt, consult a
/// policy file, or decide statically.
pub trait Approver: Send + Sync {
    fn approve(&self, name: &str, tier: Tier, args: &Value) -> Decision;
}

/// Allows everything at or below `ceiling`.
#[derive(Debug, Clone, Copy)]
pub struct Ceiling(pub Tier);

impl Approver for Ceiling {
    fn approve(&self, name: &str, tier: Tier, _args: &Value) -> Decision {
        if tier <= self.0 {
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
