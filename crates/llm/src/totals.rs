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

    /// What was added after `base` was taken: one run's own share of a
    /// session's count, so one meter answers for both.
    pub fn since(&self, base: &Totals) -> Totals {
        let left = |now: u64, then: u64| now.saturating_sub(then);
        Totals {
            usage: Usage {
                input: left(self.usage.input, base.usage.input),
                output: left(self.usage.output, base.usage.output),
                cache_read: left(self.usage.cache_read, base.usage.cache_read),
                cache_write: left(self.usage.cache_write, base.usage.cache_write),
            },
            cost: (self.cost - base.cost).max(0.0),
        }
    }
}
