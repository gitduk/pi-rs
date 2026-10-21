//! What a run of calls has cost, per turn and in total.

use crate::stream::Usage;

/// Usage and its price, accumulated across turns.
///
/// Usage figures are the provider's own, verbatim: a field the host left out
/// reads as zero — never presented as "not reported", just a real zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Totals {
    pub usage: Usage,
    pub cost: f64,
}

impl Totals {
    pub fn add(&mut self, usage: &Usage, cost: f64) {
        self.usage.add(usage);
        self.cost += cost;
    }
}
