use crate::error::LlmError;

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

/// Classify a failed request by its HTTP status.
///
/// Wording decides only one thing: whether a refusal the status calls plain
/// bad (400) or a broken stream is the window overflowing, since the major
/// providers say so in a 400 or an error frame and never in a 413. Retry
/// policy itself never rests on it: providers disagree about what a quota
/// error is called.
pub fn classify(err: &LlmError) -> Fault {
    match err {
        // 408 timeout, 409 collision, 425 early hint, 429 throttle, 5xx (plus
        // 522/524/529 from CDNs) load — all worth another attempt.
        LlmError::Api {
            status: 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 522 | 524 | 529,
            ..
        } => Fault::Transient,
        // 413 is a size refusal, whatever the body says about it.
        LlmError::Api { status: 413, .. } => Fault::Overflow,
        LlmError::Api {
            status: 400, body, ..
        } if overflowing(body) => Fault::Overflow,
        LlmError::Api { .. } => Fault::Permanent,
        LlmError::Stream(detail) if overflowing(detail) => Fault::Overflow,
        // A dropped socket or a truncated stream is worth another attempt.
        LlmError::Http(_) | LlmError::Stream(_) => Fault::Transient,
        LlmError::Json(_) | LlmError::Config(_) | LlmError::Summarizer(_) => Fault::Permanent,
    }
}

// How the providers word a request that outgrew the window: Anthropic's
// `prompt is too long`, OpenAI's `context_length_exceeded` code and its
// `maximum context length` prose, and the hosts that copy one or the other.
fn overflowing(body: &str) -> bool {
    const SAID: [&str; 7] = [
        "prompt is too long",
        "context_length_exceeded",
        "maximum context length",
        "maximum prompt length",
        "exceeds the context window",
        "context window exceeds limit",
        "reduce the length of the messages",
    ];
    let body = body.to_ascii_lowercase();
    SAID.iter().any(|s| body.contains(s))
}

/// The window the provider says it has, parsed from an overflow message;
/// `None` if no usable number is there. Only the number the provider's own
/// wording ties to the limit counts: the same message also names what was
/// asked for, its parts, and ids, any of which may be smaller.
pub fn overflow_limit(err: &LlmError) -> Option<usize> {
    // Below this is a status code or a version, never a context window.
    const FLOOR: usize = 1_000;
    // How far past the phrase its number may sit: `is`, `of`, `(`.
    const NEAR: usize = 16;
    const BEFORE: [&str; 3] = [
        "maximum context length",
        "maximum prompt length",
        "context window of",
    ];

    let text = err.to_string().to_ascii_lowercase();
    let after = BEFORE.iter().find_map(|phrase| {
        let at = text.find(phrase)? + phrase.len();
        let near: String = text[at..].chars().take(NEAR).collect();
        let digits: String = near
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<usize>().ok()
    });
    // Anthropic's `… tokens > 200000 maximum`.
    let before = || {
        let head = text[..text.find(" maximum")?].trim_end();
        let digits = head.len() - head.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        head[head.len() - digits..].parse::<usize>().ok()
    };
    after.or_else(before).filter(|n| *n >= FLOOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16, body: &str) -> LlmError {
        LlmError::Api {
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
        assert_eq!(classify(&api(429, "prompt is too long")), Fault::Transient);
    }

    #[test]
    fn a_status_outside_the_retry_set_is_not_retried() {
        for (status, body) in [
            (400, "Your credit balance is too low"),
            (400, "tools.0.name: invalid"),
            (401, "invalid x-api-key"),
        ] {
            assert_eq!(classify(&api(status, body)), Fault::Permanent);
        }
        assert_eq!(
            classify(&LlmError::Config("no key".into())),
            Fault::Permanent
        );
    }

    // Shrunk and resent, not resent unchanged: the 400 the major providers
    // send for an overflow is never classed as passing trouble.
    #[test]
    fn an_overflow_body_on_a_400_is_an_overflow() {
        for body in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#,
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","code":"context_length_exceeded"}}"#,
            "Your input exceeds the context window of this model",
            "Input length (265330) exceeds model's maximum context length (262144).",
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            "Please reduce the length of the messages or completion",
            "invalid params, context window exceeds limit",
        ] {
            assert_eq!(classify(&api(400, body)), Fault::Overflow, "{body}");
        }
        assert_eq!(
            classify(&LlmError::Stream(
                r#"{"type":"error","code":"context_length_exceeded","message":"too long"}"#.into()
            )),
            Fault::Overflow
        );
        // Only on a 400: any other refusal keeps what its status says.
        assert_eq!(classify(&api(401, "prompt is too long")), Fault::Permanent);
    }

    #[test]
    fn overflow_is_recognized_by_status_alone() {
        assert_eq!(classify(&api(413, "no body")), Fault::Overflow);
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
            // The parts of the request are smaller than the window; an id
            // carries digits too. Neither is the limit.
            (
                "This model's maximum context length is 128000 tokens. However, you requested 132000 tokens (100000 in the messages, 32000 in the completion). Please reduce the length of the messages or completion.",
                128_000,
            ),
            (
                r#"{"type":"error","error":{"message":"prompt is too long: 213462 tokens > 200000 maximum"},"request_id":"req_011C2345"}"#,
                200_000,
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
            classify(&LlmError::Stream("connection reset".into())),
            Fault::Transient
        );
        assert_eq!(
            classify(&LlmError::Stream("idle for 300s".into())),
            Fault::Transient
        );
    }
}
