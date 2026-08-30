use crate::error::BrainError;

/// What to do about a failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Load, throttling, or transport trouble. Worth another attempt.
    Transient,
    /// The request did not fit the model's window. Retrying it unchanged will
    /// fail identically; it has to get smaller first.
    Overflow,
    /// Retrying changes nothing, and for a spent quota it also costs money.
    Permanent,
}

/// Account limits that arrive wearing a throttle's clothes. Checked first: a
/// spent quota is usually an HTTP 429, and treating it as one burns money on
/// retries that cannot succeed.
const SPENT: &[&str] = &[
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "quota exhausted",
    "billing",
    "credit balance is too low",
];

/// The window was exceeded. Wording differs per provider and none of it is
/// derivable, so the list is empirical — borrowed from pi-mono's `overflow.ts`.
const OVERFLOW: &[&str] = &[
    "prompt is too long",
    "request_too_large",
    "input is too long for requested model",
    "exceeds the context window",
    "maximum context length",
    "context length exceeded",
    "context_length_exceeded",
    "maximum prompt length",
    "exceeds the maximum number of tokens",
    "reduce the length of the messages",
    "input length",
    "prompt token count",
    "exceeded model token limit",
    "context window exceeds limit",
    "exceeds the available context size",
    "too large for model with",
    "prompt too long",
];

const TRANSIENT: &[&str] = &[
    "overloaded",
    "rate limit",
    "rate_limit",
    "ratelimit",
    "too many requests",
    "service unavailable",
    "service_unavailable",
    "server error",
    "internal error",
    "internal_server_error",
    "provider returned error",
    "network error",
    "connection error",
    "connection refused",
    "connection reset",
    "connection closed",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "timed out",
    "timeout",
    "eof while parsing",
    "stream ended",
];

const TRANSIENT_STATUS: &[u16] = &[408, 409, 425, 429, 500, 502, 503, 504, 522, 524, 529];

fn hit(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Classify a failed request.
///
/// Order is load-bearing: a spent quota and a throttle both arrive as 429, and
/// only the message text tells them apart.
pub fn classify(err: &BrainError) -> Fault {
    let text = err.to_string().to_lowercase();

    if hit(&text, SPENT) {
        return Fault::Permanent;
    }
    if hit(&text, OVERFLOW) {
        return Fault::Overflow;
    }

    match err {
        BrainError::Api { status, .. } if TRANSIENT_STATUS.contains(status) => Fault::Transient,
        // 413 is a size refusal, whatever the body says about it.
        BrainError::Api { status: 413, .. } => Fault::Overflow,
        BrainError::Api { .. } => {
            if hit(&text, TRANSIENT) {
                Fault::Transient
            } else {
                Fault::Permanent
            }
        }
        // A dropped socket or a truncated stream is worth another attempt.
        BrainError::Http(_) | BrainError::Stream(_) => Fault::Transient,
        BrainError::Json(_) | BrainError::Config(_) => Fault::Permanent,
    }
}

/// The window the provider says it has, read out of an overflow message.
///
/// Most of them carry the numbers — "prompt is too long: 213462 tokens >
/// 200000 maximum" — and the smaller of the two is always the limit. Reading it
/// beats guessing at a correction factor when our own estimate was wrong by an
/// unknown amount.
///
/// None when the message carries no usable number; the caller then falls back
/// to squeezing blindly.
pub fn overflow_limit(err: &BrainError) -> Option<usize> {
    // Below this is a status code or a version, never a context window.
    const FLOOR: usize = 1_000;

    err.to_string()
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|t| t.parse::<usize>().ok())
        .filter(|n| *n >= FLOOR)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, body: &str) -> BrainError {
        BrainError::Api {
            format: "anthropic",
            status,
            body: body.into(),
        }
    }

    #[test]
    fn a_plain_throttle_is_worth_retrying() {
        assert_eq!(classify(&api(429, "rate limit exceeded")), Fault::Transient);
        assert_eq!(classify(&api(529, "overloaded_error")), Fault::Transient);
        assert_eq!(classify(&api(503, "")), Fault::Transient);
    }

    #[test]
    fn a_spent_quota_wearing_a_429_is_not() {
        // The status is identical to a throttle; only the body separates them,
        // and retrying this one just spends money.
        assert_eq!(
            classify(&api(429, r#"{"code":"insufficient_quota"}"#)),
            Fault::Permanent
        );
        assert_eq!(
            classify(&api(429, "Monthly usage limit reached")),
            Fault::Permanent
        );
        assert_eq!(
            classify(&api(400, "Your credit balance is too low")),
            Fault::Permanent
        );
        // Seen from a routing proxy, typed `rate_limit_error` and coded
        // `rate_limit_exceeded`, and four retries over 34s changed nothing.
        assert_eq!(
            classify(&api(
                429,
                r#"{"error":{"message":"AI Chat quota exhausted","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#
            )),
            Fault::Permanent
        );
    }

    #[test]
    fn overflow_is_recognized_across_provider_wordings() {
        for body in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "Your input exceeds the context window of this model",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            "Please reduce the length of the messages or completion",
            "invalid params, context window exceeds limit",
        ] {
            assert_eq!(classify(&api(400, body)), Fault::Overflow, "{body}");
        }
        assert_eq!(classify(&api(413, "no body")), Fault::Overflow);
    }

    #[test]
    fn overflow_outranks_the_status_it_arrives_with() {
        // Anthropic sends 413 for a byte-size refusal and 400 for a token one;
        // neither is a throttle, however the transport reports it.
        assert_eq!(classify(&api(429, "prompt is too long")), Fault::Overflow);
    }

    #[test]
    fn an_ordinary_bad_request_is_not_retried() {
        assert_eq!(
            classify(&api(400, "tools.0.name: invalid")),
            Fault::Permanent
        );
        assert_eq!(classify(&api(401, "invalid x-api-key")), Fault::Permanent);
        assert_eq!(
            classify(&BrainError::Config("no key".into())),
            Fault::Permanent
        );
    }

    #[test]
    fn an_overflow_message_gives_up_the_window_it_names() {
        let cases = [
            (
                "prompt is too long: 213462 tokens > 200000 maximum",
                200_000,
            ),
            (
                "Input length (265330) exceeds model's maximum context length (262144).",
                262_144,
            ),
            (
                "Requested token count exceeds the model's maximum context length of 131072 tokens",
                131_072,
            ),
            (
                "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
                131_072,
            ),
        ];
        for (body, want) in cases {
            assert_eq!(overflow_limit(&api(400, body)), Some(want), "{body}");
        }
    }

    #[test]
    fn a_message_with_no_usable_number_reads_as_unknown() {
        // Status codes are not windows.
        assert_eq!(
            overflow_limit(&api(413, "400/413 status code (no body)")),
            None
        );
        assert_eq!(
            overflow_limit(&api(413, "Request exceeds the maximum size")),
            None
        );
    }

    #[test]
    fn a_broken_stream_is_worth_another_attempt() {
        assert_eq!(
            classify(&BrainError::Stream("connection reset".into())),
            Fault::Transient
        );
        assert_eq!(
            classify(&BrainError::Stream("idle for 300s".into())),
            Fault::Transient
        );
    }
}
