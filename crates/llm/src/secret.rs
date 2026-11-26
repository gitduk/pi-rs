//! What reads as a secret in text that is about to leave the machine.
//!
//! A value is taken when it is shaped like a credential — a known prefix, a
//! key block, a bare digest, a login inside a URL — or when a name beside it
//! says it is one. The two are or-ed: shape catches what a name would miss,
//! and a name catches the short values a shape cannot tell from prose.

use std::collections::HashSet;

/// A secret found in text: the name its context gave it, if any, and the
/// value itself.
pub type Found = (Option<String>, String);

type Match<'a> = (Option<&'a str>, &'a str);

/// The distinct credentials found in `text`, with the first match for each value.
pub fn secrets_in(text: &str) -> Vec<Found> {
    let mut found = Vec::new();
    url_credentials(text, &mut found);
    named(text, &mut found);
    shaped(text, &mut found);

    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|(_, value)| seen.insert(*value))
        .map(|(name, value)| (name.map(str::to_owned), value.to_owned()))
        .collect()
}

// A word in a name that says the value beside it is a credential.
const SECRET_WORDS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "PASS",
    "AUTHORIZATION",
    "AUTH",
    "CREDENTIAL",
    "CREDENTIALS",
    "PWD",
];

// A word that turns a name away: the value beside it locates a secret, it is
// not one.
const NOT_SECRET_WORDS: &[&str] = &[
    "PATH", "FILE", "DIR", "FOLDER", "URL", "URI", "HOST", "PORT", "USER", "NAME", "MODEL", "ID",
];

// A digest is a digest: only its length says which function made it.
const LONGEST_DIGEST: usize = 128;
const DIGEST_LENGTHS: &[usize] = &[32, 40, 64, LONGEST_DIGEST];

fn url_credentials<'a>(mut text: &'a str, out: &mut Vec<Match<'a>>) {
    while let Some((before, after)) = text.split_once("://") {
        text = after;
        if !before
            .as_bytes()
            .last_chunk::<2>()
            .is_some_and(|tail| tail.iter().copied().all(is_scheme_byte))
        {
            continue;
        }
        let end = after
            .find(|c: char| {
                c.is_whitespace()
                    || matches!(c, '"' | '\'' | '`' | '<' | '>' | ')' | ']' | '}' | ',')
            })
            .unwrap_or(after.len());
        credentials_of_url(&after[..end], out);
        text = &after[end..];
    }
}

// The login, and any credential a query or fragment names, but not the host:
// the model still has to read the connection it is looking at.
fn credentials_of_url<'a>(after: &'a str, out: &mut Vec<Match<'a>>) {
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let authority = &after[..authority_end];
    // A password may hold an `@`; the last one is the host's.
    if let Some((login, _)) = authority.rsplit_once('@') {
        let secret = login
            .split_once(':')
            .map_or(login, |(_, password)| password);
        if plausible(secret) {
            out.push((None, secret));
        }
    }
    if let Some(at) = after.find(['?', '#']) {
        for pair in after[at + 1..].split('&') {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            if names_a_credential(name) && plausible(value) {
                out.push((None, value));
            }
        }
    }
}

fn named<'a>(text: &'a str, out: &mut Vec<Match<'a>>) {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if !is_name_start(bytes[at]) || (at > 0 && is_name_byte(bytes[at - 1])) {
            at += 1;
            continue;
        }
        let mut end = at;
        while end < bytes.len() && is_name_byte(bytes[end]) {
            end += 1;
        }
        let name = &text[at..end];
        if let Some(open) = after_name(bytes, end) {
            let (value, stop) = read_named_value(text, name, open);
            if !value.is_empty() && names_a_credential(name) && acceptable(value) {
                out.push((Some(name), value));
            }
            at = stop;
            continue;
        }
        at = end;
    }
}

// The byte where the value starts, when the name is followed by `=` or `:`.
fn after_name(bytes: &[u8], mut at: usize) -> Option<usize> {
    if at < bytes.len() && matches!(bytes[at], b'"' | b'\'') {
        at += 1;
    }
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t') {
        at += 1;
    }
    matches!(bytes.get(at), Some(byte) if *byte == b'=' || *byte == b':')
        .then(|| {
            at += 1;
            while at < bytes.len() && matches!(bytes[at], b' ' | b'\t') {
                at += 1;
            }
            at
        })
        .filter(|at| *at < bytes.len())
}

fn read_named_value<'a>(text: &'a str, name: &str, at: usize) -> (&'a str, usize) {
    let (value, stop) = read_value(text, at);
    if !(name.eq_ignore_ascii_case("authorization")
        || name.eq_ignore_ascii_case("proxy-authorization"))
        || matches!(text.as_bytes()[at], b'"' | b'\'')
        || !text[stop..].starts_with([' ', '\t'])
    {
        return (value, stop);
    }
    // Digest uses named parameters. Let them be scanned separately.
    if value.eq_ignore_ascii_case("digest") {
        return ("", stop);
    }
    if value.eq_ignore_ascii_case("bearer") || value.eq_ignore_ascii_case("basic") {
        let payload = text[stop..].trim_start_matches([' ', '\t']);
        return read_value(text, text.len() - payload.len());
    }
    (value, stop)
}

fn read_value(text: &str, at: usize) -> (&str, usize) {
    let rest = &text[at..];
    if let Some(&quote @ (b'"' | b'\'')) = rest.as_bytes().first() {
        let content = &rest[1..];
        if let Some(end) = content.find([char::from(quote), '\n'])
            && content.as_bytes()[end] == quote
        {
            return (&content[..end], at + end + 2);
        }
        return ("", at);
    }
    let end = rest.bytes().position(is_value_end).unwrap_or(rest.len());
    (&rest[..end], at + end)
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'[' | b']')
}

fn is_value_end(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b','
            | b';'
            | b'#'
            | b'"'
            | b'\''
            | b')'
            | b']'
            | b'}'
            | b'>'
            | b'<'
            | b'\\'
            | b'|'
            | b'&'
            | b'`'
            | b'{'
    )
}

fn names_a_credential(name: &str) -> bool {
    let mut matched = false;
    for word in name.split(['_', '.', '-', ' ', '[', ']']) {
        if NOT_SECRET_WORDS
            .iter()
            .any(|name| word.eq_ignore_ascii_case(name))
        {
            return false;
        }
        matched = matched
            || SECRET_WORDS
                .iter()
                .any(|name| word.eq_ignore_ascii_case(name));
    }
    matched
}

// Whether a named value is specific enough to be a credential rather than a
// reference or a number that happens to sit beside a key name.
fn acceptable(value: &str) -> bool {
    if is_url(value) || value.starts_with('$') {
        return false;
    }
    let length = value.chars().count();
    if !(6..=4096).contains(&length) {
        return false;
    }
    if value.chars().any(|c| {
        c.is_whitespace()
            || matches!(
                c,
                '{' | '}'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '['
                    | ']'
                    | '&'
                    | '|'
                    | '?'
                    | ';'
                    | ','
                    | '`'
                    | '\\'
                    | '"'
                    | '\''
            )
    }) {
        return false;
    }
    // A dotted identifier is a reference (`process.env.API_KEY`), not a value.
    if value.split('.').all(|part| {
        !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    }) && value.contains('.')
    {
        return false;
    }
    // A number is a setting, not a credential: 3600 says how long, not what.
    value.bytes().any(|b| b.is_ascii_alphabetic())
}

fn plausible(value: &str) -> bool {
    let length = value.chars().count();
    (4..=4096).contains(&length) && value.bytes().any(|b| b.is_ascii_alphabetic())
}

fn is_url(value: &str) -> bool {
    let length = value
        .bytes()
        .take_while(|&byte| is_scheme_byte(byte))
        .count();
    length >= 2 && value[length..].starts_with("://")
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
}

fn shaped<'a>(text: &'a str, out: &mut Vec<Match<'a>>) {
    let mut at = 0;
    let mut base64_end = 0;
    while at < text.len() {
        if let Some((value, end)) = find_shaped(text, at, &mut base64_end) {
            out.push((None, value));
            at = end;
            continue;
        }
        at += 1;
    }
}

// A known prefix takes precedence over a generic digest.
fn find_shaped<'a>(text: &'a str, at: usize, base64_end: &mut usize) -> Option<(&'a str, usize)> {
    let rest = text.get(at..)?;
    for (prefix, least) in [
        ("sk-", 16),
        ("github_pat_", 20),
        ("ghp_", 20),
        ("gho_", 20),
        ("ghu_", 20),
        ("ghs_", 20),
        ("ghr_", 20),
        ("xoxb-", 10),
        ("xoxa-", 10),
        ("xoxp-", 10),
        ("xoxr-", 10),
        ("xoxs-", 10),
        ("AIza", 30),
        ("glpat-", 20),
        ("npm_", 30),
        ("pypi-", 20),
        ("hf_", 30),
        ("dckr_pat_", 20),
        ("shpat_", 20),
        ("sk_live_", 20),
        ("rk_live_", 20),
        ("pk_live_", 20),
        ("apikey_", 24),
    ] {
        if let Some(body) = rest.strip_prefix(prefix) {
            let length = body.bytes().take_while(|byte| is_token_byte(*byte)).count();
            if length >= least {
                let end = at + prefix.len() + length;
                return Some((&text[at..end], end));
            }
        }
    }
    if let Some(body) = rest.strip_prefix("Bearer ") {
        let length = body.bytes().take_while(|byte| is_token_byte(*byte)).count();
        if length >= 16 {
            return Some((&text[at + 7..at + 7 + length], at + 7 + length));
        }
    }
    if rest.starts_with("eyJ") {
        let length = rest.bytes().take_while(|byte| is_token_byte(*byte)).count();
        let token = &text[at..at + length];
        if length >= 40 && token.matches('.').count() >= 2 {
            return Some((token, at + length));
        }
    }
    if rest.starts_with("AKIA") || rest.starts_with("ASIA") {
        let length = rest
            .bytes()
            .take(21)
            .take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            .count();
        if length == 20 {
            return Some((&text[at..at + length], at + length));
        }
    }
    if rest.as_bytes()[0].is_ascii_digit() {
        let digits = rest
            .bytes()
            .take(11)
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if (8..=10).contains(&digits)
            && let Some(body) = rest[digits..].strip_prefix(':')
        {
            let length = body.bytes().take_while(|byte| is_token_byte(*byte)).count();
            if body.starts_with("AA") && length >= 30 {
                let end = at + digits + 1 + length;
                return Some((&text[at..end], end));
            }
        }
    }
    if rest.starts_with("-----BEGIN ") {
        let end = rest.find(" PRIVATE KEY-----")?;
        let header_end = end + " PRIVATE KEY-----".len();
        let end_start = header_end + rest[header_end..].find("-----END ")?;
        let close = rest[end_start + 5..].find("-----")?;
        let stop = end_start + 5 + close + 5;
        return Some((&text[at..at + stop], at + stop));
    }
    digest(rest, at, base64_end).map(|length| (&text[at..at + length], at + length))
}

// A bare digest is a credential or a check value, and nothing on the page
// tells them apart: both are taken.
fn digest(rest: &str, at: usize, base64_end: &mut usize) -> Option<usize> {
    let length = rest
        .bytes()
        .take(LONGEST_DIGEST + 1)
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if DIGEST_LENGTHS.contains(&length)
        && rest
            .as_bytes()
            .get(length)
            .is_none_or(|byte| !is_word_byte(*byte))
    {
        return Some(length);
    }
    if at < *base64_end || rest.len() < 40 {
        return None;
    }
    // `=` only pads the end: an `=` in the middle ends the run, or a whole
    // `NAME=value` line would read as one base64 value.
    let mut length = 0;
    let mut padding = false;
    for byte in rest.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_') {
            if padding {
                break;
            }
            length += 1;
        } else if byte == b'=' {
            padding = true;
            length += 1;
        } else {
            break;
        }
    }
    // Every suffix of this run ends at the same byte and can only lose the
    // characters required by this rule. Hex and named prefixes still get checked.
    *base64_end = at + length;
    let body = &rest[..length];
    let mixed = body.bytes().any(|b| b.is_ascii_uppercase())
        && body.bytes().any(|b| b.is_ascii_lowercase())
        && body.bytes().any(|b| b.is_ascii_digit());
    (length >= 40
        && mixed
        && !body.bytes().all(|b| b.is_ascii_hexdigit())
        && rest
            .as_bytes()
            .get(length)
            .is_none_or(|byte| !is_word_byte(*byte)))
    .then_some(length)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+' | b'/' | b'=' | b'~')
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(text: &str) -> Vec<String> {
        let found = secrets_in(text);
        found.iter().map(|(_, value)| value.clone()).collect()
    }

    fn check(text: &str, expected: &[&str]) {
        let found = values(text);
        assert_eq!(found, expected, "{text}");
    }

    // A name that says what the value is hands it over, even when the value
    // has no shape at all.
    #[test]
    fn a_name_that_says_what_it_is_hands_over_its_value() {
        check(
            "OPENAI_API_KEY=sk-abc123def456ghi789",
            &["sk-abc123def456ghi789"],
        );
        check(
            r#"{"api_key": "133e2076-3868-4282-87fa-2cf6aed9295b"}"#,
            &["133e2076-3868-4282-87fa-2cf6aed9295b"],
        );
        check("ES_PASS=lk4hCyPlL=gKM4xxf", &["lk4hCyPlL=gKM4xxf"]);
        check(
            "AUTH_JWT_SECRET: 9VWmwAa9QsOUY5EkhX",
            &["9VWmwAa9QsOUY5EkhX"],
        );
        check("PASSWD=hunter2", &["hunter2"]);
        check("Authorization: Bearer sk-...", &["sk-..."]);
        check("Proxy-Authorization: Basic dXNlcjpwYXNz", &["dXNlcjpwYXNz"]);
    }

    // A number beside a key name is a setting, not a credential.
    #[test]
    fn a_setting_is_not_a_credential() {
        for text in [
            "TOKEN_TTL=3600",
            "KEY_SIZE=32",
            "secret_retries=3",
            "api_key: EMPTY",
            "const key = process.env.API_KEY;",
            r#"api_key = "$BIFROST_API_KEY""#,
        ] {
            check(text, &[]);
        }
    }

    // A shape only a credential wears is caught on its own, wherever it is.
    #[test]
    fn a_shape_only_a_credential_wears_is_caught_without_a_name() {
        check(
            "use sk-proj-0123456789abcdefghij now",
            &["sk-proj-0123456789abcdefghij"],
        );
        check(
            "Authorization: Bearer abf1955a7eb7d0c9",
            &["abf1955a7eb7d0c9"],
        );
        check(
            "7854321098:AAHcdefghijklmnopqrstuvwxyz0123456",
            &["7854321098:AAHcdefghijklmnopqrstuvwxyz0123456"],
        );
        let block = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----";
        check(block, &[block]);
        check(
            "a3f1c2b4d5e6f708192a3b4c5d6e7f80",
            &["a3f1c2b4d5e6f708192a3b4c5d6e7f80"],
        );
        let long = "a".repeat(64);
        check(&long, &[long.as_str()]);
    }

    // An id keeps its dashes and a digest keeps its place: only the digest is
    // taken, so a conversation id still reads as one.
    #[test]
    fn an_identifier_is_not_a_digest() {
        check("conversation_id=d458f2ab-7f91-4849-ae7a-ea1b0bbb21c8", &[]);
        check("MODEL=deepseek-v4.1-flash", &[]);
        check("REDIS=localhost:6379", &[]);
        check(
            "REDIS=7f5ad2c8d9e0f1a2b3c4d5e6f7081920",
            &["7f5ad2c8d9e0f1a2b3c4d5e6f7081920"],
        );
    }

    // A URL keeps its shape: the host and port stay, the login does not.
    #[test]
    fn a_url_loses_only_its_login() {
        check("redis://:s3cret2024@cache:6379/0", &["s3cret2024"]);
        check(
            "postgres://app:hunter2@db:5432/x?password=another1",
            &["hunter2", "another1"],
        );
        check("https://api.example.com/v1?limit=1", &[]);
    }

    // The same value twice in one text is reported once.
    #[test]
    fn one_value_is_reported_once() {
        check(
            "TOKEN=sk-abc123def456ghi789 again sk-abc123def456ghi789",
            &["sk-abc123def456ghi789"],
        );
    }
}
