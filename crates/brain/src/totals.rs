//! What a run of calls has cost, per turn and in total.

use crate::stream::Usage;

/// Usage and its price, accumulated across turns.
///
/// The usage figures are the provider's own, verbatim: a field the host left
/// out reads as zero, and a surface that shows a count treats that as "not
/// reported", never as a real zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Totals {
    pub usage: Usage,
    pub cost: f64,
}

impl Totals {
    pub fn add(&mut self, usage: &Usage, cost: f64) {
        self.usage.input += usage.input;
        self.usage.output += usage.output;
        self.usage.cache_read += usage.cache_read;
        self.usage.cache_write += usage.cache_write;
        self.cost += cost;
    }

    /// Fold another run's totals in whole.
    pub fn merge(&mut self, other: &Self) {
        self.add(&other.usage, other.cost);
    }
}
