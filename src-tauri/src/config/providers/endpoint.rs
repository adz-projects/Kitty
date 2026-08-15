//! Turning what a user typed into a URL worth trying, and turning what went
//! wrong into something worth reading.
//!
//! Both halves exist because of the same real failure: typing
//! `192.168.1.199:8081` into the wizard's Base URL field produced
//! **"could not reach 192.168.1.199:8081: builder error"**. Two problems in
//! one message — a bare `host:port` is not a URL reqwest can build a request
//! from, and "builder error" is reqwest's internal phrasing for that, which
//! tells the user nothing about what to do.

/// Candidate URLs to try, in order, for what the user typed.
///
/// A scheme the user supplied is always respected — if they wrote `http://`
/// they get exactly that, and no silent upgrade. Only a *schemeless* entry
/// fans out, `https` first: the secure one should win when both answer, and
/// a plain-HTTP box on a LAN is the common case that makes the fallback
/// necessary at all.
///
/// Returns empty for input with nothing to dial, so callers can report "no
/// address" rather than probing garbage.
pub fn candidates(input: &str) -> Vec<String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.contains("://") {
        return vec![trimmed.to_string()];
    }
    vec![format!("https://{trimmed}"), format!("http://{trimmed}")]
}

/// A reqwest failure, said in words a user can act on.
///
/// reqwest's own `Display` is written for developers: "builder error",
/// "error sending request for url (...)". Its *kind* predicates carry the
/// information that actually distinguishes these cases, so this maps kind to
/// cause rather than trying to pattern-match the message text.
pub fn humanize(err: &reqwest::Error) -> String {
    if err.is_builder() {
        // Reached when the URL itself couldn't be parsed. By the time we get
        // here `candidates` has already added a scheme, so this is a genuinely
        // malformed address rather than the common schemeless case.
        return "that doesn't look like a valid address".to_string();
    }
    if err.is_timeout() {
        return "it didn't respond in time".to_string();
    }
    if err.is_connect() {
        return "nothing is listening there".to_string();
    }
    if err.is_redirect() {
        return "it redirected too many times".to_string();
    }
    if err.is_decode() {
        return "it answered with something unexpected".to_string();
    }
    // Anything left is worth showing verbatim rather than flattening into a
    // vague catch-all — an unclassified error the user can paste into a
    // search beats "something went wrong".
    err.to_string()
}

/// A failed probe across every candidate, as one sentence.
///
/// Names the address actually tried rather than the raw input, so a user who
/// typed `host:port` can see that `https://host:port` was attempted — which
/// is usually the thing that explains the result.
pub fn unreachable_message(attempts: &[(String, String)]) -> String {
    match attempts {
        [] => "no address to connect to".to_string(),
        [(url, why)] => format!("could not reach {url} — {why}"),
        many => {
            let tried = many
                .iter()
                .map(|(url, why)| format!("{url} ({why})"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("could not reach it — tried {tried}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_schemeless_host_tries_https_then_http() {
        assert_eq!(
            candidates("192.168.1.199:8081"),
            vec![
                "https://192.168.1.199:8081".to_string(),
                "http://192.168.1.199:8081".to_string()
            ]
        );
    }

    /// An explicit scheme is a decision, not a hint. Upgrading a typed
    /// `http://` to `https://` would silently ignore what the user asked
    /// for, and on a LAN box without TLS it would fail where their own
    /// choice would have worked.
    #[test]
    fn an_explicit_scheme_is_respected_and_never_upgraded() {
        assert_eq!(
            candidates("http://192.168.1.199:8081"),
            vec!["http://192.168.1.199:8081".to_string()]
        );
        assert_eq!(
            candidates("https://api.example.com"),
            vec!["https://api.example.com".to_string()]
        );
    }

    #[test]
    fn trailing_slashes_and_padding_are_trimmed() {
        assert_eq!(
            candidates("  https://api.example.com/  "),
            vec!["https://api.example.com".to_string()]
        );
    }

    #[test]
    fn nothing_to_dial_yields_no_candidates() {
        assert!(candidates("").is_empty());
        assert!(candidates("   ").is_empty());
        assert!(candidates("/").is_empty());
    }

    /// A path is part of the address for OpenAI-compatible endpoints
    /// (`.../v1`), so it must survive the scheme fan-out.
    #[test]
    fn a_path_survives_the_scheme_fanout() {
        assert_eq!(
            candidates("myhost:8080/v1"),
            vec![
                "https://myhost:8080/v1".to_string(),
                "http://myhost:8080/v1".to_string()
            ]
        );
    }

    #[test]
    fn one_failed_attempt_names_the_url_actually_tried() {
        let msg = unreachable_message(&[(
            "https://192.168.1.199:8081".to_string(),
            "nothing is listening there".to_string(),
        )]);
        assert_eq!(
            msg,
            "could not reach https://192.168.1.199:8081 — nothing is listening there"
        );
    }

    /// After a fan-out the user needs to see that *both* were tried, or
    /// "could not reach it" looks like we only attempted the one they typed.
    #[test]
    fn both_attempts_are_reported_after_a_scheme_fanout() {
        let msg = unreachable_message(&[
            ("https://h:1".to_string(), "nothing is listening there".to_string()),
            ("http://h:1".to_string(), "it didn't respond in time".to_string()),
        ]);
        assert!(msg.contains("https://h:1"), "{msg}");
        assert!(msg.contains("http://h:1"), "{msg}");
    }

    #[test]
    fn no_candidates_says_so_rather_than_naming_nothing() {
        assert_eq!(unreachable_message(&[]), "no address to connect to");
    }

    /// The message that started this: reqwest reports a malformed URL as
    /// "builder error", which is meaningless to a user.
    #[tokio::test]
    async fn a_malformed_url_is_not_reported_as_builder_error() {
        let err = crate::util::http_client()
            .get("not a url at all")
            .send()
            .await
            .expect_err("that should not have been a valid request");
        let msg = humanize(&err);
        assert!(!msg.contains("builder"), "still leaking jargon: {msg}");
        assert_eq!(msg, "that doesn't look like a valid address");
    }
}
