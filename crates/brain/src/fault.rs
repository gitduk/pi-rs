use crate::error::BrainError;

/// What to do about a failed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    // Load, throttling, or transport trouble. Worth another attempt.
    Transient,
    // The request did not fit the model's window. Retrying it unchanged will
    // fail identically; it has to get smaller first.
    Overflow,
    // Retrying changes nothing.
    Permanent,
}

/// Classify a failed request by its HTTP status, never by message wording.
///
/// Message text is not consulted: providers disagree about what a quota error
/// is called, and retry policy should not depend on an empirical word list.
pub fn classify(err: &BrainError) -> Fault {
    match err {
        // 408 is a timeout, 409 a collision, 425 an early hint; 429 a
        // throttle; 5xx (and 522/524/529 from CDNs) load. All are worth
        // another attempt.
        BrainError::Api { status, .. }
            if matches!(
                status,
                408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 522 | 524 | 529
            ) =>
        {
            Fault::Transient
        }
        // 413 is a size refusal, whatever the body says about it.
        BrainError::Api { status: 413, .. } => Fault::Overflow,
        BrainError::Api { .. } => Fault::Permanent,
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
    fn a_throttle_status_is_retried_whatever_the_body_says() {
        assert_eq!(classify(&api(429, "rate limit exceeded")), Fault::Transient);
        assert_eq!(classify(&api(529, "overloaded_error")), Fault::Transient);
        assert_eq!(classify(&api(503, "")), Fault::Transient);
        // The message is not consulted: a spent quota wearing a 429 is still
        // retried, because only the status is reliable across providers.
        assert_eq!(
            classify(&api(429, r#"{"error":{"code":"insufficient_quota"}}"#)),
            Fault::Transient
        );
        assert_eq!(
            classify(&api(429, "Monthly usage limit reached")),
            Fault::Transient
        );
        assert_eq!(
            classify(&api(429, "Your credit balance is too low")),
            Fault::Transient
        );
    }

    #[test]
    fn a_status_outside_the_retry_set_is_not_retried() {
        assert_eq!(
            classify(&api(400, "Your credit balance is too low")),
            Fault::Permanent
        );
    }

    #[test]
    fn a_429_is_retried_even_when_the_body_says_overflow() {
        assert_eq!(classify(&api(429, "prompt is too long")), Fault::Transient);
    }

    #[test]
    fn an_overflow_body_without_the_413_status_is_not_retried() {
        for body in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "Your input exceeds the context window of this model",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            "Please reduce the length of the messages or completion",
            "invalid params, context window exceeds limit",
        ] {
            assert_eq!(classify(&api(400, body)), Fault::Permanent, "{body}");
        }
    }

    #[test]
    fn overflow_is_recognized_by_status_alone() {
        assert_eq!(classify(&api(413, "no body")), Fault::Overflow);
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
