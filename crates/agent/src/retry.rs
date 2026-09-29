//! What to do about a request the provider could not serve right now.

/// Retry schedule for a request the provider could not serve right now.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempts: usize,
    pub base: std::time::Duration,
    pub max: std::time::Duration,
    /// No data for this long means the stream is wedged. Generous, because a
    /// reasoning model can legitimately think for minutes before its first
    /// token.
    pub idle: std::time::Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 9,
            base: std::time::Duration::from_millis(800),
            max: std::time::Duration::from_secs(30),
            idle: std::time::Duration::from_secs(300),
        }
    }
}

impl Retry {
    // Exponential, capped, with jitter so concurrent agents do not retry in
    // lockstep against a provider that is already struggling.
    pub(crate) fn delay(&self, attempt: usize) -> std::time::Duration {
        let grown = self.base.saturating_mul(1u32 << attempt.min(10));
        let capped = grown.min(self.max);
        // Nanos from the clock are a good enough jitter source for a backoff,
        // and cheaper than taking on a rng dependency.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0) as u64;
        let jitter = capped.as_millis() as u64 / 4;
        capped + std::time::Duration::from_millis(if jitter == 0 { 0 } else { nanos % jitter })
    }
}
