//! Which Wysebee Cloud this app talks to.
//!
//! The one saved from the window (its Server tab), else the default: `WYSEBEE_CLOUD_URL`
//! when set (the shell reads it and hands it in), else [`DEFAULT_URL`]. **An address must be https, or plain http to
//! this computer** ([`secure`]): the link's secret and every call's values and output go
//! there.

pub const DEFAULT_URL: &str = "https://app.wysebee.com";
/// The environment variable the shell reads.
pub const ENV: &str = "WYSEBEE_CLOUD_URL";

/// Whether `url` is https, or plain http to loopback (a cloud run on this computer).
pub fn secure(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    let (rest, tls) = match (lower.strip_prefix("https://"), lower.strip_prefix("http://")) {
        (Some(rest), _) => (rest, true),
        (_, Some(rest)) => (rest, false),
        _ => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Userinfo (`user@host`) would let `http://localhost@evil.example` pass as loopback.
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    if tls {
        return true;
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

/// An address as typed, ready to use: trimmed, without a trailing `/`. Refused with a
/// sentence when it is not one, or not [`secure`].
pub fn checked(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) || url.contains(['?', '#']) {
        return Err("That is not an address.".into());
    }
    if !secure(url) {
        return Err("The address must start with https:// (plain http:// only for this computer, like http://localhost:8000).".into());
    }
    Ok(url.to_string())
}

/// Where an address came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Default,
    Saved,
    Env,
}

/// The default when nothing is saved: the environment's when it passes the rule, else
/// [`DEFAULT_URL`].
pub fn default_url(env: Option<&str>) -> (String, Source) {
    match env.filter(|v| !v.trim().is_empty()).map(checked) {
        Some(Ok(url)) => (url, Source::Env),
        Some(Err(_)) => {
            tracing::warn!(source = ?Source::Env, "cloud_url_refused_insecure");
            (DEFAULT_URL.to_string(), Source::Default)
        }
        None => (DEFAULT_URL.to_string(), Source::Default),
    }
}

/// The cloud to use: the saved one when it passes the rule, else the default
/// ([`default_url`]). One that does not pass is never used.
pub fn choose(env: Option<&str>, saved: Option<&str>) -> (String, Source) {
    match saved.filter(|v| !v.trim().is_empty()).map(checked) {
        Some(Ok(url)) => (url, Source::Saved),
        Some(Err(_)) => {
            tracing::warn!(source = ?Source::Saved, "cloud_url_refused_insecure");
            default_url(env)
        }
        None => default_url(env),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_https_or_http_to_this_computer() {
        for ok in ["https://app.wysebee.com", "HTTPS://Example.com/x", "http://localhost:8000", "http://127.0.0.1", "http://[::1]:8000/api", " http://LOCALHOST "] {
            assert!(secure(ok), "{ok}");
        }
        for bad in ["http://example.com", "http://localhost@evil.example", "http://localhost.evil.example", "http://192.168.1.5:8000", "ftp://localhost", "localhost:8000", "", "https://", "http://", "https://user@host", "http://[::2]"] {
            assert!(!secure(bad), "{bad}");
        }
    }

    #[test]
    fn a_typed_address_is_tidied_or_refused() {
        assert_eq!(checked(" http://localhost:8000/ ").unwrap(), "http://localhost:8000");
        assert_eq!(checked("https://staging.example.com//").unwrap(), "https://staging.example.com");
        assert!(checked("http://example.com").unwrap_err().contains("https://"));
        assert_eq!(checked("https://a b").unwrap_err(), "That is not an address.");
        assert_eq!(checked("https://a.example/?x=1").unwrap_err(), "That is not an address.");
    }

    #[test]
    fn the_saved_one_wins_then_the_environment_then_the_default() {
        assert_eq!(choose(None, None), (DEFAULT_URL.to_string(), Source::Default));
        assert_eq!(choose(Some("  "), Some("")), (DEFAULT_URL.to_string(), Source::Default));
        assert_eq!(choose(None, Some("http://localhost:8000/")), ("http://localhost:8000".to_string(), Source::Saved));
        assert_eq!(choose(Some("https://dev.example"), Some("http://localhost:8000")), ("http://localhost:8000".to_string(), Source::Saved));
        assert_eq!(choose(Some("https://dev.example/"), None), ("https://dev.example".to_string(), Source::Env));
        // One that does not pass the rule is never used.
        assert_eq!(choose(Some("http://localhost:1"), Some("http://evil.example")), ("http://localhost:1".to_string(), Source::Env));
        assert_eq!(choose(Some("http://also.evil"), Some("http://evil.example")), (DEFAULT_URL.to_string(), Source::Default));
        assert_eq!(default_url(Some("http://evil.example")), (DEFAULT_URL.to_string(), Source::Default));
    }
}
