//! Deriving the git-over-HTTPS URL from an API base URL, with credentials.
//!
//! Kept apart from the adapters because it is the one place a token gets
//! written into a string that is later handed to `git`. Everything here treats
//! the result as a secret.

/// Strips a known API path suffix to get back to the site root.
pub fn web_root(api_base: &str) -> String {
    let base = api_base.trim_end_matches('/');
    for suffix in ["/api/v1", "/api/v3", "/api/v4"] {
        if let Some(root) = base.strip_suffix(suffix) {
            return root.to_string();
        }
    }
    // api.github.com is the one host where the API lives on its own subdomain.
    if let Some(rest) = base.strip_prefix("https://api.github.com") {
        let _ = rest;
        return "https://github.com".to_string();
    }
    if let Some(rest) = base.strip_prefix("http://api.github.com") {
        let _ = rest;
        return "http://github.com".to_string();
    }
    base.to_string()
}

/// Builds `https://user:token@host/owner/repo.git`.
///
/// The return value is a secret: never log it and never put it in an event.
pub fn clone_url(api_base: &str, user: &str, token: &str, owner: &str, repo: &str) -> String {
    let root = web_root(api_base);
    let path = format!("{owner}/{repo}.git");

    if token.is_empty() {
        return format!("{root}/{path}");
    }

    let user = if user.is_empty() { "gitai" } else { user };
    match root.split_once("://") {
        Some((scheme, host_and_path)) => {
            format!(
                "{scheme}://{}:{}@{host_and_path}/{path}",
                pct(user),
                pct(token)
            )
        }
        // No scheme is a misconfiguration; return something harmless rather
        // than a URL with a token glued into an unexpected position.
        None => format!("{root}/{path}"),
    }
}

/// Percent-encodes userinfo. Tokens containing `@` or `:` would otherwise
/// break the authority section apart.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Replaces the userinfo with `***`, for logging.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    match rest.split_once('@') {
        Some((_creds, host)) => format!("{scheme}://***@{host}"),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitea_api_base_collapses_to_the_site_root() {
        assert_eq!(
            web_root("https://git.example.com/api/v1"),
            "https://git.example.com"
        );
        assert_eq!(
            web_root("https://git.example.com/api/v1/"),
            "https://git.example.com"
        );
    }

    #[test]
    fn github_dot_com_and_enterprise_both_work() {
        assert_eq!(web_root("https://api.github.com"), "https://github.com");
        assert_eq!(web_root("https://ghe.corp/api/v3"), "https://ghe.corp");
    }

    #[test]
    fn credentials_land_in_the_authority_section() {
        let url = clone_url(
            "https://git.example.com/api/v1",
            "gitai",
            "t0ken",
            "acme",
            "widgets",
        );
        assert_eq!(url, "https://gitai:t0ken@git.example.com/acme/widgets.git");
    }

    #[test]
    fn awkward_token_characters_are_encoded() {
        let url = clone_url(
            "https://api.github.com",
            "x-access-token",
            "gh:p@ss",
            "a",
            "b",
        );
        assert_eq!(url, "https://x-access-token:gh%3Ap%40ss@github.com/a/b.git");
        assert!(!url.contains("p@ss"));
    }

    #[test]
    fn no_token_means_no_userinfo() {
        let url = clone_url(
            "https://git.example.com/api/v1",
            "gitai",
            "",
            "acme",
            "widgets",
        );
        assert_eq!(url, "https://git.example.com/acme/widgets.git");
    }

    #[test]
    fn redaction_hides_the_token() {
        let url = clone_url(
            "https://git.example.com/api/v1",
            "gitai",
            "t0ken",
            "acme",
            "widgets",
        );
        assert_eq!(redact(&url), "https://***@git.example.com/acme/widgets.git");
        assert_eq!(
            redact("https://git.example.com/a/b.git"),
            "https://git.example.com/a/b.git"
        );
    }
}
